/*
 * sim_rbc_trampoline.c — e9patch arm/disarm/setup functions for the .so.
 *
 * This file is compiled into each scheduler .so. It provides:
 * - e9_worker_setup / e9_arm / e9_disarm: called by the Rust backend
 *   (E9PatchBackend) via libloading-resolved function pointers.
 * - e9_so_init: dummy DT_INIT entry so e9tool can inject its loader.
 *
 * All state lives in E9_SHARED_RBC, a global struct exported by the main
 * binary (Rust) via #[no_mangle]. The e9patch trampoline binary reads the
 * same struct (resolved via dlsym), ensuring consistent state.
 *
 * This file does NOT include sim_wrapper.h or vmlinux.h to avoid conflicts
 * with standard C headers.
 */

#include <stdint.h>

/* ------------------------------------------------------------------ */
/* Shared state — defined in Rust (preempt/mod.rs), exported via       */
/* -rdynamic and --undefined=E9_SHARED_RBC.                            */
/* ------------------------------------------------------------------ */

struct e9_shared_rbc {
	int64_t counter;
	int32_t armed;
	int32_t worker_id;
	void   *ring_ptr;
};

extern struct e9_shared_rbc E9_SHARED_RBC;

/* ------------------------------------------------------------------ */
/* Functions called from Rust backend to configure shared state        */
/* ------------------------------------------------------------------ */

__attribute__((used, visibility("default")))
void e9_worker_setup(void *ring, int worker)
{
	E9_SHARED_RBC.ring_ptr  = ring;
	E9_SHARED_RBC.worker_id = worker;
	E9_SHARED_RBC.counter   = INT64_MAX;
	E9_SHARED_RBC.armed     = 0;
}

__attribute__((used, visibility("default")))
void e9_arm(uint64_t timeslice)
{
	E9_SHARED_RBC.counter = (int64_t)timeslice;
	E9_SHARED_RBC.armed   = 1;
}

__attribute__((used, visibility("default")))
void e9_disarm(void)
{
	E9_SHARED_RBC.armed   = 0;
	E9_SHARED_RBC.counter = INT64_MAX;
}

__attribute__((used, visibility("default")))
int64_t e9_read_counter(void)
{
	return E9_SHARED_RBC.counter;
}

/* ------------------------------------------------------------------ */
/* Provide a DT_INIT entry so e9tool can inject its loader.           */
/* Without this, .so files linked with -nostdlib have no init section  */
/* and e9patch refuses to instrument them. The linker creates DT_INIT  */
/* via -Wl,--init=e9_so_init in the Makefile.                         */
/* ------------------------------------------------------------------ */

__attribute__((used, visibility("default")))
void e9_so_init(void) {}
