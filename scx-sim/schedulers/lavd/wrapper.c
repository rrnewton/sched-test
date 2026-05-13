/*
 * lavd_wrapper.c - Wrapper to compile scx_lavd as userspace C
 *
 * This file includes the simulator wrapper infrastructure and then
 * all LAVD BPF source files as a single translation unit. Header
 * guards prevent re-inclusion of common headers.
 *
 * NOTE: Compiled with -Dconst= to strip const qualifiers so BPF
 * "const volatile" globals become writable from Rust.
 */
#include "sim_wrapper.h"
#include "sim_task.h"

/*
 * =================================================================
 * LAVD-specific macro overrides
 * (after sim_wrapper.h, before LAVD source)
 * =================================================================
 */

/*
 * MEMBER_VPTR: In BPF, this uses inline asm for verifier-checked bounds.
 * In userspace, the BPF asm is invalid. Replace with a plain address
 * computation.  The bounds check is unnecessary in userspace since we
 * control array sizes.
 */
#undef MEMBER_VPTR
#define MEMBER_VPTR(base, member) \
	((typeof((base) member) *)(&((base) member)))

/*
 * __hidden: BPF internal visibility attribute. Defined in libbpf's
 * bpf_helpers.h which may not be available. Provide a fallback.
 */
#ifndef __hidden
#define __hidden __attribute__((visibility("hidden")))
#endif

/*
 * bpf_strncmp has reversed argument order vs C strncmp.
 *   BPF: bpf_strncmp(s1, n, s2) -- compares s1[0..n] against s2
 *   C:   strncmp(s1, s2, n)
 * Use __builtin_strncmp to avoid declaration issues with -Dconst=.
 */
#undef bpf_strncmp
#define bpf_strncmp(s1, n, s2) __builtin_strncmp(s1, s2, n)

/*
 * Ring buffer stubs -- introspection is not needed in simulation.
 * bpf_ringbuf_reserve/submit are static function pointers from
 * bpf_helper_defs.h, so #undef + #define is safe.
 */
#undef bpf_ringbuf_reserve
#define bpf_ringbuf_reserve(map, sz, flags) ((void *)0)
#undef bpf_ringbuf_submit
#define bpf_ringbuf_submit(data, flags) do {} while(0)

/*
 * bpf_per_cpu_ptr -- kernel per-CPU variables don't exist in the
 * simulator. Return NULL so callers skip the code path.
 */
#undef bpf_per_cpu_ptr
#define bpf_per_cpu_ptr(ptr, cpu) ((typeof(ptr))0)

/*
 * bpf_get_current_pid_tgid -- no meaningful PID in the simulator.
 * Static function pointer in bpf_helper_defs.h, safe to override.
 */
#undef bpf_get_current_pid_tgid
#define bpf_get_current_pid_tgid() ((u64)0)

/*
 * bpf_ksym_exists -- kernel symbol existence check.
 * Return 0 (absent) to disable kfunc probing paths.
 */
#undef bpf_ksym_exists
#define bpf_ksym_exists(sym) (0)

/*
 * __COMPAT_scx_bpf_dsq_peek -- override the compat wrapper to directly
 * call scx_bpf_dsq_peek which is implemented in kfuncs.rs. The compat
 * wrapper normally falls through to bpf_iter_scx_dsq_* when bpf_ksym_exists
 * returns 0, but those iterators aren't implemented in the simulator.
 */
extern struct task_struct *scx_bpf_dsq_peek(u64 dsq_id);
#define __COMPAT_scx_bpf_dsq_peek(dsq_id) scx_bpf_dsq_peek(dsq_id)

/*
 * bpf_iter_scx_dsq_*: bpf_for_each(scx_dsq, ...) uses a cleanup() destructor,
 * so lavd needs concrete function symbols, not just macro rewrites.
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
 * __builtin_memcpy_inline fallback for non-Clang or older versions.
 */
#ifndef __has_builtin
#define __has_builtin(x) 0
#endif
#if !__has_builtin(__builtin_memcpy_inline)
#define __builtin_memcpy_inline(dst, src, sz) __builtin_memcpy(dst, src, sz)
#endif

/*
 * The simulator always calls select_cpu before enqueue.
 */
#undef __COMPAT_is_enq_cpu_selected
#define __COMPAT_is_enq_cpu_selected(enq_flags) (true)

/*
 * is_migration_disabled: use the simulator's task_struct accessor.
 *
 * The kernel's is_migration_disabled() checks p->migration_disabled with
 * special handling for migration_disabled == 1 (ambiguous because BPF
 * prolog increments it). In the simulator, we don't run actual BPF code,
 * so we use a simpler check: migration_disabled > 0 means disabled.
 *
 * For production bug reproduction (sim-7cc89), set migration_disabled >= 2
 * on the task to model a task that was already migration-disabled before
 * entering the scheduler callback.
 */
extern unsigned short sim_task_get_migration_disabled(struct task_struct *p);
#undef is_migration_disabled
#define is_migration_disabled(p) (sim_task_get_migration_disabled(p) > 0)

/*
 * scx_clock_task / scx_clock_pelt override.
 *
 * Route to the simulator's kfunc which returns local_clock - irq_cumulative_ns.
 * This lets LAVD's steal_util tracking detect IRQ-heavy CPUs as turbulent.
 */
extern u64 sim_scx_clock_task(u32 cpu);
#define scx_clock_task(cpu) sim_scx_clock_task(cpu)
#define scx_clock_pelt(cpu) sim_scx_clock_task(cpu)

/*
 * =================================================================
 * Per-CPU context and map infrastructure
 * =================================================================
 */
#define MAX_SIM_CPUS 128

/*
 * Forward declaration for per-CPU lookup.
 * Defined after LAVD source since struct cpu_ctx is needed.
 */
static struct cpu_ctx *lavd_lookup_percpu_elem(int cpu);
#undef bpf_map_lookup_percpu_elem
#define bpf_map_lookup_percpu_elem(map, key, cpu) lavd_lookup_percpu_elem(cpu)

/*
 * BPF timer overrides for periodic system stat updates AND for any
 * additional timer the cgroup_bw library compiled in by Phase 2 will
 * arm (e.g. `cbw_replenish_timer`).
 *
 * Phase 1 BPF infra scale-up items 1+2 (tg
 * `scxsim-bpf-infra-scale-up-phase1`, design doc section Phase 1 items
 * 1+2): replaces the prior single global timer state with a fixed
 * `LAVD_MAX_BPF_TIMERS = 8` slot table keyed by `(struct bpf_timer *)`.
 *
 * Slot allocation policy: `bpf_timer_init(timer, map, flags)` finds
 * the first free slot whose `timer_ptr` is NULL, claims it for the
 * supplied `(struct bpf_timer *)`, and records the timer's map.
 * `bpf_timer_set_callback(timer, cb)` and
 * `bpf_timer_start(timer, nsecs, flags)` look up the slot for the
 * supplied `timer` and update / fire it. `lavd_fire_timer(slot)`
 * dispatches the engine's `EventKind::TimerFired { slot }` to the
 * right callback.
 *
 * Slot 0 is the conventional update_timer slot; subsequent slots are
 * assigned in init order. The table is reset by `lavd_register_maps`
 * so consecutive simulation runs start with a clean slate (matches
 * the existing single-timer determinism guarantee).
 *
 * This MUST stay <= the Rust-side `MAX_BPF_TIMERS` constant in
 * `unsafe_impl/kfuncs.rs`. If LAVD ever needs more, bump both and
 * add a build-time assertion.
 */
#define LAVD_MAX_BPF_TIMERS 8

struct lavd_timer_slot {
	struct bpf_timer *timer_ptr; /* NULL = unused slot */
	int (*timer_cb)(void *, int *, struct bpf_timer *);
	void *timer_map;
};

static struct lavd_timer_slot lavd_timer_table[LAVD_MAX_BPF_TIMERS];

extern void sim_timer_start(unsigned long long nsecs);
extern void sim_timer_start_slot(unsigned int slot, unsigned long long nsecs);

/*
 * Find or assign a slot for the given (struct bpf_timer *).
 *
 * Returns the slot index in [0, LAVD_MAX_BPF_TIMERS), or -1 on
 * exhaustion (which means LAVD_MAX_BPF_TIMERS / MAX_BPF_TIMERS is too
 * low for the workload -- bump them in lockstep).
 */
static int lavd_timer_slot_for(struct bpf_timer *timer)
{
	int i;
	for (i = 0; i < LAVD_MAX_BPF_TIMERS; i++) {
		if (lavd_timer_table[i].timer_ptr == timer)
			return i;
	}
	for (i = 0; i < LAVD_MAX_BPF_TIMERS; i++) {
		if (!lavd_timer_table[i].timer_ptr) {
			lavd_timer_table[i].timer_ptr = timer;
			return i;
		}
	}
	return -1;
}

#undef bpf_timer_init
#define bpf_timer_init(timer, map, flags) \
	({ \
		int _s = lavd_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) lavd_timer_table[_s].timer_map = (void *)(map); \
		0; \
	})

