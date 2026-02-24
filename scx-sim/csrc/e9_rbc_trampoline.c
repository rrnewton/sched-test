/*
 * e9_rbc_trampoline.c — e9patch call trampoline for software RBC counting.
 *
 * Compiled with e9compile.sh into a trampoline binary that e9tool injects
 * into each instrumented scheduler .so. Called at every Jcc instruction.
 *
 * On init(), resolves the shared state struct (E9_SHARED_RBC) and the yield
 * function (e9_preempt_yield) from the main binary via dlsym. The fast path
 * (counter > 0) is a single decrement + branch with no function calls.
 * The slow path (counter expired) calls e9_preempt_yield via dlcall() to
 * handle ABI alignment differences.
 *
 * Global (not TLS) state is fine because the PreemptRing token protocol
 * guarantees only one worker is active at a time.
 *
 * Must be compiled from the e9patch directory so #include "stdlib.c" resolves:
 *   cd third_party/e9patch && ./e9compile.sh <path>/e9_rbc_trampoline.c
 */

#include <stdint.h>

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

/* Resolved pointers — set once during init(), used on every Jcc. */
static struct e9_shared_rbc *state = NULL;
static void *yield_fn = NULL;

/* ------------------------------------------------------------------ */
/* init — called once by e9tool's loader when the patched .so loads.   */
/* Resolves E9_SHARED_RBC and e9_preempt_yield from the main binary.   */
/* ------------------------------------------------------------------ */

void init(int argc, char **argv, char **envp, void *dynamic)
{
	(void)argc; (void)argv; (void)envp;
	if (dlinit(dynamic) < 0)
		return;

	void *handle = dlopen(NULL, 0x00001); /* RTLD_LAZY */
	if (!handle)
		return;

	state = (struct e9_shared_rbc *)dlsym(handle, "E9_SHARED_RBC");
	yield_fn = dlsym(handle, "e9_preempt_yield");
}

/* ------------------------------------------------------------------ */
/* rbc_trampoline — called at each instrumented Jcc by e9tool.         */
/*                                                                     */
/* Fast path: decrement counter, return if > 0. Integer ops only,      */
/* safe under e9tool's "clean" ABI (no SSE save needed).               */
/*                                                                     */
/* Slow path: call e9_preempt_yield via dlcall() for proper ABI        */
/* alignment (16-byte stack, SSE save/restore).                        */
/* ------------------------------------------------------------------ */

void rbc_trampoline(void)
{
	if (__builtin_expect(state == NULL, 0))
		return;
	if (__builtin_expect(--(state->counter) > 0, 1))
		return;
	if (__builtin_expect(!state->armed, 0))
		return;
	if (__builtin_expect(yield_fn == NULL, 0))
		return;
	/* Counter expired — yield via Rust, get new timeslice. */
	state->counter = (int64_t)dlcall(yield_fn,
		state->ring_ptr, (intptr_t)state->worker_id);
}
