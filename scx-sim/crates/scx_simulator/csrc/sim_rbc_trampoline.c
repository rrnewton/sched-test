/*
 * sim_rbc_trampoline.c — e9patch arm/disarm functions for the .so.
 *
 * This file is compiled into each scheduler .so. It provides:
 * - e9_arm / e9_disarm: called by the Rust backend (E9PatchBackend)
 *   via libloading-resolved function pointers.
 * - e9_so_init: dummy DT_INIT entry so e9tool can inject its loader.
 *
 * All state lives at the fixed mmap'd address E9_SHARED_ADDR (0x1E9000000),
 * shared between the Rust backend, the .so functions here, and the
 * e9-injected trampoline code.
 *
 * This file does NOT include sim_wrapper.h or vmlinux.h to avoid conflicts
 * with standard C headers.
 */

#include <stdint.h>

/* ------------------------------------------------------------------ */
/* Fixed shared address — must match E9_SHARED_ADDR in Rust and the   */
/* e9_rbc_trampoline.c trampoline binary.                              */
/* ------------------------------------------------------------------ */

struct e9_shared_rbc {
	int64_t  counter;
	int32_t  armed;
	int32_t  _pad;
	void    *yield_fn;
};

#define E9_SHARED_ADDR  ((volatile struct e9_shared_rbc *)0x1E9000000ULL)

/* ------------------------------------------------------------------ */
/* Functions called from Rust backend to configure shared state        */
/* ------------------------------------------------------------------ */

__attribute__((used, visibility("default")))
void e9_arm(uint64_t timeslice)
{
	volatile struct e9_shared_rbc *s = E9_SHARED_ADDR;
	s->counter = (int64_t)timeslice;
	s->armed   = 1;
}

__attribute__((used, visibility("default")))
void e9_disarm(void)
{
	volatile struct e9_shared_rbc *s = E9_SHARED_ADDR;
	s->armed   = 0;
	s->counter = INT64_MAX;
}

__attribute__((used, visibility("default")))
int64_t e9_read_counter(void)
{
	return E9_SHARED_ADDR->counter;
}

/* ------------------------------------------------------------------ */
/* Provide a DT_INIT entry so e9tool can inject its loader.           */
/* Without this, .so files linked with -nostdlib have no init section  */
/* and e9patch refuses to instrument them. The linker creates DT_INIT  */
/* via -Wl,--init=e9_so_init in the Makefile.                         */
/* ------------------------------------------------------------------ */

__attribute__((used, visibility("default")))
void e9_so_init(void) {}