#undef bpf_timer_set_callback
#define bpf_timer_set_callback(timer, cb) \
	({ \
		int _s = lavd_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) \
			lavd_timer_table[_s].timer_cb = \
				(typeof(lavd_timer_table[0].timer_cb))(cb); \
		0; \
	})

#undef bpf_timer_start
#define bpf_timer_start(timer, nsecs, flags) \
	({ \
		int _s = lavd_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) sim_timer_start_slot((unsigned int)_s, (nsecs)); \
		0; \
	})

/*
 * Map lookup override.
 *
 * LAVD uses bpf_map_lookup_elem for two maps:
 *   cpu_ctx_stor (PERCPU_ARRAY) -- routed to our static per-CPU array
 *   update_timer (ARRAY)        -- routed to static backing storage
 *
 * Pointers are set in lavd_register_maps() after the source is included.
 */
static void *lavd_cpu_ctx_stor_ptr;
static void *lavd_update_timer_map_ptr;
static char lavd_update_timer_buf[256];

extern unsigned int sim_bpf_get_smp_processor_id(void);

/*
 * Forward declaration -- definition after LAVD source where struct cpu_ctx
 * is available.
 */
static void *lavd_map_lookup(void *map, const void *key);

#undef bpf_map_lookup_elem
#define bpf_map_lookup_elem(map, key) lavd_map_lookup((void *)(map), key)

/*
 * __COMPAT_scx_bpf_cpu_curr override.
 * Return actual running task or a synthetic idle task.
 */
extern struct task_struct *scx_bpf_cpu_curr(int cpu);
static struct task_struct sim_lavd_idle_task;
static bool sim_lavd_idle_init;

static struct task_struct *lavd_cpu_curr(int cpu)
{
	struct task_struct *p = scx_bpf_cpu_curr(cpu);
	if (p)
		return p;
	if (!sim_lavd_idle_init) {
		__builtin_memset(&sim_lavd_idle_task, 0,
				 sizeof(sim_lavd_idle_task));
		sim_lavd_idle_task.flags = PF_IDLE;
		sim_lavd_idle_init = true;
	}
	return &sim_lavd_idle_task;
}

#undef __COMPAT_scx_bpf_cpu_curr
#define __COMPAT_scx_bpf_cpu_curr(cpu) lavd_cpu_curr(cpu)

/*
 * Division-by-zero protection: provided by sim_sigfpe.c (separate TU
 * to avoid signal.h / vmlinux.h type conflicts).
 */
extern void sim_install_sigfpe_handler(void);

/*
 * Per-task arena storage initialization (sim_sdt_stubs.c).
 * Must be called before any scx_task_alloc() calls.
 */
extern int scx_task_init(u64 data_size);

/*
 * Kernel symbols that LAVD references but are not available in userspace.
 *
 * nr_cpu_ids: kernel global variable for number of possible CPUs.
 * Declared as "const extern volatile u32" in lavd.bpf.h; with -Dconst=
 * it becomes "extern volatile u32" (just a declaration). We provide the
 * definition here and lavd_setup() sets the value.
 *
 * cpufreq_cpu_data / hw_pressure: __ksym kernel symbols referenced in
 * power.bpf.c. Provide zero-initialized definitions to satisfy the linker.
 */
volatile u32 nr_cpu_ids;
struct cpufreq_policy *cpufreq_cpu_data;
unsigned long hw_pressure;

/*
 * CONFIG_NO_HZ_IDLE: __kconfig __weak bool used in sys_stat.bpf.c.
 * In BPF, this resolves to the kernel config; in simulation it's a
 * regular weak symbol. Without a definition, the weak symbol resolves
 * to address 0x0 in the -nostdlib .so, causing a SIGSEGV on access.
 * Set to false — the simulator doesn't model NO_HZ_IDLE.
 */
bool CONFIG_NO_HZ_IDLE;

/*
 * bpf_probe_read_kernel override for LAVD.
 *
 * The generic sim_wrapper.h implementation does memcpy(dst, src, sz) which
 * is normally fine. However, update_effective_capacity() in power.bpf.c
 * uses &cpufreq_cpu_data as an array base and indexes by CPU id. In BPF,
 * &cpufreq_cpu_data is NULL when the ksym doesn't exist, making the read
 * fail. In userspace, &cpufreq_cpu_data is always non-NULL, and reading
 * base[cpu] for cpu > 0 reads beyond the single variable into garbage
 * memory, causing SIGSEGV when the resulting pointer is dereferenced.
 *
 * Since there is no kernel memory in the simulator, bpf_probe_read_kernel
 * should always fail. This is safe — the only caller in LAVD is in
 * update_effective_capacity's cpufreq path which gracefully handles failure.
 */
#undef bpf_probe_read_kernel
#define bpf_probe_read_kernel(dst, sz, src) \
	(__builtin_memset((dst), 0, (sz)), (long)(-14))
/*
 * struct ravg_data -- running average data structure used in lavd.bpf.h.
 * Defined in scheds/include/lib/ravg.h inside #ifdef __BPF__, so it's
 * not available in userspace compilation. Provide the definition here.
 */
/*
 * ravg constants from ravg.h (inside #ifdef __BPF__).
 */
enum ravg_consts {
	RAVG_VAL_BITS = 44,
	RAVG_FRAC_BITS = 20,
};

struct ravg_data {
	u64 val;
	u64 val_at;
	u64 old;
	u64 cur;
};

/*
 * ravg helper functions and implementations.
 * These are defined inside #ifdef __BPF__ in ravg.h and implemented in
 * scx/lib/ravg.bpf.c. We need them for userspace compilation.
 */
#define RAVG_FN_ATTRS __attribute__((unused, always_inline))

static RAVG_FN_ATTRS void ravg_add(u64 *sum, u64 addend)
{
	u64 new = *sum + addend;
	if (new >= *sum)
		*sum = new;
	else
		*sum = -1;
}

static RAVG_FN_ATTRS inline u64 ravg_decay(u64 v, u32 shift)
{
	if (shift >= 64)
		return 0;
	else
		return v >> shift;
}

static RAVG_FN_ATTRS u32 ravg_normalize_dur(u32 dur, u32 half_life)
{
	if (dur < half_life)
		return (((u64)dur << RAVG_FRAC_BITS) + half_life - 1) /
			half_life;
	else
		return 1 << RAVG_FRAC_BITS;
}

#ifndef __arena
#define __arena
#endif

static RAVG_FN_ATTRS void ravg_transfer(struct ravg_data *base, u64 base_new_val,
					 struct ravg_data *xfer, u64 xfer_new_val,
					 u32 half_life, bool is_xfer_in)
{
	if ((s64)(base->val_at - xfer->val_at) < 0)
		ravg_accumulate(base, base_new_val, xfer->val_at, half_life);
	else if ((s64)(base->val_at - xfer->val_at) > 0)
		ravg_accumulate(xfer, xfer_new_val, base->val_at, half_life);

	if (is_xfer_in) {
		base->old += xfer->old;
		base->cur += xfer->cur;
	} else {
		if (base->old > xfer->old)
			base->old -= xfer->old;
		else
			base->old = 0;

		if (base->cur > xfer->cur)
			base->cur -= xfer->cur;
		else
			base->cur = 0;
	}
}

static RAVG_FN_ATTRS int ravg_to_arena(struct ravg_data __arena *to, struct ravg_data *from)
{
	*to = *from;
	return 0;
}

static RAVG_FN_ATTRS int ravg_from_arena(struct ravg_data *to, struct ravg_data __arena *from)
{
	*to = *from;
	return 0;
}

/*
 * Include ravg.bpf.c for ravg_accumulate, ravg_read, ravg_scale implementations.
 * Guard the header include since common.bpf.h is already included.
 */
#define __SCX_RAVG_BPF_H__  /* prevent ravg.h re-include */
#include "../../scx/lib/ravg.bpf.c"

/*
 * =================================================================
 * Include LAVD source files
 * =================================================================
 *
 * All 10 LAVD BPF source files are included as a single translation
 * unit. Header guards prevent re-inclusion. Order: utilities and
 * subsystems first, main last.
 *
 * NOTE: bpf_experimental.h declares kfuncs (bpf_task_from_pid,
 * bpf_cgroup_from_id, bpf_cgroup_release) as extern __ksym.
 * We must NOT use macro overrides for these -- instead we provide
 * weak function stubs after the includes.
 */
/*
 * sdt_task_defs.h is conditionally included under #ifdef __BPF__ in
 * sdt_task.h, but its constants (SDT_TASK_ENTS_PER_CHUNK) are used
 * unconditionally. Include it explicitly for the simulator.
 */
#include "sdt_task_defs.h"

#include "intf.h"

