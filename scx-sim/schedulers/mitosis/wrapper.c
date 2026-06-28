/*
 * mitosis_wrapper.c - Wrapper to compile scx_mitosis as userspace C
 *
 * This file includes the simulator wrapper infrastructure and then
 * the actual scheduler source. The header guards in common.bpf.h
 * prevent re-inclusion, so our overridden macros take effect.
 *
 * NOTE: This file is compiled with -Dconst= to strip const qualifiers.
 * BPF schedulers declare globals as "const volatile" (patched by the
 * BPF loader). Stripping const makes them writable from Rust.
 *
 * Map strategy: all maps use the generic scx_test_map registry
 * (mitosis_register_maps): ARRAY / PERCPU_ARRAY maps are registered and
 * pre-seeded so per-index lookups always hit; TASK_STORAGE / CGRP_STORAGE
 * use the registry's object-identity-keyed, per-slot-stable storage.
 */
#include "sim_wrapper.h"
#include "sim_task.h"

/* Forward declarations for libc functions */
extern void *memset(void *s, int c, unsigned long n);

/* ---------------------------------------------------------------------------
 * Macro overrides (defined after sim_wrapper.h, before mitosis.bpf.c)
 * ---------------------------------------------------------------------------*/

/* ---------------------------------------------------------------------------
 * Map access: all 8 mitosis maps use the generic scx_test_map registry.
 *
 * bpf_map_lookup_elem / bpf_map_lookup_percpu_elem / bpf_task_storage_get /
 * bpf_cgrp_storage_get resolve to the scx_test_map.h macros (the latter two
 * are object-identity keyed). The maps are registered + the ARRAY/PERCPU
 * ones pre-seeded in mitosis_register_maps() below, called from mitosis_setup
 * before mitosis_init. The former bespoke static-array overrides (a PID-indexed
 * task array, per-map static ARRAY buffers, a cgroup-pointer scan) are gone --
 * the generic registry now provides per-index ARRAY storage, current-CPU PERCPU
 * resolution, and pointer-identity + per-slot-stable task/cgroup storage.
 * ---------------------------------------------------------------------------*/

/* ---------------------------------------------------------------------------
 * BPF timer overrides
 *
 * bpf_timer_set_callback stores the callback pointer.
 * bpf_timer_start calls sim_timer_start() (Rust kfunc) to schedule
 * a TimerFired event in the simulator's event queue.
 * mitosis_fire_timer() invokes the stored callback from the engine.
 * ---------------------------------------------------------------------------*/
static int (*mitosis_timer_cb)(void *, int *, struct bpf_timer *);
static struct bpf_timer *mitosis_timer_ptr;
static void *mitosis_timer_map;

extern void sim_timer_start(unsigned long long nsecs);

#undef bpf_timer_init
#define bpf_timer_init(timer, map, flags) \
	(mitosis_timer_map = (void *)(map), 0)

#undef bpf_timer_set_callback
#define bpf_timer_set_callback(timer, cb) \
	(mitosis_timer_cb = (typeof(mitosis_timer_cb))(cb), \
	 mitosis_timer_ptr = (struct bpf_timer *)(timer), 0)

#undef bpf_timer_start
#define bpf_timer_start(timer, nsecs, flags) \
	(sim_timer_start(nsecs), 0)

/* ---------------------------------------------------------------------------
 * RAII cleanup neutralization
 *
 * The upstream mitosis BPF code uses gcc cleanup attributes for automatic
 * resource management (RAII). The pattern is:
 *   struct cgroup *cgrp __free(cgroup) = bpf_cgroup_from_id(id);
 *
 * This calls __free_cgroup(cgrp) when the scope exits, which calls
 * bpf_cgroup_release(). In the simulator, we don't do reference counting,
 * so we neutralize these macros.
 *
 * The macros are defined in cleanup.bpf.h which is included via mitosis.bpf.h.
 * We include cleanup.bpf.h FIRST to let it define its macros, then we
 * override them with our neutralized versions.
 * ---------------------------------------------------------------------------*/

