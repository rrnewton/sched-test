/*
 * e9_rbc_trampoline.c — e9patch call trampoline for software RBC counting.
 *
 * Compiled with e9compile.sh. Called at every instrumented Jcc instruction.
 * Uses a magic global array that the Rust backend writes the E9_SHARED_RBC
 * pointer and e9_preempt_yield function pointer into after loading the .so.
 *
 * Must be compiled from the e9patch directory so #include "stdlib.c" resolves:
 *   cd third_party/e9patch && ./e9compile.sh <path>/e9_rbc_trampoline.c
 */

#include <stdint.h>

/* Need LIBDL for dlcall() which handles ABI alignment when calling
 * e9_preempt_yield (stack alignment, SSE save/restore). */
#define LIBDL
#include "stdlib.c"

/* ------------------------------------------------------------------ */
/* Shared state layout (must match E9SharedRbc in Rust preempt/mod.rs) */
/* ------------------------------------------------------------------ */

struct e9_shared_rbc {
	int64_t counter;
	int32_t armed;
	int32_t worker_id;
	void   *ring_ptr;
};

/* ------------------------------------------------------------------ */
/* Magic mailbox: the Rust backend writes pointers here after dlopen.  */
/*                                                                     */
/* e9_mailbox[0] = pointer to E9_SHARED_RBC struct                    */
/* e9_mailbox[1] = pointer to e9_preempt_yield function               */
/*                                                                     */
/* NOT static — exported via --export-dynamic so the Rust backend can  */
/* find it in /proc/self/maps by searching for the magic sentinel.     */
/* Initialized with a known magic value so the Rust backend can locate */
/* this array by scanning the trampoline's data pages.                 */
/* ------------------------------------------------------------------ */

#define E9_MAILBOX_MAGIC  0xE90A7C00C0FFEEULL
volatile uint64_t e9_mailbox[4] = {
	E9_MAILBOX_MAGIC, /* [0]: magic sentinel (replaced with state ptr) */
	0,                /* [1]: yield_fn ptr (set by Rust)                */
	0,                /* [2]: reserved                                  */
	0,                /* [3]: reserved                                  */
};

/* ------------------------------------------------------------------ */
/* rbc_trampoline — called at each instrumented Jcc by e9tool.         */
/* ------------------------------------------------------------------ */

void rbc_trampoline(void)
{
	struct e9_shared_rbc *state =
		(struct e9_shared_rbc *)(uintptr_t)e9_mailbox[0];
	if (__builtin_expect(state == NULL ||
	                     (uintptr_t)state == E9_MAILBOX_MAGIC, 0))
		return;
	if (__builtin_expect(--(state->counter) > 0, 1))
		return;
	if (__builtin_expect(!state->armed, 0))
		return;
	void *yield_fn = (void *)(uintptr_t)e9_mailbox[1];
	if (__builtin_expect(yield_fn == NULL, 0))
		return;
	/* Counter expired — yield via Rust, get new timeslice.
	 * Use dlcall for proper ABI alignment (16-byte stack). */
	state->counter = (int64_t)dlcall(yield_fn,
		state->ring_ptr, (intptr_t)state->worker_id);
}