/*
 * Pre-include bpf_experimental.h to trigger its include guard, then
 * override can_loop and __cond_break which it defines using BPF-only
 * inline asm (.byte 0xe5 / may_goto). bpf_arena_common.bpf.h already
 * defines these correctly for SCX_BPF_UNITTEST but bpf_experimental.h
 * unconditionally redefines them.
 *
 * Also override the IRQ/NMI context checks (get_preempt_count,
 * bpf_in_hardirq, bpf_in_nmi, bpf_in_serving_softirq) which access
 * per-CPU kernel variables via bpf_this_cpu_ptr/bpf_core_field_exists.
 * The simulator routes these to Rust kfuncs that read per-CPU IRQ state.
 */
#include <bpf_experimental.h>
#undef can_loop
#define can_loop true
#undef __cond_break
#define __cond_break(expr) expr

extern unsigned int sim_bpf_in_hardirq(void);
extern unsigned int sim_bpf_in_nmi(void);
extern unsigned int sim_bpf_in_serving_softirq(void);
extern unsigned int sim_bpf_in_interrupt(void);

#define get_preempt_count() (sim_bpf_in_hardirq() ? 0x10000 : 0)
#define bpf_in_hardirq() sim_bpf_in_hardirq()
#define bpf_in_nmi() sim_bpf_in_nmi()
#define bpf_in_serving_softirq() sim_bpf_in_serving_softirq()
#define bpf_in_interrupt() sim_bpf_in_interrupt()

#include "util.bpf.c"
#include "power.bpf.c"
#include "sys_stat.bpf.c"
#include "lock.bpf.c"
#include "balance.bpf.c"
#include "idle.bpf.c"
#include "lat_cri.bpf.c"
#include "preempt.bpf.c"
#include "introspec.bpf.c"
#include "main.bpf.c"

/*
 * =================================================================
 * Post-include definitions
 * =================================================================
 */

/*
 * Per-CPU context array and map lookup (struct cpu_ctx now available).
 */
static struct cpu_ctx percpu_ctx[MAX_SIM_CPUS];

#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
/*
 * Phase 2: forward decls for cgroup_bw library timer-map short-circuit
 * (full backing storage is below; the actual map_ptr values are
 * populated by lavd_register_cbw_maps after cgroup_bw.bpf.c is
 * included). Declared here so lavd_map_lookup can reference them.
 */
extern char cbw_replenish_timer_storage[256];
extern char cbw_accounting_timer_storage[256];
extern void *cbw_replenish_timer_map_ptr;
extern void *cbw_accounting_timer_map_ptr;
#endif

static void *lavd_map_lookup(void *map, const void *key)
{
	if (map == lavd_cpu_ctx_stor_ptr && lavd_cpu_ctx_stor_ptr) {
		int cpu = sim_bpf_get_smp_processor_id();
		if (cpu >= 0 && cpu < MAX_SIM_CPUS)
			return &percpu_ctx[cpu];
		return NULL;
	}
	if (map == lavd_update_timer_map_ptr && lavd_update_timer_map_ptr)
		return lavd_update_timer_buf;
#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
	/* Phase 2: cgroup_bw library timer maps short-circuit to static
	 * single-entry storage so `bpf_map_lookup_elem(&replenish_timer,
	 * &key=0)` and similarly for accounting_timer return real
	 * `struct {bpf_timer}` slots that the library can hand to
	 * bpf_timer_init/set_callback/start. */
	if (map == cbw_replenish_timer_map_ptr && cbw_replenish_timer_map_ptr)
		return &cbw_replenish_timer_storage[0];
	if (map == cbw_accounting_timer_map_ptr && cbw_accounting_timer_map_ptr)
		return &cbw_accounting_timer_storage[0];
#endif
	return scx_test_map_lookup_elem(map, key);
}

/*
 * Per-CPU context lookup (definition after struct cpu_ctx is available).
 */
static struct cpu_ctx *lavd_lookup_percpu_elem(int cpu)
{
	if (cpu < 0 || cpu >= MAX_SIM_CPUS)
		return NULL;
	return &percpu_ctx[cpu];
}

/*
 * Register BPF maps with the test map infrastructure.
 */
static struct scx_test_map cpu_ctx_test_map;

#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
/*
 * Phase 2 BPF map glue: register cgroup_bw's 5 maps so that
 * `bpf_map_lookup_elem(&replenish_timer, &key)`,
 * `bpf_map_lookup_elem(&accounting_timer, &key)`, and the per-CPU
 * `tree_levels_map` lookup return real backing storage. Without
 * registration, scx_cgroup_bw_lib_init bails out at the first
 * `bpf_map_lookup_elem` call with a "Failed to lookup ..." cbw_err.
 *
 * - replenish_timer / accounting_timer: BPF_MAP_TYPE_ARRAY with
 *   max_entries=1, value=struct {bpf_timer}. Single entry suffices.
 * - tree_levels_map: BPF_MAP_TYPE_PERCPU_ARRAY with max_entries=1.
 *   Phase 1 item 6 wired bpf_map_lookup_percpu_elem to scxsim's
 *   percpu test map registry; we just need to register the map.
 * - cbw_cgrp_map (CGRP_STORAGE) and cbw_cgrp_llc_map (HASH): wired
 *   via scx_test_map registration so scx_test_cgrp_storage_get and
 *   scx_test_map_update_elem find their value-sizes.
 */
static struct scx_test_map cbw_cgrp_test_map;
static struct scx_test_map cbw_cgrp_llc_test_map;
static struct scx_test_map cbw_replenish_timer_test_map;
static struct scx_test_map cbw_accounting_timer_test_map;
static struct scx_percpu_test_map *cbw_tree_levels_test_map;

/* Static backing for the single-entry ARRAY maps so
 * `bpf_map_lookup_elem(&replenish_timer, &key=0)` returns a real
 * `struct {bpf_timer}` slot the library can write into. char[256]
 * is generous: `struct bpf_timer` is ~64 bytes; the library's
 * `struct replenish_timer { struct bpf_timer timer; }` and
 * `struct accounting_timer { struct bpf_timer timer; }` fit easily.
 * Using char[] sidesteps the forward-declaration problem (the structs
 * live inside cgroup_bw.bpf.c which is included LATER in this TU). */
char cbw_replenish_timer_storage[256] __attribute__((aligned(16)));
char cbw_accounting_timer_storage[256] __attribute__((aligned(16)));

/* Pointers used by lavd_map_lookup() to short-circuit the cgroup_bw
 * timer maps to the static backing arrays above (mirrors how
 * `lavd_update_timer_map_ptr` short-circuits LAVD's own update_timer).
 * Defined here for visibility to lavd_map_lookup; populated by
 * lavd_register_maps which runs AFTER cgroup_bw.bpf.c is included
 * (so `&replenish_timer` / `&accounting_timer` are in scope). */
void *cbw_replenish_timer_map_ptr;
void *cbw_accounting_timer_map_ptr;
#endif /* SCXSIM_PHASE2_REAL_CGROUP_BW */

void lavd_register_maps(void)
{
	scx_test_map_clear_all();

	INIT_SCX_TEST_MAP(&cpu_ctx_test_map, cpu_ctx_stor);
	scx_test_map_register(&cpu_ctx_test_map, &cpu_ctx_stor);

	lavd_cpu_ctx_stor_ptr = (void *)&cpu_ctx_stor;
	lavd_update_timer_map_ptr = (void *)&update_timer;

	/*
	 * Phase 1 BPF infra scale-up items 1+2: clear the multi-timer
	 * slot table so consecutive simulation runs start clean.
	 * Without this, slots leak across runs and a re-registered
	 * (struct bpf_timer *) might reuse a stale slot's callback,
	 * destroying determinism.
	 */
	__builtin_memset(lavd_timer_table, 0, sizeof(lavd_timer_table));

	/*
	 * Phase 2: cgroup_bw's BPF maps live in cgroup_bw.bpf.c which is
	 * included later in this TU. Their registration is split out into
	 * `lavd_register_cbw_maps()` (defined post-include) and called
	 * from lavd_setup after lavd_register_maps. Whole path is
	 * gated on `SCXSIM_PHASE2_REAL_CGROUP_BW`.
	 */
}

#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
/* Forward declaration; full def lives after the cgroup_bw.bpf.c include. */
static void lavd_register_cbw_maps(void);
#endif

/*
 * Fire the stored BPF timer callback for the given `slot`.
 *
 * Called from the Rust engine when an `EventKind::TimerFired { slot }`
 * fires. Phase 1 BPF infra scale-up items 1+2: dispatches by slot id
 * so that LAVD's `update_timer` (slot 0 by convention -- whichever
 * the BPF source registers first via `bpf_timer_init`) and Phase 2's
 * compiled-in cgroup_bw `cbw_replenish_timer` can fire independently
 * without aliasing.
 */
void lavd_fire_timer(unsigned int slot)
{
	int key = 0;
	if (slot >= LAVD_MAX_BPF_TIMERS)
		return;
	if (lavd_timer_table[slot].timer_cb && lavd_timer_table[slot].timer_ptr) {
		lavd_timer_table[slot].timer_cb(
			lavd_timer_table[slot].timer_map,
			&key,
			lavd_timer_table[slot].timer_ptr);
	}
}

