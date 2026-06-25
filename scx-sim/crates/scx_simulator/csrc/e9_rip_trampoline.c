/*
 * e9_rip_trampoline.c — e9patch call trampoline for RIP-targeted replay.
 *
 * Compiled with e9compile.sh. Patched onto specific instruction addresses
 * from a preemption trace to implement deterministic replay of --break-on
 * insn recordings using e9patch.
 *
 * The trampoline fires whenever execution reaches a patched instruction.
 * It checks whether this address is the current replay target (armed_rip)
 * and, if so, calls e9_replay_yield() via the yield function pointer.
 *
 * State sharing uses a FIXED mmap'd address (E9_RIP_SHARED_ADDR), separate
 * from the E9_SHARED_ADDR used by the Jcc branch-counting trampoline.
 *
 * Must be compiled from the e9patch directory so #include "stdlib.c" resolves:
 *   cd third_party/e9patch && ./e9compile.sh <path>/e9_rip_trampoline.c
 */

#include <stdint.h>

/* Need LIBDL for dlcall() which handles ABI alignment when calling
 * e9_preempt_yield (16-byte stack alignment, SSE state save/restore). */
#define LIBDL
#include "stdlib.c"

/* ------------------------------------------------------------------ */
/* Fixed shared address — must match E9_RIP_SHARED_ADDR in Rust.       */
/* Offset by one page (0x1000) from E9_SHARED_ADDR to avoid collision. */
/* ------------------------------------------------------------------ */

#define E9_RIP_SHARED_ADDR  ((volatile struct e9_rip_shared *)0x1E9001000ULL)

/* ------------------------------------------------------------------ */
/* Shared state layout (must match E9RipShared in e9patch.rs)          */
/* ------------------------------------------------------------------ */

struct e9_rip_shared {
	uint64_t armed_rip;   /* Target RIP (0 = disarmed) */
	void    *yield_fn;    /* e9_replay_yield function pointer */
};

/* ------------------------------------------------------------------ */
/* call_yield — wrapper that aligns the stack to 16 bytes before       */
/* calling the Rust yield function (System V ABI requirement).         */
/* The e9tool "clean" ABI does NOT guarantee stack alignment.          */
/*                                                                     */
/* Reuses the same approach as e9_rbc_trampoline.c.                    */
/* ------------------------------------------------------------------ */

asm (
".globl call_rip_yield\n"
".type  call_rip_yield, @function\n"
"call_rip_yield:\n"
"    push   %rbp\n"
"    mov    %rsp, %rbp\n"
"    and    $-16, %rsp\n"       /* align stack to 16 bytes */
"    call   *%rdi\n"            /* call yield_fn() — no args */
"    mov    %rbp, %rsp\n"
"    pop    %rbp\n"
"    ret\n"
".size  call_rip_yield, .-call_rip_yield\n"
);

/* call_rip_yield(yield_fn) -> int64_t (ignored — new counter value) */
extern int64_t call_rip_yield(void *yield_fn);

/* ------------------------------------------------------------------ */
/* rip_trampoline — called at each patched RIP by e9tool.              */
/*                                                                     */
/* Takes the instruction address as an argument (passed by e9tool via  */
/* 'rip_trampoline(addr)@binary'). Checks if this address matches the */
/* currently armed target RIP. If so, calls e9_replay_yield().         */
/*                                                                     */
/* Fast path: load armed_rip, compare with addr, return if mismatch.  */
/* All integer ops, safe under e9tool's "clean" ABI.                   */
/* ------------------------------------------------------------------ */

void rip_trampoline(const void *addr)
{
	volatile struct e9_rip_shared *s = E9_RIP_SHARED_ADDR;
	uint64_t armed = s->armed_rip;
	if (__builtin_expect(armed == 0, 1))
		return;
	if (__builtin_expect(armed != (uint64_t)addr, 1))
		return;
	void *yield_fn = (void *)s->yield_fn;
	if (__builtin_expect(yield_fn == 0, 0))
		return;
	/* Target RIP reached — yield via Rust. */
	call_rip_yield(yield_fn);
}