/* Include cleanup.bpf.h to let it define its RAII framework first.
 * Upstream moved cleanup.bpf.h from scx_mitosis/src/bpf/ to
 * scheds/include/lib/. Use angle-bracket include to find it via
 * -I$(ROOT_DIR)/scheds/include. */
#include <lib/cleanup.bpf.h>

/* Now override the RAII macros with neutralized versions */

/* Strip __free() cleanup attributes - simulator manages resources manually */
#undef __free
#define __free(x)

/* no_free_ptr just returns the pointer unchanged */
#undef no_free_ptr
#define no_free_ptr(p) (p)

/*
 * Cgroup acquire/release (no reference counting in the sim) are the generic
 * weak stubs in csrc/sim_bpf_stubs.c (acquire=identity, release=no-op),
 * shared by every .so. No per-scheduler override needed here.
 */

/*
 * bpf_iter_css_*: the simulator's compare tests only exercise the root
 * cgroup, so a single-element iterator is sufficient for userspace init.
 */
int bpf_iter_css_new(struct bpf_iter_css *it,
		     struct cgroup_subsys_state *start,
		     unsigned int flags)
{
	struct bpf_iter_css_kern *iter = (struct bpf_iter_css_kern *)it;

	iter->start = start;
	iter->pos = NULL;
	iter->flags = flags;
	return 0;
}

struct cgroup_subsys_state *bpf_iter_css_next(struct bpf_iter_css *it)
{
	struct bpf_iter_css_kern *iter = (struct bpf_iter_css_kern *)it;

	if (iter->pos)
		return NULL;

	iter->pos = iter->start;
	return iter->start;
}

void bpf_iter_css_destroy(struct bpf_iter_css *it)
{
	(void)it;
}

/*
 * cpumask acquire/release:
 * - acquire: just return the cpumask (no refcounting)
 * - release: must actually free since bpf_cpumask_create allocates
 *
 * Note: bpf_cpumask_release is already handled by scx_test_map.h or
 * we need to provide our own implementation that calls sim_cpumask_release.
 */

/* ---------------------------------------------------------------------------
 * Include mitosis source
 * ---------------------------------------------------------------------------*/
#include "intf.h"
#include "mitosis.bpf.c"

/* ---------------------------------------------------------------------------
 * Implementations (after mitosis.bpf.c, so struct types are available)
 * ---------------------------------------------------------------------------*/

/*
 * Map registration.
 *
 * Register every mitosis BPF map with the generic scx_test_map registry and
 * pre-seed the ARRAY / PERCPU_ARRAY maps. The kernel pre-allocates ARRAY
 * entries (zeroed); mitosis's lookups scx_bpf_error() on a NULL miss, so the
 * full key range must exist up front. TASK_STORAGE / CGRP_STORAGE are
 * create-on-demand, keyed by object identity (the pointer-identity +
 * per-slot-stable storage backend). Called from mitosis_setup() before
 * mitosis_init(); scx_test_map_clear_all() gives clean state on re-load.
 */
#define MAX_SIM_CPUS 128

static void mitosis_register_maps(void)
{
	scx_test_map_clear_all();

	/* ARRAY maps: register + pre-seed [0..max_entries) with zeroed values. */
	SCX_REGISTER_ARRAY(debug_events, true);
	SCX_REGISTER_ARRAY(cells, true);
	SCX_REGISTER_ARRAY(cell_cpumasks, true);
	SCX_REGISTER_ARRAY(update_timer, true);

	/* PERCPU_ARRAY maps: per-CPU storage + pre-seed every key on every CPU. */
	SCX_REGISTER_PERCPU(cpu_ctxs, true);
	SCX_REGISTER_PERCPU(cgrp_init_percpu_cpumask, true);

	/* TASK_STORAGE / CGRP_STORAGE: register; create-on-demand, identity-keyed. */
	SCX_REGISTER_STORAGE(task_ctxs);
	SCX_REGISTER_STORAGE(cgrp_ctxs);
}