/*
 * Cgroup bandwidth control: redirect to engine-owned BandwidthManager.
 *
 * Diff 4/5 of the cgroup_bw stack — these wrappers used to return 0
 * unconditionally, which left LAVD's cgroup-bw view incoherent with the
 * engine-side enforcement loop wired up by Diff 3 (cf93b57). Now they
 * forward to sim_cgroup_bw_* shims defined in
 * crates/scx_simulator/src/unsafe_impl/cgroup_bw_ffi.rs which talk to
 * the same BandwidthManager the engine uses for admission gating,
 * charging, and refill. That gives the dual-controller surface (kernel
 * cpu.max x LAVD cgroup-bw) a single source of truth for the H6
 * Bug-1 reproducer.
 */
/*
 * Phase 2 ON  (`-DSCXSIM_PHASE2_REAL_CGROUP_BW=1`): the cgroup_bw
 *             library compiled in below provides STRONG definitions of
 *             `scx_cgroup_bw_is_cgroup_throttled` and
 *             `scx_cgroup_bw_is_task_throttled`; the engine-side
 *             `sim_cgroup_bw_is_cgroup_throttled` / `_is_task_throttled`
 *             shims (in `cgroup_bw_ffi.rs`) become unused.
 *
 * Phase 2 OFF (default for now): we keep the pre-Phase-2 forwarders
 *             that delegate into the engine `BandwidthManager`. They
 *             can't co-exist with the library's strong defs because
 *             they're non-weak; gating on the build-time switch is
 *             how Phase 2 lands without breaking the default
 *             regression baseline (canonical Bug-1 reproducer keeps
 *             firing the watchdog at runnable_for_ns=80000793).
 */
#ifndef SCXSIM_PHASE2_REAL_CGROUP_BW
extern int sim_cgroup_bw_is_cgroup_throttled(u64 cgrp_id);
extern int sim_cgroup_bw_is_task_throttled(u64 taskc_ptr);

int scx_cgroup_bw_is_cgroup_throttled(u64 cgrp_id)
{
	return sim_cgroup_bw_is_cgroup_throttled(cgrp_id);
}

int scx_cgroup_bw_is_task_throttled(u64 taskc_ptr)
{
	return sim_cgroup_bw_is_task_throttled(taskc_ptr);
}
#endif /* !SCXSIM_PHASE2_REAL_CGROUP_BW */

/*
 * =================================================================
 * Kfunc stubs for BPF experimental functions
 * =================================================================
 *
 * These functions are declared as extern __ksym in bpf_experimental.h.
 * Macro overrides would corrupt the declarations, so we provide
 * function implementations instead.
 */

/*
 * bpf_task_from_pid — provided by the Rust kfuncs (kfuncs.rs).
 * The extern __ksym declaration from bpf_experimental.h resolves
 * to the simulator's real PID→task_struct lookup at runtime.
 */

/*
 * Cgroup lookup by ID -- delegates to the Rust cgroup registry.
 * Returns the struct cgroup pointer for the given cgroup ID,
 * or NULL if no registry is installed or the ID is not found.
 */
extern void *sim_cgroup_lookup_by_id(u64 cgid);

struct cgroup *bpf_cgroup_from_id(u64 cgroupid)
{
	return (struct cgroup *)sim_cgroup_lookup_by_id(cgroupid);
}

/* Cgroup reference release -- no-op */
void bpf_cgroup_release(struct cgroup *cgrp)
{
	(void)cgrp;
}

/*
 * =================================================================
 * Cgroup bandwidth stubs with resource tracking
 * =================================================================
 *
 * These stubs simulate BPF map resource limits. In production LAVD,
 * cgroup_bw_map has size CBW_NR_CGRP_MAX = 2048. When this map fills
 * up, scx_cgroup_bw_init fails with -ENOMEM.
 *
 * The simulator tracks cgroup BPF map entries in the Rust CgroupRegistry.
 * sim_cgroup_registry_allocate() returns 0 or -ENOMEM based on the
 * scenario's max_cgroups limit.
 */
extern int sim_cgroup_registry_allocate(void);
extern void sim_cgroup_registry_free(void);

/*
 * Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`): the
 * production `scx/lib/cgroup_bw.bpf.c` is compiled into this TU below
 * (after `main.bpf.c`). It provides STRONG definitions for every
 * `scx_cgroup_bw_*` entry point, so the weak forwarders previously
 * defined here -- which delegated into Diff 4/5's
 * `crates/scx_simulator/src/unsafe_impl/cgroup_bw_ffi.rs` shims
 * (`sim_cgroup_bw_*`) -- are gone. The engine no longer drives a
 * second BandwidthManager state machine; the library is the single
 * source of truth.
 *
 * `sim_cgroup_registry_allocate` / `_free` are retained for the
 * scenario-side ENOMEM exhaustion modeling -- those are wired in
 * from the engine when allocating implicit cgroups, NOT from
 * scx_cgroup_bw_init (which now lives in the library).
 */
#ifndef SCXSIM_PHASE2_REAL_CGROUP_BW  /* Phase 2: replaced by strong defs in cgroup_bw.bpf.c below */
extern int sim_cgroup_bw_lib_init(void);
extern int sim_cgroup_bw_init(void *cgrp);
extern int sim_cgroup_bw_exit(void *cgrp);
extern int sim_cgroup_bw_set(void *cgrp, u64 period_us, u64 quota_us, u64 burst_us);
extern int sim_cgroup_bw_throttled(void *cgrp);
extern int sim_cgroup_bw_consume(void *cgrp, u64 runtime_ns);
extern int sim_cgroup_bw_put_aside(void *p, u64 taskc, u64 vtime, void *cgrp);
extern int sim_cgroup_bw_reenqueue(void);
extern int sim_cgroup_bw_cancel(u64 taskc);
extern int sim_cgroup_bw_move(void *p, u64 taskc, void *from, void *to);
extern int sim_cgroup_bw_dump(u64 cgrp_id, bool descendant, bool accurate, bool indent);

__attribute__((weak)) int scx_cgroup_bw_lib_init(
	struct scx_cgroup_bw_config *config)
{
	(void)config;
	return sim_cgroup_bw_lib_init();
}

__attribute__((weak)) int scx_cgroup_bw_init(
	struct cgroup *cgrp, struct scx_cgroup_init_args *args)
{
	int rc;
	(void)args;
	/*
	 * Attempt to allocate a BPF map entry. Returns -ENOMEM if the
	 * scenario's max_cgroups limit has been reached. Failing here
	 * preserves Diff 1's resource-exhaustion semantics; the engine-side
	 * sim_cgroup_bw_init shim does not touch BandwidthManager state
	 * because BandwidthManager only tracks cgroups with finite quota
	 * (configured via _set, not _init).
	 */
	rc = sim_cgroup_registry_allocate();
	if (rc)
		return rc;
	return sim_cgroup_bw_init((void *)cgrp);
}

__attribute__((weak)) int scx_cgroup_bw_exit(struct cgroup *cgrp)
{
	(void)sim_cgroup_bw_exit((void *)cgrp);
	/*
	 * Free the BPF map entry allocated in scx_cgroup_bw_init.
	 */
	sim_cgroup_registry_free();
	return 0;
}

__attribute__((weak)) int scx_cgroup_bw_set(
	struct cgroup *cgrp, u64 period, u64 quota, u64 burst)
{
	return sim_cgroup_bw_set((void *)cgrp, period, quota, burst);
}

/*
 * cgroup_bw API version split.
 *
 * Between scx commits a08c9e272b (Apr 23 2026, OLD API) and 6f4921a6c6
 * (Apr 30 2026, NEW API), three of these functions changed signature:
 *
 *   OLD:  scx_cgroup_bw_throttled (struct cgroup *cgrp,
 *                                  struct task_struct *p)
 *   NEW:  scx_cgroup_bw_throttled (u64 cgrp_id,
 *                                  struct task_struct *p, u64 taskc)
 *
 *   OLD:  scx_cgroup_bw_consume   (struct cgroup *cgrp, u64 runtime)
 *   NEW:  scx_cgroup_bw_consume   (u64 cgrp_id, u64 consumed_ns,
 *                                  u64 taskc_raw)
 *
 *   OLD:  scx_cgroup_bw_put_aside (struct task_struct *p, u64 taskc,
 *                                  u64 vtime, struct cgroup *cgrp)
 *   NEW:  scx_cgroup_bw_put_aside (struct task_struct *p, u64 ctx,
 *                                  u64 vtime, u64 cgrp_id)
 *
 * The remaining scx_cgroup_bw_* entry points (lib_init, init, exit, set,
 * reenqueue, cancel, move, dump, is_cgroup_throttled, is_task_throttled)
 * kept their signatures across the flag-day.
 *
 * `SCX_CGROUP_BW_NEW_API` is defined to 1 by `schedulers/Makefile` when
 * the scx submodule's `scheds/include/lib/cgroup.h` defines
 * `struct scx_task_cgroup_bw` (the NEW-API marker). Without that define,
 * the OLD-API signatures are emitted (matching scx pre-Apr-30-2026 SHAs).
 *
 * Both branches dispatch into the same `sim_cgroup_bw_*` Rust shims that
 * take `struct cgroup *`; the NEW-API branch resolves cgrp_id back to a
 * `struct cgroup *` via `sim_cgroup_lookup_by_id` (the registry's
 * id->ptr lookup). This preserves identical engine-side semantics across
 * both API versions for the cpu-bw-stall-bug scx-version matrix. See tg
 * task `fix-wrapper-c-old-new-cgroup-bw-api-conditional` for the full
 * rationale and the v2-matrix verification recipe.
 */
