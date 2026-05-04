// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

/*
 * e9_rbc_trampoline.c — e9patch call trampoline for software RBC counting.
 *
 * Compiled with e9compile.sh. Called at every instrumented Jcc instruction.
 *
 * State sharing uses a FIXED mmap'd address (E9_SHARED_ADDR). The Rust
 * backend mmaps E9SharedRbc at this address before loading the _e9.so.
 * The trampoline reads/writes through this hardcoded address — no RIP-
 * relative addressing, no dlsym, no relocations.
 *
 * Must be compiled from the e9patch directory so #include "stdlib.c" resolves:
 *   cd third_party/e9patch && ./e9compile.sh <path>/e9_rbc_trampoline.c
 */

#include <stdint.h>

/* Need LIBDL for dlcall() which handles ABI alignment when calling
 * e9_preempt_yield (16-byte stack alignment, SSE state save/restore). */
#define LIBDL
#include "stdlib.c"

/* ------------------------------------------------------------------ */
/* Fixed shared address — must match E9_SHARED_ADDR in Rust.           */
/* Chosen to be in an obscure region unlikely to collide with ASLR.    */
/* ------------------------------------------------------------------ */

#define E9_SHARED_ADDR  ((volatile struct e9_shared_rbc *)0x1E9000000ULL)

/* ------------------------------------------------------------------ */
/* Shared state layout (must match E9SharedRbc in Rust preempt/mod.rs) */
/* ------------------------------------------------------------------ */

struct e9_shared_rbc {
	int64_t  counter;
	int32_t  armed;
	int32_t  _pad;
	void    *yield_fn;
};

/* ------------------------------------------------------------------ */
/* call_yield — wrapper that aligns the stack to 16 bytes before       */
/* calling the Rust yield function (System V ABI requirement).         */
/* The e9tool "clean" ABI does NOT guarantee stack alignment.          */
/*                                                                     */
/* e9_preempt_yield() takes no arguments (it gets ring and worker_id   */
/* from Rust thread-local storage) and returns uint64_t (new counter). */
/* ------------------------------------------------------------------ */

asm (
".globl call_yield\n"
".type  call_yield, @function\n"
"call_yield:\n"
"    push   %rbp\n"
"    mov    %rsp, %rbp\n"
"    and    $-16, %rsp\n"       /* align stack to 16 bytes */
"    call   *%rdi\n"            /* call yield_fn() — no args */
"    mov    %rbp, %rsp\n"
"    pop    %rbp\n"
"    ret\n"
".size  call_yield, .-call_yield\n"
);

/* call_yield(yield_fn) -> int64_t (new counter value) */
extern int64_t call_yield(void *yield_fn);

/* ------------------------------------------------------------------ */
/* rbc_trampoline — called at each instrumented Jcc by e9tool.         */
/*                                                                     */
/* Fast path: load state from fixed address, decrement counter, return */
/* if > 0. All integer ops, safe under e9tool's "clean" ABI.           */
/* ------------------------------------------------------------------ */

void rbc_trampoline(void)
{
	volatile struct e9_shared_rbc *s = E9_SHARED_ADDR;
	int64_t c = s->counter - 1;
	s->counter = c;
	if (__builtin_expect(c > 0, 1))
		return;
	if (__builtin_expect(!s->armed, 0))
		return;
	void *yield_fn = (void *)s->yield_fn;
	if (__builtin_expect(yield_fn == 0, 0))
		return;
	/* Counter expired — yield via Rust, get new timeslice. */
	s->counter = call_yield(yield_fn);
}