/* ---------------------------------------------------------------------------
 * fire_timer: called from the Rust engine when a TimerFired event fires.
 *
 * Phase 1 BPF infra scale-up items 1+2 (tg
 * `scxsim-bpf-infra-scale-up-phase1`): the engine now passes a `slot`
 * id so multi-timer schedulers (LAVD post-Phase-1, Phase-2 compiled-in
 * cgroup_bw library) can dispatch to the right callback. Mitosis is a
 * single-timer scheduler; it ignores `slot` and always fires its only
 * timer (whatever was last installed via mitosis_timer_set_callback).
 * The single arg is required by the new FFI signature
 * `FireTimerFn = unsafe extern "C" fn(u32)` so the symbol resolves.
 * ---------------------------------------------------------------------------*/
void mitosis_fire_timer(unsigned int slot)
{
	int key = 0;
	(void)slot;
	if (mitosis_timer_cb && mitosis_timer_ptr)
		mitosis_timer_cb(mitosis_timer_map, &key, mitosis_timer_ptr);
}

/* ---------------------------------------------------------------------------
 * mitosis_sum_cstat: host-side test observation of a per-cell stat counter.
 *
 * cpu_ctxs is a PERCPU_ARRAY (single key 0); each CPU's struct cpu_ctx holds
 * u64 cstats[MAX_CELLS][NR_CSTATS] (intf.h). cstat_inc accumulates per (cell,
 * cpu), so the host-visible total for a stat index is the sum over all cells
 * and all CPUs. Summing every cell avoids assuming which cell a task lands in.
 * Lets a test observe e.g. CSTAT_SLICE_SHRINK_* firing, which is not visible in
 * the event trace (slice_shrink_apply writes p->scx.slice + bumps the counter).
 * Mirrors the host-context map access the setup seed loop already performs.
 * ---------------------------------------------------------------------------*/
u64 mitosis_sum_cstat(u32 idx)
{
	const u32 key0 = 0;
	u64 sum = 0;
	int cpu;
	u32 cell;

	if (idx >= NR_CSTATS)
		return 0;
	for (cpu = 0; cpu < (int)nr_possible_cpus && cpu < (int)MAX_SIM_CPUS; cpu++) {
		struct cpu_ctx *cctx =
			scx_test_map_lookup_percpu_elem(&cpu_ctxs, &key0, cpu);
		if (!cctx)
			continue;
		for (cell = 0; cell < (u32)MAX_CELLS; cell++)
			sum += cctx->cstats[cell][idx];
	}
	return sum;
}

/* ---------------------------------------------------------------------------
 * Setup function called from Rust before mitosis_init().
 *
 * Registers + pre-seeds the maps, clears timer state, and populates the
 * all_cpus bitmask. The config globals are written before run by the manifest
 * apply_rodata path (scheduler_manifest.rs mitosis.runtime.rodata), not here.
 * ---------------------------------------------------------------------------*/
void mitosis_setup(unsigned int num_cpus)
{
	unsigned int i;

	/* Register + pre-seed all maps via the generic scx_test_map registry
	 * (replaces the former static-array zeroing). */
	mitosis_register_maps();

	/* Clear timer state from previous runs */
	mitosis_timer_cb = NULL;
	mitosis_timer_ptr = NULL;
	mitosis_timer_map = NULL;

	/* Populate all_cpus bitmask for each simulated CPU */
	memset((void *)all_cpus, 0, sizeof(all_cpus));
	for (i = 0; i < num_cpus && i < MAX_CPUS; i++)
		((volatile unsigned char *)all_cpus)[i / 8] |=
			(unsigned char)(1 << (i % 8));
}