#if defined(SCX_CGROUP_BW_NEW_API) && SCX_CGROUP_BW_NEW_API

__attribute__((weak)) int scx_cgroup_bw_throttled(u64 cgrp_id,
					   struct task_struct *p, u64 taskc)
{
	struct cgroup *cgrp;
	(void)p;
	(void)taskc;
	cgrp = (struct cgroup *)sim_cgroup_lookup_by_id(cgrp_id);
	return sim_cgroup_bw_throttled((void *)cgrp);
}

__attribute__((weak)) int scx_cgroup_bw_consume(
	u64 cgrp_id, u64 consumed_ns, u64 taskc_raw)
{
	struct cgroup *cgrp;
	(void)taskc_raw;
	cgrp = (struct cgroup *)sim_cgroup_lookup_by_id(cgrp_id);
	return sim_cgroup_bw_consume((void *)cgrp, consumed_ns);
}

__attribute__((weak)) int scx_cgroup_bw_put_aside(
	struct task_struct *p, u64 ctx, u64 vtime, u64 cgrp_id)
{
	struct cgroup *cgrp;
	cgrp = (struct cgroup *)sim_cgroup_lookup_by_id(cgrp_id);
	return sim_cgroup_bw_put_aside((void *)p, ctx, vtime, (void *)cgrp);
}

#else /* OLD API: pre-Apr-30-2026 scx SHAs */

__attribute__((weak)) int scx_cgroup_bw_throttled(struct cgroup *cgrp,
					   struct task_struct *p)
{
	(void)p;
	return sim_cgroup_bw_throttled((void *)cgrp);
}

__attribute__((weak)) int scx_cgroup_bw_consume(
	struct cgroup *cgrp, u64 runtime)
{
	return sim_cgroup_bw_consume((void *)cgrp, runtime);
}

__attribute__((weak)) int scx_cgroup_bw_put_aside(
	struct task_struct *p, u64 taskc, u64 vtime, struct cgroup *cgrp)
{
	return sim_cgroup_bw_put_aside((void *)p, taskc, vtime, (void *)cgrp);
}

#endif /* SCX_CGROUP_BW_NEW_API */

__attribute__((weak)) int scx_cgroup_bw_reenqueue(void)
{
	return sim_cgroup_bw_reenqueue();
}

__attribute__((weak)) int scx_cgroup_bw_cancel(u64 taskc)
{
	return sim_cgroup_bw_cancel(taskc);
}

__attribute__((weak)) int scx_cgroup_bw_move(
	struct task_struct *p, u64 taskc,
	struct cgroup *from, struct cgroup *to)
{
	return sim_cgroup_bw_move((void *)p, taskc, (void *)from, (void *)to);
}

__attribute__((weak)) int scx_cgroup_bw_dump(
	u64 cgrp_id, bool descendent, bool accurate, bool indent)
{
	return sim_cgroup_bw_dump(cgrp_id, descendent, accurate, indent);
}
#endif /* Phase 2: weak forwarders deactivated */

#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
/*
 * =================================================================
 * Phase 2: compile in scx/lib/cgroup_bw.bpf.c
 * =================================================================
 *
 * tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`. The
 * production cgroup-bandwidth library is compiled as part of this
 * translation unit AFTER the LAVD source includes (so `scx_cgroup_bw_*`
 * call sites inside main.bpf.c bind to the real library symbols, not
 * the weak shims above) and AFTER scxsim's own glue (so the macro
 * overrides for arena_*, cast_*, BPF map declarations, and topology
 * stubs are in scope when the library expands).
 *
 * Glue provided here:
 *
 *   - `arena_spinlock_t` -> `int` and `arena_spin_lock/_unlock` -> no-op.
 *     The simulator is single-threaded; the BPF arena spinlock has no
 *     contention to model. Phase 3 (`design-and-implement-stochastic-
 *     timer-interleaving-mode-for-scxsim-phase3`) is where genuine
 *     timing-race coverage will live; until then no-op is correct.
 *
 *   - `cast_kern(p)` / `cast_user(p)` -> identity. The arena address-
 *     space casts that LLVM emits in real BPF compilation are
 *     unnecessary in userspace -- the pointers are plain x86_64.
 *
 *   - `nr_topo_nodes[TOPO_LLC-1] = 1` and `topo_cpu_to_llc_id` returning
 *     0. cgroup_bw walks per-LLC backlogs (`bpf_for(i, 0, TOPO_NR(LLC))`
 *     at lib/cgroup_bw.bpf.c:656, 727, 1119, 1623, 1886, 2145, 2492);
 *     scxsim is single-LLC for the cpu-bw-stall-bug reproducer (see
 *     `experiments/lavd_cpubw_stalls_202604/SCXSIM_REAL_CGROUP_BW_LIBRARY_DESIGN.md`
 *     section Phase 2 item 3). Multi-LLC is a Phase-2-followup if
 *     needed.
 *
 *   - `scx_bpf_error` -> `bpf_printk` so library-internal "BUG:"
 *     messages surface in the printk pipeline rather than aborting
 *     the simulator (which is not a verifier-style failure mode).
 *
 *   - `bpf_rcu_read_lock/unlock` -> no-op. Single-threaded sim.
 *
 *   - `bpf_core_field_exists(x)` -> 1. CO-RE field existence is a
 *     compile-time concept in production BPF; in scxsim every field
 *     declared in vmlinux.h is present.
 *
 *   - `bpf_probe_read_kernel_str` -> bounded strncpy. Used by cgroup_bw
 *     for cgroup-name diagnostics in dump paths only.
 */

#include <bpf_arena_common.bpf.h>

/* arena_spin_lock semantics: real BPF uses bpf_arena_spin_lock.h's
 * qspinlock; we override with no-op pre-include since the simulator
 * is single-threaded. Must come BEFORE lib/cgroup_bw.bpf.c is
 * included (which #includes <bpf_arena_spin_lock.h>). */
#define _BPF_ARENA_SPIN_LOCK_H 1  /* prevent the real header inclusion */
#define arena_spinlock_t int
#define arena_spin_lock(lock) ({ (void)(lock); 0; })
#define arena_spin_unlock(lock) ((void)(lock))
#define arena_spin_trylock(lock) ({ (void)(lock); 0; })

/*
 * bpf_map_delete_elem: same helper-defs-pointer pattern as
 * bpf_cgrp_storage_get below. scxsim already overrides
 * bpf_map_lookup_elem and bpf_map_update_elem via
 * `lib/scxtest/scx_test_map.h`, but not bpf_map_delete_elem. Route to
 * scx_test_map_delete_elem (Phase 1 item 5).
 */
extern int scx_test_map_delete_elem(void *map, const void *key);
#undef bpf_map_delete_elem
#define bpf_map_delete_elem(map, key) scx_test_map_delete_elem((map), (key))

/*
 * bpf_cgrp_storage_get / _delete: ROOT CAUSE of the post-lavd_init
 * SIGSEGV PC=0xd2 in early Stage B testing. The libbpf helper-defs
 * header `bpf_helper_defs.h` declares these as static function pointers
 * initialized to the helper ID number:
 *
 *   static long (*bpf_cgrp_storage_get)(struct bpf_map *, struct cgroup *,
 *                                        void *, __u64) = (void *) 210;
 *   static long (*bpf_cgrp_storage_delete)(struct bpf_map *, struct cgroup *)
 *                                        = (void *) 211;
 *
 * In a real BPF program these "calls" become BPF_CALL insns the verifier
 * lowers to kernel helper invocations. In our userspace .so, the call
 * site loads the literal 210 (= 0xd2) into a register and `call *rax`s
 * it -- straight into a NULL-page SIGSEGV. The Rust kfuncs.rs strong
 * symbols never get a chance to resolve because the compiler emitted a
 * constant load, not an external symbol reference.
 *
 * Fix: macro-override BEFORE cgroup_bw.bpf.c is included so the call
 * site bypasses the helper-ID pointer entirely and routes directly to
 * scxsim's scx_test_cgrp_storage_get / _delete (Phase 1 item 5).
 */
extern void *scx_test_cgrp_storage_get(void *map, const void *cgrp_ptr_loc,
				       void *value, unsigned long flags);
extern int scx_test_cgrp_storage_delete(void *map, const void *cgrp_ptr_loc);
#undef bpf_cgrp_storage_get
#define bpf_cgrp_storage_get(map, cgrp, value, flags) \
	scx_test_cgrp_storage_get((map), (const void *)&(cgrp), (value), (flags))
