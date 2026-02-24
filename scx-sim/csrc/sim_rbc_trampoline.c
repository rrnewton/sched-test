/*
 * sim_rbc_trampoline.c — Software RBC trampoline for e9patch preemption.
 *
 * This file is compiled into each scheduler .so. The rbc_trampoline()
 * function is called by e9patch at every instrumented Jcc (conditional
 * branch) instruction. It decrements a global counter and, when
 * the counter expires, calls into Rust to yield via the PreemptRing.
 *
 * Global (not TLS) state is used because the PreemptRing token protocol
 * guarantees only one worker is active at a time. The same globals are
 * accessed by both the Rust backend (via e9_arm/e9_disarm) and the
 * trampoline (rbc_trampoline), ensuring consistent state.
 *
 * The trampoline and supporting functions are placed in the .text.rbc
 * section so that e9tool can exclude them from instrumentation (preventing
 * infinite recursion).
 *
 * This file does NOT include sim_wrapper.h or vmlinux.h to avoid conflicts
 * with standard C headers.
 */

#include <stdint.h>

/* ------------------------------------------------------------------ */
/* Global software RBC state (single active worker via token ring)    */
/* ------------------------------------------------------------------ */

static int64_t  rbc_counter   = INT64_MAX;  /* disarmed initially */
static int      rbc_armed     = 0;
static void    *rbc_ring_ptr  = (void *)0;  /* *const PreemptRing  */
static int      rbc_worker_id = -1;

/* ------------------------------------------------------------------ */
/* Declared in Rust (preempt/mod.rs), exported via -rdynamic           */
/* ------------------------------------------------------------------ */

extern uint64_t e9_preempt_yield(void *ring, int worker_id);

/* ------------------------------------------------------------------ */
/* Functions called from Rust backend to configure state               */
/* ------------------------------------------------------------------ */

__attribute__((section(".text.rbc"), used, visibility("default")))
void e9_worker_setup(void *ring, int worker)
{
	rbc_ring_ptr  = ring;
	rbc_worker_id = worker;
	rbc_counter   = INT64_MAX;
	rbc_armed     = 0;
}

__attribute__((section(".text.rbc"), used, visibility("default")))
void e9_arm(uint64_t timeslice)
{
	rbc_counter = (int64_t)timeslice;
	rbc_armed   = 1;
}

__attribute__((section(".text.rbc"), used, visibility("default")))
void e9_disarm(void)
{
	rbc_armed   = 0;
	rbc_counter = INT64_MAX;
}

__attribute__((section(".text.rbc"), used, visibility("default")))
int64_t e9_read_counter(void)
{
	return rbc_counter;
}

/* ------------------------------------------------------------------ */
/* Provide a DT_INIT entry so e9tool can inject its loader.           */
/* Without this, .so files linked with -nostdlib have no init section  */
/* and e9patch refuses to instrument them. The linker creates DT_INIT  */
/* via -Wl,--init=e9_so_init in the Makefile.                         */
/* ------------------------------------------------------------------ */

__attribute__((used, visibility("default")))
void e9_so_init(void) {}

/* ------------------------------------------------------------------ */
/* The trampoline — called at each instrumented Jcc                    */
/* ------------------------------------------------------------------ */

__attribute__((section(".text.rbc"), used, visibility("default")))
void rbc_trampoline(void)
{
	if (__builtin_expect(--rbc_counter > 0, 1))
		return;
	if (__builtin_expect(!rbc_armed, 0))
		return;
	/* Counter expired — yield via Rust, get new timeslice. */
	rbc_counter = (int64_t)e9_preempt_yield(rbc_ring_ptr, rbc_worker_id);
}
