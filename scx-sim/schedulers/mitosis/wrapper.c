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

/*
 * NOTE: for mitosis this bpf_ksym_exists override is INERT -- =1 and =0 are
 * behavior-identical. It is kept only until the per-scheduler overrides are
 * replaced by a generic rule, and documents a subtlety worth not re-learning:
 *
 * This redefine lands AFTER sim_wrapper.h has included <scx/common.bpf.h> (hence
 * compat.bpf.h), so the static-inline __COMPAT_* helpers there (notably
 * __COMPAT_scx_bpf_cpu_curr) were already compiled against libbpf's
 * bpf_ksym_exists = !!sym and cannot be changed by this define. mitosis has no
 * direct bpf_ksym_exists / ___new / ___old uses, and its plain compat macros are
 * #undef-routed to sim exports in sim_wrapper.h -- so this override governs no
 * live mitosis path. __COMPAT_scx_bpf_cpu_curr always calls the real
 * scx_bpf_cpu_curr regardless of this value, because the simulator provides that
 * symbol (resolved at dlopen) so !!sym is always true.
 *
 * (An earlier comment here claimed =1 makes __COMPAT_scx_bpf_cpu_curr return the
 * real cpu_curr while =0 forced a NULL scx_bpf_cpu_rq fallback -- that was FALSE
 * per the include-order reasoning above.)
 */
#undef bpf_ksym_exists
#define bpf_ksym_exists(sym) (1)

/*
 * The simulator always calls select_cpu before enqueue, so the
 * CPU is always selected.
 */
#undef __COMPAT_is_enq_cpu_selected
#define __COMPAT_is_enq_cpu_selected(enq_flags) (true)

/*
 * __COMPAT_scx_bpf_dsq_peek -- route directly to the simulator's export.
 * This avoids taking the ksym-probing path when the scheduler asks for
 * lockless DSQ peek support.
 */
extern struct task_struct *scx_bpf_dsq_peek(u64 dsq_id);
#define __COMPAT_scx_bpf_dsq_peek(dsq_id) scx_bpf_dsq_peek(dsq_id)

/*
 * bpf_iter_scx_dsq_*: bpf_for_each(scx_dsq, ...) uses a cleanup() destructor,
 * so mitosis needs concrete function symbols, not just macro rewrites.
 */
extern void *sim_dsq_iter_begin(u64 dsq_id, u64 flags);
extern void *sim_dsq_iter_next(void);

#undef bpf_iter_scx_dsq_new
int bpf_iter_scx_dsq_new(struct bpf_iter_scx_dsq *it, u64 dsq_id, u64 flags)
{
	u64 *opaque = (u64 *)it;

	opaque[0] = (u64)(unsigned long)sim_dsq_iter_begin(dsq_id, flags);
	opaque[1] = 1;
	return 0;
}

#undef bpf_iter_scx_dsq_next
struct task_struct *bpf_iter_scx_dsq_next(struct bpf_iter_scx_dsq *it)
{
	u64 *opaque = (u64 *)it;

	if (opaque[1]) {
		opaque[1] = 0;
		return (struct task_struct *)(unsigned long)opaque[0];
	}

	return (struct task_struct *)sim_dsq_iter_next();
}

#undef bpf_iter_scx_dsq_destroy
void bpf_iter_scx_dsq_destroy(struct bpf_iter_scx_dsq *it)
{
	while (bpf_iter_scx_dsq_next(it))
		;
}

/*
 * is_migration_disabled: use the simulator's task_struct accessor.
 *
 * The kernel's is_migration_disabled() checks p->migration_disabled with
 * special handling for migration_disabled == 1 (ambiguous because BPF
 * prolog increments it). In the simulator, we use a simpler check:
 * migration_disabled > 0 means disabled.
 */
extern unsigned short sim_task_get_migration_disabled(struct task_struct *p);
#undef is_migration_disabled
#define is_migration_disabled(p) (sim_task_get_migration_disabled(p) > 0)

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

/* bpf_kptr_xchg: atomically exchange pointer, return old value */
static inline void *sim_kptr_xchg(void **kptr, void *new_val) {
	void *old = *kptr;
	*kptr = new_val;
	return old;
}
#undef bpf_kptr_xchg
#define bpf_kptr_xchg(kptr, val) sim_kptr_xchg((void **)(kptr), (void *)(val))

/* Cgroup acquire/release - simulator doesn't do reference counting */
static inline struct cgroup *sim_cgroup_acquire(struct cgroup *cgrp) {
	return cgrp;
}
#undef bpf_cgroup_acquire
#define bpf_cgroup_acquire(cgrp) sim_cgroup_acquire(cgrp)

#undef bpf_cgroup_release
#define bpf_cgroup_release(cgrp) ((void)0)

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