#undef bpf_cgrp_storage_delete
#define bpf_cgrp_storage_delete(map, cgrp) \
	scx_test_cgrp_storage_delete((map), (const void *)&(cgrp))

/* bpf_iter_css_*: route LAVD's `bpf_for_each(css, pos, root, flags)`
 * to scxsim's Phase 1 item 3 flags-aware iterator (sim_bpf_iter_css_*
 * in csrc/sim_cgroup.c). Without these overrides, dlopen leaves
 * bpf_iter_css_new / _next / _destroy as unresolved-weak (= NULL)
 * because the wrapper.c hasn't otherwise installed them; the
 * library's first `bpf_for_each(css, ...)` call (e.g. inside
 * cbw_cgroup_bw_throttled's per-LLC BTQ walk) jumps to NULL. */
extern int sim_bpf_iter_css_new(struct bpf_iter_css *it,
				struct cgroup_subsys_state *start,
				unsigned int flags);
extern struct cgroup_subsys_state *sim_bpf_iter_css_next(struct bpf_iter_css *it);
extern void sim_bpf_iter_css_destroy(struct bpf_iter_css *it);
#undef bpf_iter_css_new
#define bpf_iter_css_new(it, start, flags) sim_bpf_iter_css_new((it), (start), (flags))
#undef bpf_iter_css_next
#define bpf_iter_css_next(it) sim_bpf_iter_css_next(it)
/*
 * IMPORTANT: bpf_iter_css_destroy must be an OBJECT-like macro (no
 * parenthesized parameter list) -- not a function-like macro. The
 * libbpf `bpf_for_each(type, cur, args...)` macro at
 * `<bpf/bpf_helpers.h>:380` references the destroy as a bare
 * identifier inside `__attribute__((cleanup(bpf_iter_css_destroy)))`
 * (no `(`), so a function-like macro definition would NOT expand and
 * cleanup would call the helper-defs NULL pointer at scope exit.
 * Object-like substitution makes the cleanup attribute reference
 * `sim_bpf_iter_css_destroy` directly -- which is the real function.
 */
#undef bpf_iter_css_destroy
#define bpf_iter_css_destroy sim_bpf_iter_css_destroy

/* scx_atq_lock / scx_atq_unlock: declared as `static __always_inline`
 * inside `scx/scheds/include/lib/atq.h` BUT only under `#ifdef __BPF__`.
 * In our userspace compilation those declarations are not visible, so
 * cgroup_bw.bpf.c's call sites generate external references that fail
 * to resolve at .so dlopen. Provide function-style macro wrappers that
 * expand to a no-op (single-threaded simulator -- the spinlock has no
 * contention to model). Phase 3
 * (`design-and-implement-stochastic-timer-interleaving-mode-for-scxsim-phase3`)
 * is the right place to model the CAS race the lock guards. */
#define scx_atq_lock(atq)   ({ (void)(atq); 0; })
#define scx_atq_unlock(atq) ((void)(atq))

/* Kernel SMP memory-order barriers: cgroup_bw.bpf.c uses
 * smp_load_acquire / smp_store_release / READ_ONCE / WRITE_ONCE on
 * `cbw_backlog_stat` to coordinate timer-vs-dispatch CAS. The
 * single-threaded simulator has no concurrent observers, so plain
 * loads / stores are correct under sequential semantics. Phase 3 will
 * revisit if stochastic-interleaving needs to expose the race. */
#ifndef smp_load_acquire
#define smp_load_acquire(p)        (*(volatile typeof(*(p)) *)(p))
#endif
#ifndef smp_store_release
#define smp_store_release(p, v)    do { *(volatile typeof(*(p)) *)(p) = (v); } while (0)
#endif
#ifndef READ_ONCE
#define READ_ONCE(x)               (*(volatile typeof(x) *)&(x))
#endif
#ifndef WRITE_ONCE
#define WRITE_ONCE(x, val)         do { *(volatile typeof(x) *)&(x) = (val); } while (0)
#endif
#ifndef smp_mb
#define smp_mb() __asm__ __volatile__("" ::: "memory")
#endif

/* `scx_atq_create(fifo)` is a macro inside `lib/atq.h` under
 * `#ifdef __BPF__`. The function declaration of
 * `scx_atq_create_internal` lives in `lib/scxtest/overrides.h` (which
 * is transitively included). We just need to add the macro so
 * cgroup_bw.bpf.c's `scx_atq_create(false)` (lib/cgroup_bw.bpf.c:610)
 * expands to a call into csrc/sim_atq.c. */
#ifndef scx_atq_create
#define scx_atq_create(fifo) scx_atq_create_internal((fifo), (unsigned long)-1)
#endif

/* cast_kern / cast_user identity. These macros are no-ops in production
 * BPF when LLVM has __BPF_FEATURE_ADDR_SPACE_CAST, which is the case
 * for the toolchain scxsim builds with -- but bpf_arena_common.bpf.h
 * conditionally redefines them to inline asm if the feature is missing.
 * Force the no-op definition to keep the userspace path simple. */
#undef cast_kern
#define cast_kern(ptr) /* nop */
#undef cast_user
#define cast_user(ptr) /* nop */

/* RCU read lock no-ops. */
#define bpf_rcu_read_lock()   ((void)0)
#define bpf_rcu_read_unlock() ((void)0)

/* CO-RE field existence: scxsim's task_struct (from vmlinux.h) is the
 * real kernel layout, so every field cgroup_bw probes via
 * `bpf_core_field_exists` is in fact present. Override to return 1.
 *
 * NOTE: this also flips LAVD's `scx_lib_init` check (probes whether
 * `task_struct::migration_disabled` exists). With the override -> 1,
 * scx_lib_init proceeds to dereference
 * `bpf_get_current_task_btf()->migration_disabled`. scxsim's synthetic
 * idle task is a zero-filled `struct task_struct`, so reading any
 * field returns 0 -- safe. Phase 1 multi-timer + Phase 2 stub mode
 * tests confirm no crash from this path.
 */
#undef bpf_core_field_exists
#define bpf_core_field_exists(...) 1

/* `bpf_probe_read_kernel_str` is already provided by `csrc/sim_wrapper.h`
 * (included transitively); no override needed here. */

/* scx_bpf_error -> stderr (don't abort sim). */
extern int dprintf(int fd, const char *fmt, ...);
#undef scx_bpf_error
#define scx_bpf_error(fmt, ...) dprintf(2, "scx_bpf_error: " fmt "\n", ##__VA_ARGS__)

/* bpf_printk override.
 *
 * Production BPF: `bpf_printk(fmt, args...)` expands (via
 * `<bpf/bpf_helpers.h>`) to `__bpf_printk` which calls
 * `bpf_trace_printk(__fmt, sizeof(__fmt), ##args)`. Both
 * `bpf_trace_printk` and `bpf_trace_vprintk` are kernel BPF helpers
 * resolved by the verifier; they have NO userspace counterpart in
 * scxsim, so dlopen leaves them as undefined-weak (resolved to NULL
 * function pointers). The first cgroup_bw `cbw_err` / `cbw_dbg` call
 * therefore SIGSEGVs the simulator with `PC ~= 0`.
 *
 * Phase 2 fix: short-circuit `bpf_printk` to `dprintf(2, ...)` so the
 * library's diagnostics surface in the existing `[LAVD-PRINTK]` stderr
 * stream rather than crashing. The format-string variadic chain is
 * portable to glibc dprintf -- the BPF-helper wrapping just adds an
 * unused length argument.
 */
#undef bpf_printk
#define bpf_printk(fmt, ...) dprintf(2, "[LAVD-PRINTK] " fmt "\n", ##__VA_ARGS__)

/* Topology stubs: single-LLC simulator. cgroup_bw uses TOPO_NR(LLC)
 * to size per-LLC backlog walks. Pull in lib/topology.h for the
 * TOPO_MAX_LEVEL constant; the stubs must be visible BEFORE
 * cgroup_bw.bpf.c is included. */
#include <lib/topology.h>
int nr_topo_nodes[TOPO_MAX_LEVEL] = {1, 1, 1, 1, 1};
int topo_cpu_to_llc_id(u32 cpu) { (void)cpu; return 0; }

/* Map registration glue: cgroup_bw declares cbw_cgrp_map (CGRP_STORAGE),
 * cbw_cgrp_llc_map (HASH), tree_levels_map (PERCPU_ARRAY), and the
 * two timer maps (replenish_timer, accounting_timer). Forward-declare
 * the map symbol names so we can register them via INIT_SCX_TEST_MAP
 * after the library is included. */

/*
 * The actual library inclusion. Order matters:
 *   - main.bpf.c (LAVD) is already included above; its scx_cgroup_bw_*
 *     call sites are still pointing at the weak wrapper.c shims here
 *     because the strong defs from cgroup_bw.bpf.c haven't been seen
 *     yet. The linker resolves call sites AFTER the whole TU is
 *     compiled, so the strong defs win.
 *   - cgroup_bw.bpf.c declares its own scx_cgroup_bw_* functions
 *     non-weak, so the linker promotes them and demotes the weak ones.
 *   - cgroup_bw.bpf.c calls scx_cgroup_bw_enqueue_cb -- defined by
 *     LAVD's REGISTER_SCX_CGROUP_BW_ENQUEUE_CB macro (main.bpf.c:2302).
 *   - cgroup_bw.bpf.c calls scx_atq_* -- resolves to csrc/sim_atq.c
 *     (Phase 1 item 7) at .so dlopen via -rdynamic.
 */
#include "../../scx/lib/cgroup_bw.bpf.c"

/*
 * Phase 2: register cgroup_bw's BPF maps with scx_test_map. Must be
 * defined AFTER the include so the map symbols (replenish_timer,
 * accounting_timer, cbw_cgrp_map, cbw_cgrp_llc_map, tree_levels_map)
 * and value-types (struct replenish_timer, struct accounting_timer,
 * struct scx_cgroup_ctx, struct scx_cgroup_llc_ctx, struct tree_levels)
 * are in scope.
 */
static void lavd_register_cbw_maps(void)
{
	cbw_replenish_timer_map_ptr = (void *)&replenish_timer;
	cbw_accounting_timer_map_ptr = (void *)&accounting_timer;
	__builtin_memset(cbw_replenish_timer_storage, 0,
			 sizeof(cbw_replenish_timer_storage));
	__builtin_memset(cbw_accounting_timer_storage, 0,
			 sizeof(cbw_accounting_timer_storage));

	/* CGRP_STORAGE: keyed by struct cgroup *, value = scx_cgroup_ctx.
	 * BPF_MAP_TYPE_CGRP_STORAGE has no max_entries field; use the
	 * TASK_STORAGE-style init which hardcodes max_entries = 100 (the
	 * scxsim cap on simulated cgroups; bumped via Phase-1 SDT scale-up
	 * if needed). */
	INIT_SCX_TEST_MAP_FROM_TASK_STORAGE(&cbw_cgrp_test_map, cbw_cgrp_map);
	scx_test_map_register(&cbw_cgrp_test_map, &cbw_cgrp_map);

	/* HASH: keyed by cgroup_llc_id, value = scx_cgroup_llc_ctx. */
	INIT_SCX_TEST_MAP(&cbw_cgrp_llc_test_map, cbw_cgrp_llc_map);
	scx_test_map_register(&cbw_cgrp_llc_test_map, &cbw_cgrp_llc_map);

	/* PERCPU_ARRAY tree_levels_map: keyed by u32, value = struct tree_levels.
	 * Allocate per-CPU storage; MAX_SIM_CPUS is the simulator ceiling. */
	cbw_tree_levels_test_map = scx_alloc_percpu_test_map(MAX_SIM_CPUS);
	INIT_SCX_PERCPU_TEST_MAP(cbw_tree_levels_test_map, tree_levels_map);
	scx_register_percpu_test_map(cbw_tree_levels_test_map,
				     &tree_levels_map);
}
#endif /* SCXSIM_PHASE2_REAL_CGROUP_BW */

/*
 * =================================================================
 * Setup function
 * =================================================================
 *
 * Called from Rust before lavd_init() to initialize globals,
 * register maps, and install the SIGFPE handler.
 */
extern int dprintf(int fd, const char *fmt, ...);
void lavd_setup(unsigned int num_cpus)
{
	unsigned int cpu;

	dprintf(2, "[PHASE2-DBG] lavd_setup enter cpus=%u\n", num_cpus);
	/* Install SIGFPE handler for BPF div-by-zero semantics */
	sim_install_sigfpe_handler();
	dprintf(2, "[PHASE2-DBG] sigfpe installed\n");

	/* Initialize per-task arena storage for task_ctx */
	scx_task_init(sizeof(struct task_ctx));
	dprintf(2, "[PHASE2-DBG] scx_task_init done\n");

	/* Register maps */
	lavd_register_maps();
#ifdef SCXSIM_PHASE2_REAL_CGROUP_BW
	lavd_register_cbw_maps();
#endif
	dprintf(2, "[PHASE2-DBG] lavd_register_maps done\n");

	/* Core globals */
	nr_cpus_onln = num_cpus;
	nr_cpu_ids = num_cpus;
	nr_llcs = 1;
	is_smt_active = false;

	/*
	 * Power mode: default to performance (matches --performance flag).
	 * This keeps no_core_compaction=true and is_powersave_mode=false.
	 */
	power_mode = 0; /* LAVD_PM_PERFORMANCE */
	is_powersave_mode = false;

	/* Disable complex features for initial simulation */
	enable_cpu_bw = false;
	is_autopilot_on = false;
	no_core_compaction = true;
	no_freq_scaling = true;
	no_preemption = false;
	no_wake_sync = false;
	no_slice_boost = false;
	no_use_em = true; /* no kernel energy model in the simulator */
	verbose = 0;

	/* Per-CPU topology: uniform capacity, no big/little, no SMT */
	for (cpu = 0; cpu < num_cpus && cpu < LAVD_CPU_ID_MAX; cpu++) {
		cpu_capacity[cpu] = 1024;
		cpu_big[cpu] = 0;
		cpu_turbo[cpu] = 0;
		cpu_sibling[cpu] = cpu;
	}

	/*
	 * Set up a single compute domain with all CPUs.
	 * lavd_init() will call init_cpdoms() to create DSQs.
	 */
	{
		struct cpdom_ctx *cpdomc = &cpdom_ctxs[0];
		__builtin_memset(cpdomc, 0, sizeof(*cpdomc));
		cpdomc->id = 0;
		cpdomc->alt_id = 0;
		cpdomc->numa_id = 0;
		cpdomc->llc_id = 0;
		cpdomc->is_big = 0;
		cpdomc->is_valid = 1;
		cpdomc->nr_active_cpus = num_cpus;
		cpdomc->cap_sum_active_cpus = num_cpus * 1024;

		for (cpu = 0; cpu < num_cpus && cpu < LAVD_CPU_ID_MAX; cpu++)
			cpdomc->__cpumask[cpu / 64] |=
				(1ULL << (cpu % 64));
	}
	dprintf(2, "[PHASE2-DBG] lavd_setup exit\n");
}

/*
 * =================================================================
 * Multi-domain setup for load balancing coverage
 * =================================================================
 *
 * Reconfigures the compute domain topology created by lavd_setup()
 * into multiple domains with neighbor relationships. This enables
 * cross-domain migration code paths in balance.bpf.c.
 *
 * Must be called AFTER lavd_setup() and BEFORE lavd_init().
 *
 * Parameters:
 *   nr_domains: number of compute domains to create (must be >= 2)
 *
 * CPUs are split evenly across domains. Remaining CPUs go to the
 * last domain. All domains are neighbors of each other at distance 0.
 */
void lavd_setup_multi_domain(unsigned int nr_domains)
{
	unsigned int cpus_per_domain, cpu, d, i;

	if (nr_domains < 2 || nr_domains > LAVD_CPDOM_MAX_NR)
		return;
	if (nr_cpus_onln < nr_domains)
		return;

	cpus_per_domain = nr_cpus_onln / nr_domains;

	/* Clear domain 0 that lavd_setup() created */
	__builtin_memset(&cpdom_ctxs[0], 0, sizeof(cpdom_ctxs[0]));

	/* Create nr_domains domains with disjoint CPU sets */
	for (d = 0; d < nr_domains; d++) {
		struct cpdom_ctx *cpdomc = &cpdom_ctxs[d];
		unsigned int first_cpu = d * cpus_per_domain;
		unsigned int last_cpu = (d == nr_domains - 1)
			? nr_cpus_onln
			: first_cpu + cpus_per_domain;

		cpdomc->id = d;
		cpdomc->alt_id = (d == 0) ? 1 : 0;
		cpdomc->numa_id = d;
		cpdomc->llc_id = d;
		cpdomc->is_big = 0;
		cpdomc->is_valid = 1;
		cpdomc->nr_active_cpus = last_cpu - first_cpu;
		cpdomc->cap_sum_active_cpus =
			cpdomc->nr_active_cpus * 1024;

		/* Set cpumask for this domain's CPUs */
		for (cpu = first_cpu;
		     cpu < last_cpu && cpu < LAVD_CPU_ID_MAX;
		     cpu++) {
			cpdomc->__cpumask[cpu / 64] |=
				(1ULL << (cpu % 64));
		}

		/*
		 * All other domains are neighbors at distance 0.
		 * This enables cross-domain task stealing.
		 */
		cpdomc->nr_neighbors[0] = nr_domains - 1;
		i = 0;
		for (unsigned int n = 0; n < nr_domains; n++) {
			if (n == d)
				continue;
			cpdomc->neighbor_ids[0 * LAVD_CPDOM_MAX_NR + i] = n;
			i++;
		}
	}

	/*
	 * Enable core compaction so do_core_compaction() runs during
	 * update_sys_stat() and keeps nr_active_cpdoms up to date.
	 */
	no_core_compaction = false;
}