static struct scx_test_map debug_events_test_map;
static struct scx_test_map cells_test_map;
static struct scx_test_map cell_cpumasks_test_map;
static struct scx_test_map update_timer_test_map;
static struct scx_test_map task_ctxs_test_map;
static struct scx_test_map cgrp_ctxs_test_map;
static struct scx_percpu_test_map *cpu_ctxs_test_map;
static struct scx_percpu_test_map *cgrp_init_percpu_cpumask_test_map;

static void mitosis_register_maps(void)
{
	u32 i;
	int cpu;

	scx_test_map_clear_all();

	/* ARRAY maps: register + pre-seed [0..max_entries) with zeroed values. */
	{
		struct debug_event zero = {};
		INIT_SCX_TEST_MAP(&debug_events_test_map, debug_events);
		scx_test_map_register(&debug_events_test_map, &debug_events);
		for (i = 0; i < debug_events_test_map.max_entries; i++)
			bpf_map_update_elem(&debug_events, &i, &zero, 0);
	}
	{
		struct cell zero = {};
		INIT_SCX_TEST_MAP(&cells_test_map, cells);
		scx_test_map_register(&cells_test_map, &cells);
		for (i = 0; i < cells_test_map.max_entries; i++)
			bpf_map_update_elem(&cells, &i, &zero, 0);
	}
	{
		struct cell_cpumask_wrapper zero = {};
		INIT_SCX_TEST_MAP(&cell_cpumasks_test_map, cell_cpumasks);
		scx_test_map_register(&cell_cpumasks_test_map, &cell_cpumasks);
		for (i = 0; i < cell_cpumasks_test_map.max_entries; i++)
			bpf_map_update_elem(&cell_cpumasks, &i, &zero, 0);
	}
	{
		struct update_timer zero = {};
		INIT_SCX_TEST_MAP(&update_timer_test_map, update_timer);
		scx_test_map_register(&update_timer_test_map, &update_timer);
		for (i = 0; i < update_timer_test_map.max_entries; i++)
			bpf_map_update_elem(&update_timer, &i, &zero, 0);
	}

	/* PERCPU_ARRAY maps: per-CPU storage + pre-seed every key on every CPU. */
	{
		struct cpu_ctx zero = {};
		const u32 key0 = 0;
		cpu_ctxs_test_map = scx_alloc_percpu_test_map(MAX_SIM_CPUS);
		INIT_SCX_PERCPU_TEST_MAP(cpu_ctxs_test_map, cpu_ctxs);
		scx_register_percpu_test_map(cpu_ctxs_test_map, &cpu_ctxs);
		for (cpu = 0; cpu < (int)MAX_SIM_CPUS; cpu++)
			scx_test_map_update_percpu_elem(&cpu_ctxs, &key0, &zero,
							cpu, 0);
	}
	{
		struct cpumask_entry zero = {};
		cgrp_init_percpu_cpumask_test_map =
			scx_alloc_percpu_test_map(MAX_SIM_CPUS);
		INIT_SCX_PERCPU_TEST_MAP(cgrp_init_percpu_cpumask_test_map,
					 cgrp_init_percpu_cpumask);
		scx_register_percpu_test_map(cgrp_init_percpu_cpumask_test_map,
					     &cgrp_init_percpu_cpumask);
		for (cpu = 0; cpu < (int)MAX_SIM_CPUS; cpu++)
			for (i = 0; i < (u32)MAX_CPUMASK_ENTRIES; i++)
				scx_test_map_update_percpu_elem(
					&cgrp_init_percpu_cpumask, &i, &zero,
					cpu, 0);
	}

	/* TASK_STORAGE / CGRP_STORAGE: register; create-on-demand, identity-keyed. */
	INIT_SCX_TEST_MAP_FROM_TASK_STORAGE(&task_ctxs_test_map, task_ctxs);
	scx_test_map_register(&task_ctxs_test_map, &task_ctxs);
	INIT_SCX_TEST_MAP_FROM_TASK_STORAGE(&cgrp_ctxs_test_map, cgrp_ctxs);
	scx_test_map_register(&cgrp_ctxs_test_map, &cgrp_ctxs);
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
 * Sets global variables, populates the all_cpus bitmask, and clears
 * all static map arrays so the scheduler starts with clean state.
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

	/* Set globals to safe simulator values */
	nr_possible_cpus = num_cpus;
	smt_enabled = false;
	slice_ns = 20000000;   /* 20ms */
	root_cgid = 1;
	debug_events_enabled = false;
	exiting_task_workaround_enabled = false;
	cpu_controller_disabled = true;
	reject_multicpu_pinning = false;

	/* Populate all_cpus bitmask for each simulated CPU */
	memset((void *)all_cpus, 0, sizeof(all_cpus));
	for (i = 0; i < num_cpus && i < MAX_CPUS; i++)
		((volatile unsigned char *)all_cpus)[i / 8] |=
			(unsigned char)(1 << (i % 8));
}