/*
 * =================================================================
 * Configuration setters for DSQ and migration modes
 * =================================================================
 *
 * These set globals that control balance.bpf.c code paths.
 * Must be called AFTER lavd_setup() and BEFORE lavd_init().
 */

/* Enable per-CPU DSQ mode: tasks enqueue to per-CPU DSQs. */
void lavd_set_per_cpu_dsq(unsigned int val)
{
	per_cpu_dsq = !!val;
}

/* Set pinned_slice_ns: dual-DSQ mode (both per-CPU and per-cpdom). */
void lavd_set_pinned_slice_ns(unsigned long long val)
{
	pinned_slice_ns = val;
}

/* Set mig_delta_pct: fixed migration threshold percentage. */
void lavd_set_mig_delta_pct(unsigned int val)
{
	mig_delta_pct = (u8)val;
}

/* Enable/disable the is_monitored flag (introspection latency tracking). */
void lavd_set_is_monitored(unsigned int val)
{
	is_monitored = !!val;
}

/* Enable/disable core compaction. */
void lavd_set_no_core_compaction(unsigned int val)
{
	no_core_compaction = !!val;
}

/*
 * =================================================================
 * Power mode configuration
 * =================================================================
 *
 * These functions mimic the effect of scx_lavd CLI power mode flags:
 *   --performance, --balanced, --powersave, --autopilot
 *
 * Power mode constants (from intf.h):
 *   LAVD_PM_PERFORMANCE = 0
 *   LAVD_PM_BALANCED = 1
 *   LAVD_PM_POWERSAVE = 2
 */

/*
 * Set power mode (performance=0, balanced=1, powersave=2).
 *
 * WARNING: This duplicates logic from the real scx_lavd userspace.
 * If LAVD's power mode behavior changes, this code must be updated.
 *
 * Source of truth (check these if behavior seems wrong):
 *   - scheds/rust/scx_lavd/src/main.rs: Opts::proc(), init_globals()
 *   - scheds/rust/scx_lavd/src/bpf/power.bpf.c: do_set_power_profile()
 *   - scheds/rust/scx_lavd/src/bpf/intf.h: LAVD_PM_* constants
 */
void lavd_set_power_mode(int mode)
{
	power_mode = mode;
	switch (mode) {
	case 0: /* LAVD_PM_PERFORMANCE */
		no_core_compaction = true;
		is_powersave_mode = false;
		break;
	case 1: /* LAVD_PM_BALANCED */
		no_core_compaction = false;
		is_powersave_mode = false;
		break;
	case 2: /* LAVD_PM_POWERSAVE */
		no_core_compaction = false;
		is_powersave_mode = true;
		break;
	}
}

/*
 * Enable or disable autopilot mode.
 *
 * WARNING: This duplicates logic from the real scx_lavd userspace.
 * If LAVD's autopilot behavior changes, this code must be updated.
 *
 * Source of truth:
 *   - scheds/rust/scx_lavd/src/main.rs: Opts::proc() for autopilot init
 *   - scheds/rust/scx_lavd/src/bpf/power.bpf.c: do_autopilot()
 */
void lavd_set_autopilot(int on)
{
	is_autopilot_on = !!on;
	/* autopilot starts in balanced mode (matches main.rs behavior) */
	if (on) {
		power_mode = 1; /* LAVD_PM_BALANCED */
		no_core_compaction = false;
		is_powersave_mode = false;
	}
}

/*
 * Set the maximum number of cgroups that can have BPF map entries.
 *
 * This simulates CBW_NR_CGRP_MAX in production LAVD. Once the limit
 * is reached, scx_cgroup_bw_init() will return -ENOMEM.
 */
extern void sim_cgroup_registry_set_max(unsigned int max);

void lavd_set_cgroup_bw_max(unsigned int max)
{
	sim_cgroup_registry_set_max(max);
}

/*
 * =================================================================
 * Probe functions — exported accessors for scheduler-internal state
 * =================================================================
 *
 * These are called from Rust via dlsym to sample LAVD state during
 * simulation. Each returns 0/default if the context is not available.
 */

/* Per-task probes: access task_ctx fields via get_task_ctx(). */

u16 lavd_probe_lat_cri(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->lat_cri : 0;
}

u64 lavd_probe_wait_freq(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->wait_freq : 0;
}

u64 lavd_probe_wake_freq(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->wake_freq : 0;
}

u64 lavd_probe_avg_runtime(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->avg_runtime_wall : 0;
}

u16 lavd_probe_lat_cri_waker(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->lat_cri_waker : 0;
}

u16 lavd_probe_lat_cri_wakee(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->lat_cri_wakee : 0;
}

/* System-wide probes: access global sys_stat. */

u32 lavd_probe_sys_avg_lat_cri(void)
{
	return sys_stat.avg_lat_cri;
}

u32 lavd_probe_sys_thr_lat_cri(void)
{
	return sys_stat.thr_lat_cri;
}

u64 lavd_probe_sys_nr_sched(void)
{
	return sys_stat.nr_sched;
}

u64 lavd_probe_sys_nr_lat_cri(void)
{
	return sys_stat.nr_lat_cri;
}

u64 lavd_probe_sys_avg_sc_util(void)
{
	return sys_stat.avg_util_invr;
}

int lavd_probe_calc_nr_active(void)
{
	return calc_nr_active_cpus();
}

u32 lavd_probe_sys_nr_active(void)
{
	return sys_stat.nr_active;
}

u32 lavd_probe_sys_nr_cpus_onln(void)
{
	return nr_cpus_onln;
}

/* Probe for sys_stat.slice_wall (current target slice). */
u64 lavd_probe_sys_slice_wall(void)
{
	return sys_stat.slice_wall;
}

/* Probe for sys_stat.nr_queued_task. */
u32 lavd_probe_sys_nr_queued_task(void)
{
	return sys_stat.nr_queued_task;
}

/* Probe for can_boost_slice() result. */
u8 lavd_probe_can_boost_slice(void)
{
	return can_boost_slice() ? 1 : 0;
}

/* Probe for task's slice_wall from task_ctx. */
u64 lavd_probe_task_slice_wall(struct task_struct *p)
{
	struct task_ctx *taskc = get_task_ctx(p);
	return taskc ? taskc->slice_wall : 0;
}

/*
 * Direct setter for sys_stat.nr_active.
 * Used by tests to force the dispatch compaction path
 * (use_full_cpus() returns false when nr_active < nr_cpus_onln).
 */
void lavd_set_sys_nr_active(u32 val)
{
	sys_stat.nr_active = val;
}

/*
 * Direct setter for sys_stat.nr_active_cpdoms.
 */
void lavd_set_sys_nr_active_cpdoms(u32 val)
{
	sys_stat.nr_active_cpdoms = val;
}

/*
 * =================================================================
 * Direct compaction control
 * =================================================================
 *
 * Force compaction state after lavd_init() has run (cpumasks allocated).
 * Sets nr_active, marks first nr_active_cpus CPUs as active, rest as
 * inactive. Uses the PCO ordering table for CPU order.
 *
 * Pure memory operations (no kfuncs), safe to call outside sim context.
 */
void lavd_force_compaction(int nr_active_cpus)
{
	struct bpf_cpumask *active_mask = active_cpumask;
	struct bpf_cpumask *ovrflw_mask = ovrflw_cpumask;
	const volatile u16 *cpu_order;
	int i, cpu;

	if (!active_mask || !ovrflw_mask)
		return;

	cpu_order = get_cpu_order();

	for (i = 0; i < (int)nr_cpu_ids && i < LAVD_CPU_ID_MAX; i++) {
		cpu = cpu_order[i];
		if (cpu >= LAVD_CPU_ID_MAX)
			break;

		if (i < nr_active_cpus) {
			bpf_cpumask_set_cpu(cpu, active_mask);
			bpf_cpumask_clear_cpu(cpu, ovrflw_mask);
		} else {
			bpf_cpumask_clear_cpu(cpu, active_mask);
			bpf_cpumask_clear_cpu(cpu, ovrflw_mask);
		}
	}

	sys_stat.nr_active = nr_active_cpus;
}

/*
 * =================================================================
 * Diagnostic probes for core compaction debugging
 * =================================================================
 */

u8 lavd_probe_no_core_compaction(void)
{
	return (u8)no_core_compaction;
}

u8 lavd_probe_active_cpumask_null(void)
{
	return (u8)(active_cpumask == NULL);
}

u8 lavd_probe_ovrflw_cpumask_null(void)
{
	return (u8)(ovrflw_cpumask == NULL);
}

u8 lavd_probe_cpuc_is_online(int cpu)
{
	if (cpu < 0 || cpu >= MAX_SIM_CPUS)
		return 0;
	return (u8)percpu_ctx[cpu].is_online;
}

u32 lavd_probe_cpuc_eff_cap(int cpu)
{
	if (cpu < 0 || cpu >= MAX_SIM_CPUS)
		return 0;
	return percpu_ctx[cpu].effective_capacity;
}
