/*
 * layered/wrapper.c - Wrapper to compile scx_layered as userspace C
 *
 * Like the other scheduler wrappers, this file includes the simulator
 * wrapper infrastructure and then the unmodified scx_layered BPF source.
 * The header guards in common.bpf.h prevent re-inclusion, so the macro
 * overrides established here take effect inside the scheduler.
 *
 * NOTE: compiled with -Dconst= to strip const qualifiers. BPF schedulers
 * declare globals "const volatile" (patched by the BPF loader); stripping
 * const makes them writable from the wrapper and from Rust.
 *
 * WHAT THIS WRAPPER IS
 * --------------------
 * scx_layered is userspace-driven: ~14.7k lines of Rust
 * (main.rs / alloc.rs / layer_core_growth.rs) compute the topology tables,
 * the layer specifications, and a continuously re-evaluated CPU allocation,
 * then publish them into BPF rodata/bss/maps. This wrapper plays exactly
 * that userspace role and nothing more — it publishes configuration and
 * then gets out of the way. Every scheduling decision is made by the real
 * `main.bpf.c` / `util.bpf.c` / `timer.bpf.c` compiled in below.
 *
 * WHAT IT DELIBERATELY DOES NOT DO (documented divergences, not stubs)
 * -------------------------------------------------------------------
 * 1. STATIC CPU ALLOCATION. Production re-runs `refresh_cpumasks()` on a
 *    timer, growing and shrinking each layer's CPU set from live
 *    utilisation. The simulator has no model for a userspace control loop,
 *    so the allocation is computed once, before ops.init, and held fixed.
 *    Layer growth/shrink code paths are therefore not exercised. Tracked
 *    as a substrate task, NOT worked around by faking the loop.
 * 2. NUMA. The scxsim engine models LLCs and SMT siblings but has no NUMA
 *    concept at all, so `nr_numa_nodes` is a harness-supplied grouping over
 *    LLCs with no simulated distance cost. Same shape as the existing
 *    `cosmos_configure_numa()` precedent.
 * 3. Per-CPU layer-scan orders are a deterministic rotation rather than
 *    production's `fastrand`-shuffled orders. See `fill_layer_orders()`.
 */
#include "sim_wrapper.h"
#include "sim_task.h"

/* ---------------------------------------------------------------------------
 * Externs: libc (resolved from the main binary at dlopen time via -rdynamic)
 * and simulator entry points.
 * ---------------------------------------------------------------------------*/
extern void *memset(void *s, int c, unsigned long n);
extern void *memcpy(void *d, const void *s, unsigned long n);
extern int dprintf(int fd, const char *fmt, ...);

/* RBC counter pause/resume — kfunc bodies must not be counted as scheduler
 * branches. Provided by the Rust binary (kfuncs.rs). */
extern void sim_rbc_pause(void);
extern void sim_rbc_resume(void);

/* Simulated jiffies, derived from the same per-CPU clock as
 * bpf_ktime_get_ns() and the same HZ as the engine's tick interval. */
extern unsigned long long sim_bpf_jiffies64(void);

/* BPF timer arming (slot-based; layered has a single timer, slot 0). */
extern void sim_timer_start_slot(unsigned int slot, unsigned long long nsecs);

/* DSQ iterator backing for bpf_for_each(scx_dsq, ...). */
extern void *sim_dsq_iter_begin(unsigned long long dsq_id, unsigned long long flags);
extern void *sim_dsq_iter_next(void);

/* task_struct accessor used by is_migration_disabled(). */
extern unsigned short sim_task_get_migration_disabled(struct task_struct *p);

/*
 * CONFIG_HZ: `extern unsigned CONFIG_HZ __kconfig` in main.bpf.c. In the
 * kernel the BPF loader patches this from the running kernel's config; here
 * it must match the engine's tick rate (engine::TICK_INTERVAL_NS = 4ms), the
 * same value sim_bpf_jiffies64() divides by. If these two ever disagree,
 * layered's antistall delay accounting silently drifts.
 */
unsigned int CONFIG_HZ = 250;

/* ---------------------------------------------------------------------------
 * BPF helper overrides
 *
 * <bpf/bpf_helper_defs.h> declares these as static function pointers
 * initialised to the raw helper NUMBER, so calling one unoverridden jumps to
 * a bogus address and SIGSEGVs. Every helper scx_layered can reach must be
 * routed to a real implementation here.
 * ---------------------------------------------------------------------------*/

/* bpf_printk -> stderr, matching the LAVD wrapper. layered's dbg()/trace()
 * are gated on the `debug` rodata (default 0), but tests may raise it. */
#undef bpf_printk
#define bpf_printk(fmt, ...) dprintf(2, "[LAYERED] " fmt "\n", ##__VA_ARGS__)

/* bpf_jiffies64: simulated jiffies from the engine clock. */
#undef bpf_jiffies64
#define bpf_jiffies64() sim_bpf_jiffies64()

/* bpf_strncmp(s1, n, s2) compares s1[0..n) with the NUL-terminated s2. */
#undef bpf_strncmp
#define bpf_strncmp(s1, n, s2) __builtin_strncmp((s1), (s2), (n))

/* bpf_probe_read_str: same semantics as the kernel-space variant that
 * sim_wrapper.h already provides. */
#undef bpf_probe_read_str
#define bpf_probe_read_str(dst, sz, src) sim_bpf_probe_read_kernel_str((dst), (sz), (src))

/*
 * bpf_get_current_pid_tgid: only reachable from layered's
 * SEC("?kprobe/nvidia_*") GPU probes, which the simulator never delivers
 * (enable_gpu_support is off and there is no NVIDIA driver to probe).
 * Defined so the symbol resolves; returns the current task's pid/tgid so
 * that if the probes ever are driven the answer is real rather than made up.
 */
static unsigned long long layered_current_pid_tgid(void);
#undef bpf_get_current_pid_tgid
#define bpf_get_current_pid_tgid() layered_current_pid_tgid()

/* bpf_map_delete_elem -> the scx_test_map registry (HASH maps only). */
#undef bpf_map_delete_elem
#define bpf_map_delete_elem(map, key) scx_test_map_delete_elem((void *)(map), (key))

/* bpf_snprintf(str, sz, fmt, data, data_len): formats from a u64 array
 * rather than varargs. Implemented below. */
static long layered_bpf_snprintf(char *str, unsigned int str_sz, const char *fmt,
				 unsigned long long *data, unsigned int data_len);
#undef bpf_snprintf
#define bpf_snprintf(str, sz, fmt, data, data_len) \
	layered_bpf_snprintf((str), (sz), (fmt), (data), (data_len))

/*
 * is_migration_disabled: the kernel's version has special handling for
 * migration_disabled == 1 (ambiguous because the BPF prolog increments it).
 * The simulator has no such prolog, so >0 means disabled. Matches the
 * mitosis and cosmos wrappers.
 */
#undef is_migration_disabled
#define is_migration_disabled(p) (sim_task_get_migration_disabled(p) > 0)

/*
 * The simulator always calls select_cpu before enqueue, so
 * SCX_ENQ_CPU_SELECTED is always effectively set.
 */
#undef __COMPAT_is_enq_cpu_selected
#define __COMPAT_is_enq_cpu_selected(enq_flags) (true)

/*
 * NOTE: bpf_ksym_exists() is deliberately NOT overridden (mitosis forces 0,
 * cosmos forces 1). Every compat wrapper scx_layered uses resolves correctly
 * from the real weak-symbol test: scx_bpf_cpu_curr and
 * scx_bpf_reenqueue_local___v2___compat ARE exported by the simulator so the
 * modern paths are taken, while scx_bpf_task_set_slice___new /
 * scx_bpf_task_set_dsq_vtime___new are NOT, so those fall back to the direct
 * `p->scx.*` writes the simulator supports. Forcing the macro either way
 * would break one of the two groups.
 */

/* ---------------------------------------------------------------------------
 * Map routing
 *
 * Forward-declared here so the macros are in effect inside the scheduler
 * source; defined after it, where the map symbols and value structs exist.
 *
 * WHY STATIC ARRAYS. The generic scx_test_map registry grows its value
 * storage with reallocarray(), so any pointer a scheduler holds across an
 * insert dangles. scx_layered holds `struct task_ctx *` and
 * `struct cpu_ctx *` across nested lookups constantly. BPF ARRAY,
 * PERCPU_ARRAY and (with BPF_F_NO_PREALLOC absent) TASK_STORAGE maps are
 * preallocated in the kernel, so a fixed static array is the MORE faithful
 * model, not a shortcut — the same reasoning as the mitosis wrapper.
 * The genuinely sparse HASH maps still go through scx_test_map.
 * ---------------------------------------------------------------------------*/
static void *layered_map_lookup_elem(void *map, const void *key);
#undef bpf_map_lookup_elem
#define bpf_map_lookup_elem(map, key) layered_map_lookup_elem((void *)(map), (key))

static void *layered_map_lookup_percpu_elem(void *map, const void *key, int cpu);
#undef bpf_map_lookup_percpu_elem
#define bpf_map_lookup_percpu_elem(map, key, cpu) \
	layered_map_lookup_percpu_elem((void *)(map), (key), (cpu))

static void *layered_task_storage_get(void *map, void *task, void *value,
				      unsigned long flags);
#undef bpf_task_storage_get
#define bpf_task_storage_get(map, task, value, flags) \
	layered_task_storage_get((void *)(map), (task), (value), (flags))

static int layered_task_storage_delete(void *map, void *task);
#undef bpf_task_storage_delete
#define bpf_task_storage_delete(map, task) \
	layered_task_storage_delete((void *)(map), (task))

/*
 * bpf_perf_event_read_value() — the one genuinely-hardware primitive
 * scx/lib/pmu.bpf.c needs (see the pmu.bpf.c include below for why the real
 * library is compiled in rather than stubbed).
 *
 * The simulated machine exposes no performance counters: scxsim models CPU
 * time, not microarchitecture, and there is no per-task memory-bandwidth
 * signal to report. -ENOENT is exactly what the kernel returns for a perf
 * event array slot with nothing installed, so the library takes its real
 * "counter unavailable" path instead of being handed invented numbers.
 * Consequently `membw_event` stays 0 and scx_layered's membw code is off —
 * the same state as production run without --membw-event.
 */
static int layered_perf_event_read_value(void *map, unsigned long long flags,
					 void *buf, unsigned int buf_size);
#undef bpf_perf_event_read_value
#define bpf_perf_event_read_value(map, flags, buf, buf_size) \
	layered_perf_event_read_value((void *)(map), (flags), (buf), (buf_size))

/* ---------------------------------------------------------------------------
 * BPF timer routing
 *
 * layered arms one timer (ANTISTALL_TIMER) through a map-backed
 * `struct timer_wrapper`. Record the callback and re-arm through the
 * engine's slot-based timer queue; layered_fire_timer() below is the entry
 * point the engine calls when the TimerFired event pops.
 * ---------------------------------------------------------------------------*/
static unsigned long long layered_timer_fires;
static int (*layered_sim_timer_cb)(void *, int *, struct bpf_timer *);
static void *layered_sim_timer_map;
static struct bpf_timer *layered_sim_timer_ptr;

#undef bpf_timer_init
#define bpf_timer_init(timer, map, flags) (layered_sim_timer_map = (void *)(map), 0)

#undef bpf_timer_set_callback
#define bpf_timer_set_callback(timer, cb)                             \
	(layered_sim_timer_cb = (typeof(layered_sim_timer_cb))(cb),           \
	 layered_sim_timer_ptr = (struct bpf_timer *)(timer), 0)

#undef bpf_timer_start
#define bpf_timer_start(timer, nsecs, flags) (sim_timer_start_slot(0, (nsecs)), 0)

/* ---------------------------------------------------------------------------
 * bpf_for_each(scx_dsq, ...) needs concrete symbols, not just macro rewrites,
 * because the iterator uses a cleanup() destructor. Same shape as the mitosis
 * and cosmos wrappers.
 * ---------------------------------------------------------------------------*/
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

/* ---------------------------------------------------------------------------
 * Include the scx_layered source.
 *
 * ops.init is renamed so the wrapper can run the post-attach steps that
 * scx_layered's userspace performs immediately after attaching
 * (refresh_layer_cpumasks + refresh_node_ctx). See layered_init() below.
 * ---------------------------------------------------------------------------*/
#define layered_init layered_bpf_init

#include "intf.h"
#include "timer.bpf.c"
#include "util.bpf.c"
#include "main.bpf.c"

/*
 * scx/lib/pmu.bpf.c — the REAL PMU library, compiled in.
 *
 * scx_layered calls scx_pmu_install / _uninstall / _task_init / _task_fini /
 * _read for its optional memory-bandwidth tracking. Hand-writing five
 * `scx_pmu_*` bodies in this wrapper (as the cosmos wrapper currently does)
 * would be exactly the "elided library" antipattern scx-sim/CLAUDE.md's
 * No-Stub Rule forbids, so the library's own logic is linked in and only the
 * hardware primitive underneath it (bpf_perf_event_read_value, above) is
 * supplied by the simulator.
 *
 * `_license` has to be renamed because SEC() is a no-op here, so pmu.bpf.c's
 * and main.bpf.c's license arrays would otherwise collide in one translation
 * unit.
 */
#define _license _scx_pmu_license
#include "lib/pmu.bpf.c"
#undef _license

#undef layered_init

/* ---------------------------------------------------------------------------
 * Static map storage (defined after the source, where the value types exist)
 * ---------------------------------------------------------------------------*/

/* Must be >= the engine's maximum CPU count (lib/scxtest NR_CPUS). */
#define LAYERED_MAX_SIM_CPUS 128
/* PID-indexed task storage. Matches the mitosis wrapper's bound. */
#define LAYERED_MAX_SIM_TASKS 4096

/* cpu_ctxs: PERCPU_ARRAY(max_entries=1) — one struct cpu_ctx per CPU. */
static struct cpu_ctx layered_cpu_ctxs[LAYERED_MAX_SIM_CPUS];

/* node_data / llc_data: ARRAY maps, preallocated for every possible id. */
static struct node_ctx layered_node_ctxs[MAX_NUMA_NODES];
static struct llc_ctx layered_llc_ctxs[MAX_LLCS];

/* layer_cpumasks / layer_node_cpumasks: ARRAY maps of kptr wrappers. */
static struct layer_cpumask_wrapper layered_layer_cpumasks[MAX_LAYERS];
static struct layer_node_cpumask_wrapper
	layered_layer_node_cpumasks[MAX_LAYERS * MAX_NUMA_NODES];

/* hint_to_layer_id_map: ARRAY(1025). Only read when task_hint_map_enabled. */
static struct hint_layer_info layered_hint_to_layer[1025];

/* antistall_cpu_dsq / antistall_cpu_max_delay: PERCPU_ARRAY(max_entries=1). */
static u64 layered_antistall_dsq[LAYERED_MAX_SIM_CPUS];
static u64 layered_antistall_max_delay[LAYERED_MAX_SIM_CPUS];

/* layered_timer_data: ARRAY(MAX_TIMERS) of struct timer_wrapper. */
static struct timer_wrapper layered_timer_wrappers[MAX_TIMERS];

/*
 * util.bpf.c scratch buffers: PERCPU_ARRAY(max_entries=1) declared with
 * key_size/value_size rather than __type, so they cannot go through
 * INIT_SCX_TEST_MAP. Backed per-CPU here, matching the kernel's per-CPU
 * allocation (two callbacks on different CPUs must not share a buffer).
 */
static char layered_cgrp_path_bufs[LAYERED_MAX_SIM_CPUS][2 * MAX_PATH];
static char layered_match_bufs[LAYERED_MAX_SIM_CPUS][MAX_PATH];
static char layered_str_bufs[LAYERED_MAX_SIM_CPUS][MAX_PATH];

/* task_ctxs / scx_layered_task_hint_map: TASK_STORAGE, indexed by pid. */
static struct task_ctx layered_task_ctxs[LAYERED_MAX_SIM_TASKS];
static bool layered_task_ctx_in_use[LAYERED_MAX_SIM_TASKS];
static struct task_hint layered_task_hints[LAYERED_MAX_SIM_TASKS];
static bool layered_task_hint_in_use[LAYERED_MAX_SIM_TASKS];

/* scx_pmu_tasks: TASK_STORAGE owned by the compiled-in scx/lib/pmu.bpf.c. */
static struct scx_pmu_counters layered_pmu_counters[LAYERED_MAX_SIM_TASKS];
static bool layered_pmu_counters_in_use[LAYERED_MAX_SIM_TASKS];

/* HASH maps stay on the generic registry — they really are sparse. */
static struct scx_test_map layered_layer_match_dbg_map;
static struct scx_test_map layered_gpu_tgid_map;
static struct scx_test_map layered_gpu_tid_map;
static struct scx_test_map layered_cgroup_match_bitmap_map;

/* Number of CPUs the harness configured. Bounds every per-CPU loop below. */
static u32 layered_nr_sim_cpus = 1;

/* ---------------------------------------------------------------------------
 * Helper implementations forward-declared above the scheduler source
 * ---------------------------------------------------------------------------*/

static unsigned long long layered_current_pid_tgid(void)
{
	struct task_struct *p = (struct task_struct *)sim_bpf_get_current_task_btf();

	if (!p)
		return 0;
	return ((unsigned long long)(u32)p->tgid << 32) | (u32)p->pid;
}

/*
 * layered_bpf_snprintf - userspace stand-in for the bpf_snprintf() helper.
 *
 * The kernel helper takes format arguments as a u64 array rather than
 * varargs. Supports the `%[0][width][l|ll]{d,i,u,x,s}` subset, which covers
 * every call site in scx_layered (all of which build debug headers). An
 * unsupported conversion is copied through literally rather than
 * misformatted, and the result is always NUL-terminated.
 *
 * RBC-guarded: this is a kernel helper, so its branches must not be counted
 * as scheduler overhead.
 */
static void layered_append_uint(char *out, unsigned int sz, unsigned int *len,
				unsigned long long v, unsigned int base,
				unsigned int width, bool zero_pad)
{
	char tmp[24];
	unsigned int n = 0;

	do {
		unsigned int digit = (unsigned int)(v % base);

		tmp[n++] = (char)(digit < 10 ? '0' + digit : 'a' + digit - 10);
		v /= base;
	} while (v && n < sizeof(tmp));

	while (n < width && n < sizeof(tmp))
		tmp[n++] = zero_pad ? '0' : ' ';

	while (n > 0 && *len + 1 < sz)
		out[(*len)++] = tmp[--n];
}

static long layered_bpf_snprintf(char *str, unsigned int str_sz, const char *fmt,
				 unsigned long long *data, unsigned int data_len)
{
	unsigned int nr_args = data_len / sizeof(unsigned long long);
	unsigned int len = 0, arg = 0;
	const char *p = fmt;

	sim_rbc_pause();

	if (!str || str_sz == 0) {
		sim_rbc_resume();
		return -EINVAL;
	}

	while (*p && len + 1 < str_sz) {
		const char *spec_start = p;
		unsigned int width = 0;
		bool zero_pad = false;

		if (*p != '%') {
			str[len++] = *p++;
			continue;
		}
		p++;
		if (*p == '%') {
			str[len++] = *p++;
			continue;
		}
		if (*p == '0') {
			zero_pad = true;
			p++;
		}
		while (*p >= '0' && *p <= '9')
			width = width * 10 + (unsigned int)(*p++ - '0');
		while (*p == 'l')
			p++;

		if (arg >= nr_args || !data) {
			/* Not enough arguments — emit the spec literally. */
			while (spec_start <= p && len + 1 < str_sz)
				str[len++] = *spec_start++;
			p++;
			continue;
		}

		switch (*p) {
		case 'd':
		case 'i': {
			long long v = (long long)data[arg++];

			if (v < 0 && len + 1 < str_sz) {
				str[len++] = '-';
				v = -v;
			}
			layered_append_uint(str, str_sz, &len,
					    (unsigned long long)v, 10, width,
					    zero_pad);
			p++;
			break;
		}
		case 'u':
			layered_append_uint(str, str_sz, &len, data[arg++], 10,
					    width, zero_pad);
			p++;
			break;
		case 'x':
			layered_append_uint(str, str_sz, &len, data[arg++], 16,
					    width, zero_pad);
			p++;
			break;
		case 's': {
			const char *s = (const char *)(unsigned long)data[arg++];

			while (s && *s && len + 1 < str_sz)
				str[len++] = *s++;
			p++;
			break;
		}
		default:
			/* Unsupported conversion — copy it through literally. */
			while (spec_start <= p && len + 1 < str_sz)
				str[len++] = *spec_start++;
			if (*p)
				p++;
			break;
		}
	}

	str[len] = '\0';
	sim_rbc_resume();
	return (long)len + 1;
}

/* ---------------------------------------------------------------------------
 * Map routing implementations
 * ---------------------------------------------------------------------------*/

static void *layered_percpu_scratch(void *base, unsigned long stride, int cpu)
{
	if (cpu < 0 || cpu >= LAYERED_MAX_SIM_CPUS)
		return NULL;
	return (char *)base + (unsigned long)cpu * stride;
}

static void *layered_map_lookup_common(void *map, const void *key, int cpu)
{
	u32 idx = key ? *(const u32 *)key : 0;

	/* --- PERCPU_ARRAY maps: index by CPU, ignore the (always 0) key --- */
	if (map == &cpu_ctxs)
		return layered_percpu_scratch(layered_cpu_ctxs,
					      sizeof(layered_cpu_ctxs[0]), cpu);
	if (map == &antistall_cpu_dsq)
		return layered_percpu_scratch(layered_antistall_dsq,
					      sizeof(layered_antistall_dsq[0]), cpu);
	if (map == &antistall_cpu_max_delay)
		return layered_percpu_scratch(layered_antistall_max_delay,
					      sizeof(layered_antistall_max_delay[0]),
					      cpu);
	if (map == &cgrp_path_bufs)
		return layered_percpu_scratch(layered_cgrp_path_bufs,
					      sizeof(layered_cgrp_path_bufs[0]), cpu);
	if (map == &match_bufs)
		return layered_percpu_scratch(layered_match_bufs,
					      sizeof(layered_match_bufs[0]), cpu);
	if (map == &str_bufs)
		return layered_percpu_scratch(layered_str_bufs,
					      sizeof(layered_str_bufs[0]), cpu);

	/* --- ARRAY maps: index by key --- */
	if (map == &node_data)
		return idx < MAX_NUMA_NODES ? &layered_node_ctxs[idx] : NULL;
	if (map == &llc_data)
		return idx < MAX_LLCS ? &layered_llc_ctxs[idx] : NULL;
	if (map == &layer_cpumasks)
		return idx < MAX_LAYERS ? &layered_layer_cpumasks[idx] : NULL;
	if (map == &layer_node_cpumasks)
		return idx < MAX_LAYERS * MAX_NUMA_NODES
			       ? &layered_layer_node_cpumasks[idx]
			       : NULL;
	if (map == &hint_to_layer_id_map)
		return idx < 1025 ? &layered_hint_to_layer[idx] : NULL;
	if (map == &layered_timer_data)
		return idx < MAX_TIMERS ? &layered_timer_wrappers[idx] : NULL;

	/* --- Genuinely sparse HASH maps: generic registry --- */
	return scx_test_map_lookup_elem(map, key);
}

static void *layered_map_lookup_elem(void *map, const void *key)
{
	return layered_map_lookup_common(map, key,
					 (int)sim_bpf_get_smp_processor_id());
}

static void *layered_map_lookup_percpu_elem(void *map, const void *key, int cpu)
{
	return layered_map_lookup_common(map, key, cpu);
}

#ifndef BPF_LOCAL_STORAGE_GET_F_CREATE
#define BPF_LOCAL_STORAGE_GET_F_CREATE (1ULL << 0)
#endif

static void *layered_task_storage_slot(void *slots, bool *in_use,
				       unsigned long stride, void *task,
				       unsigned long flags, unsigned long size)
{
	struct task_struct *p = (struct task_struct *)task;
	int pid;

	if (!p)
		return NULL;

	pid = p->pid;
	if (pid < 0 || pid >= LAYERED_MAX_SIM_TASKS)
		return NULL;

	if (in_use[pid])
		return (char *)slots + (unsigned long)pid * stride;

	if (!(flags & BPF_LOCAL_STORAGE_GET_F_CREATE))
		return NULL;

	in_use[pid] = true;
	memset((char *)slots + (unsigned long)pid * stride, 0, size);
	return (char *)slots + (unsigned long)pid * stride;
}

static void *layered_task_storage_get(void *map, void *task, void *value,
				      unsigned long flags)
{
	(void)value;

	if (map == &task_ctxs)
		return layered_task_storage_slot(layered_task_ctxs,
						 layered_task_ctx_in_use,
						 sizeof(layered_task_ctxs[0]),
						 task, flags,
						 sizeof(struct task_ctx));
	if (map == &scx_layered_task_hint_map)
		return layered_task_storage_slot(layered_task_hints,
						 layered_task_hint_in_use,
						 sizeof(layered_task_hints[0]),
						 task, flags,
						 sizeof(struct task_hint));
	if (map == &scx_pmu_tasks)
		return layered_task_storage_slot(layered_pmu_counters,
						 layered_pmu_counters_in_use,
						 sizeof(layered_pmu_counters[0]),
						 task, flags,
						 sizeof(struct scx_pmu_counters));
	return NULL;
}

static int layered_task_storage_delete(void *map, void *task)
{
	struct task_struct *p = (struct task_struct *)task;
	int pid;

	if (!p)
		return -EINVAL;
	pid = p->pid;
	if (pid < 0 || pid >= LAYERED_MAX_SIM_TASKS)
		return -EINVAL;

	if (map == &task_ctxs)
		layered_task_ctx_in_use[pid] = false;
	else if (map == &scx_layered_task_hint_map)
		layered_task_hint_in_use[pid] = false;
	else if (map == &scx_pmu_tasks)
		layered_pmu_counters_in_use[pid] = false;
	else
		return -EINVAL;
	return 0;
}

static int layered_perf_event_read_value(void *map, unsigned long long flags,
					 void *buf, unsigned int buf_size)
{
	(void)map;
	(void)flags;
	(void)buf;
	(void)buf_size;
	/* No perf event is installed on the simulated machine. */
	return -ENOENT;
}

/* ---------------------------------------------------------------------------
 * ABI probes
 *
 * The Rust `LayerKind` / `LayerMatch` / `LayerGrowthAlgo` types carry the
 * intf.h enum values as discriminants. Rather than trusting that they stay in
 * sync across scx submodule bumps, export the authoritative values here so a
 * test can compare them. An upstream reordering then fails a test instead of
 * silently mis-configuring every layer.
 * ---------------------------------------------------------------------------*/

/* Selector namespace for layered_probe_enum(). */
enum layered_enum_probe_id {
	LAYERED_PROBE_KIND_OPEN,
	LAYERED_PROBE_KIND_GROUPED,
	LAYERED_PROBE_KIND_CONFINED,
	LAYERED_PROBE_MATCH_CGROUP_PREFIX,
	LAYERED_PROBE_MATCH_COMM_PREFIX,
	LAYERED_PROBE_MATCH_PCOMM_PREFIX,
	LAYERED_PROBE_MATCH_NICE_ABOVE,
	LAYERED_PROBE_MATCH_NICE_BELOW,
	LAYERED_PROBE_MATCH_NICE_EQUALS,
	LAYERED_PROBE_MATCH_USER_ID_EQUALS,
	LAYERED_PROBE_MATCH_GROUP_ID_EQUALS,
	LAYERED_PROBE_MATCH_PID_EQUALS,
	LAYERED_PROBE_MATCH_PPID_EQUALS,
	LAYERED_PROBE_MATCH_TGID_EQUALS,
	LAYERED_PROBE_MATCH_IS_GROUP_LEADER,
	LAYERED_PROBE_MATCH_IS_KTHREAD,
	LAYERED_PROBE_MATCH_CGROUP_SUFFIX,
	LAYERED_PROBE_MATCH_CGROUP_CONTAINS,
	LAYERED_PROBE_MATCH_NUMA_NODE,
	LAYERED_PROBE_GROWTH_STICKY,
	LAYERED_PROBE_GROWTH_LINEAR,
	LAYERED_PROBE_GROWTH_REVERSE,
	LAYERED_PROBE_GROWTH_TOPO,
	LAYERED_PROBE_GROWTH_ROUND_ROBIN,
	LAYERED_PROBE_MAX_LAYERS,
	LAYERED_PROBE_DEFAULT_LAYER_WEIGHT,
	LAYERED_PROBE_NR_INVALID,
};

int layered_probe_enum(int which)
{
	switch (which) {
	case LAYERED_PROBE_KIND_OPEN:			return LAYER_KIND_OPEN;
	case LAYERED_PROBE_KIND_GROUPED:		return LAYER_KIND_GROUPED;
	case LAYERED_PROBE_KIND_CONFINED:		return LAYER_KIND_CONFINED;
	case LAYERED_PROBE_MATCH_CGROUP_PREFIX:		return MATCH_CGROUP_PREFIX;
	case LAYERED_PROBE_MATCH_COMM_PREFIX:		return MATCH_COMM_PREFIX;
	case LAYERED_PROBE_MATCH_PCOMM_PREFIX:		return MATCH_PCOMM_PREFIX;
	case LAYERED_PROBE_MATCH_NICE_ABOVE:		return MATCH_NICE_ABOVE;
	case LAYERED_PROBE_MATCH_NICE_BELOW:		return MATCH_NICE_BELOW;
	case LAYERED_PROBE_MATCH_NICE_EQUALS:		return MATCH_NICE_EQUALS;
	case LAYERED_PROBE_MATCH_USER_ID_EQUALS:	return MATCH_USER_ID_EQUALS;
	case LAYERED_PROBE_MATCH_GROUP_ID_EQUALS:	return MATCH_GROUP_ID_EQUALS;
	case LAYERED_PROBE_MATCH_PID_EQUALS:		return MATCH_PID_EQUALS;
	case LAYERED_PROBE_MATCH_PPID_EQUALS:		return MATCH_PPID_EQUALS;
	case LAYERED_PROBE_MATCH_TGID_EQUALS:		return MATCH_TGID_EQUALS;
	case LAYERED_PROBE_MATCH_IS_GROUP_LEADER:	return MATCH_IS_GROUP_LEADER;
	case LAYERED_PROBE_MATCH_IS_KTHREAD:		return MATCH_IS_KTHREAD;
	case LAYERED_PROBE_MATCH_CGROUP_SUFFIX:		return MATCH_CGROUP_SUFFIX;
	case LAYERED_PROBE_MATCH_CGROUP_CONTAINS:	return MATCH_CGROUP_CONTAINS;
	case LAYERED_PROBE_MATCH_NUMA_NODE:		return MATCH_NUMA_NODE;
	case LAYERED_PROBE_GROWTH_STICKY:		return GROWTH_ALGO_STICKY;
	case LAYERED_PROBE_GROWTH_LINEAR:		return GROWTH_ALGO_LINEAR;
	case LAYERED_PROBE_GROWTH_REVERSE:		return GROWTH_ALGO_REVERSE;
	case LAYERED_PROBE_GROWTH_TOPO:			return GROWTH_ALGO_TOPO;
	case LAYERED_PROBE_GROWTH_ROUND_ROBIN:		return GROWTH_ALGO_ROUND_ROBIN;
	case LAYERED_PROBE_MAX_LAYERS:			return MAX_LAYERS;
	case LAYERED_PROBE_DEFAULT_LAYER_WEIGHT:	return DEFAULT_LAYER_WEIGHT;
	default:					return -1;
	}
}

/* ---------------------------------------------------------------------------
 * Read-only observability probes
 *
 * These read real scheduler state so tests can assert on layered's own
 * decisions rather than on a re-derivation. Same pattern as the LAVD
 * wrapper's lat_cri probes. They must never mutate scheduler state.
 * ---------------------------------------------------------------------------*/

/* Which layer scx_layered assigned to `pid`, or MAX_LAYERS if none/unknown. */
unsigned int layered_probe_task_layer(int pid)
{
	if (pid < 0 || pid >= LAYERED_MAX_SIM_TASKS)
		return MAX_LAYERS;
	if (!layered_task_ctx_in_use[pid])
		return MAX_LAYERS;
	return layered_task_ctxs[pid].layer_id;
}

/* The DSQ scx_layered last enqueued `pid` to, or SCX_DSQ_INVALID. */
unsigned long long layered_probe_task_dsq(int pid)
{
	if (pid < 0 || pid >= LAYERED_MAX_SIM_TASKS ||
	    !layered_task_ctx_in_use[pid])
		return SCX_DSQ_INVALID;
	return layered_task_ctxs[pid].dsq_id;
}

/* Number of layers currently configured. */
unsigned int layered_probe_nr_layers(void)
{
	return nr_layers;
}

/* A configured layer's CPU count, as the BPF side sees it. */
unsigned int layered_probe_layer_nr_cpus(unsigned int layer_id)
{
	if (layer_id >= nr_layers)
		return 0;
	return layers[layer_id].nr_cpus;
}

/* Is `cpu` in `layer_id`'s published cpumask? */
int layered_probe_layer_has_cpu(unsigned int layer_id, unsigned int cpu)
{
	if (layer_id >= nr_layers || cpu >= MAX_CPUS)
		return 0;
	return !!(((volatile unsigned char *)layers[layer_id].cpus)[cpu / 8] &
		  (1 << (cpu % 8)));
}

/* Per-layer task count (`layer->nr_tasks`), maintained by switch_to_layer(). */
unsigned long long layered_probe_layer_nr_tasks(unsigned int layer_id)
{
	if (layer_id >= nr_layers)
		return 0;
	return layers[layer_id].nr_tasks;
}

/* Sum of a per-layer stat across all CPUs (see `enum layer_stat_id`). */
unsigned long long layered_probe_layer_stat(unsigned int layer_id, unsigned int stat_id)
{
	unsigned long long total = 0;
	u32 cpu;

	if (layer_id >= MAX_LAYERS || stat_id >= NR_LSTATS)
		return 0;
	for (cpu = 0; cpu < layered_nr_sim_cpus && cpu < LAYERED_MAX_SIM_CPUS; cpu++)
		total += layered_cpu_ctxs[cpu].lstats[layer_id][stat_id];
	return total;
}

/* Sum of a global stat across all CPUs (see `enum global_stat_id`). */
unsigned long long layered_probe_global_stat(unsigned int stat_id)
{
	unsigned long long total = 0;
	u32 cpu;

	if (stat_id >= NR_GSTATS)
		return 0;
	for (cpu = 0; cpu < layered_nr_sim_cpus && cpu < LAYERED_MAX_SIM_CPUS; cpu++)
		total += layered_cpu_ctxs[cpu].gstats[stat_id];
	return total;
}

/* Topology as the scheduler sees it, for cross-checking against the engine. */
unsigned int layered_probe_cpu_llc(unsigned int cpu)
{
	if (cpu >= LAYERED_MAX_SIM_CPUS)
		return (unsigned int)-1;
	return layered_cpu_ctxs[cpu].llc_id;
}

unsigned int layered_probe_cpu_node(unsigned int cpu)
{
	if (cpu >= LAYERED_MAX_SIM_CPUS)
		return (unsigned int)-1;
	return layered_cpu_ctxs[cpu].node_id;
}

unsigned int layered_probe_nr_llcs(void)
{
	return nr_llcs;
}

unsigned int layered_probe_nr_nodes(void)
{
	return nr_nodes;
}

int layered_probe_sibling_cpu(unsigned int cpu)
{
	if (cpu >= MAX_CPUS)
		return -1;
	return __sibling_cpu[cpu];
}

/* ---------------------------------------------------------------------------
 * Timer delivery
 * ---------------------------------------------------------------------------*/

/*
 * Called by the engine when a TimerFired event pops. scx_layered has exactly
 * one timer (ANTISTALL_TIMER, see timer.bpf.h), armed in slot 0, so `slot` is
 * ignored — but the argument is required by the FireTimerFn FFI signature.
 */
void layered_fire_timer(unsigned int slot)
{
	int key = 0;

	(void)slot;
	if (layered_sim_timer_cb && layered_sim_timer_ptr) {
		layered_timer_fires++;
		layered_sim_timer_cb(layered_sim_timer_map, &key, layered_sim_timer_ptr);
	}
}

/* How many times the antistall timer callback has run this simulation. */
unsigned long long layered_probe_timer_fires(void)
{
	return layered_timer_fires;
}

/* ---------------------------------------------------------------------------
 * tp_btf tracepoint delivery
 *
 * scx_layered attaches two BTF tracepoints outside the struct_ops surface.
 * Both are real BPF programs that must run, so the wrapper exports plain-C
 * entry points the engine can call at the corresponding simulated events.
 *
 * BPF_PROG() expands to `name(unsigned long long *ctx)` plus an inlined
 * `____name(ctx, typed args...)` that casts out of the ctx array, so the
 * shims below marshal a ctx array exactly as the kernel's BTF tracepoint
 * trampoline does.
 * ---------------------------------------------------------------------------*/

/*
 * tp_btf/cgroup_attach_task(cgrp, cgrp_path, leader, threadgroup).
 *
 * Fired when a task is moved into a cgroup. scx_layered uses it (rather than
 * ops.cgroup_move) because layer membership follows the DEFAULT hierarchy,
 * not the CPU controller's.
 *
 * `threadgroup` is always false here, and that is the faithful value: the
 * simulator migrates one task at a time, never a whole thread group. It also
 * matters for safety — the threadgroup path walks
 * `leader->signal->thread_head`, and the simulated task_struct has no
 * `signal`, so claiming a group move would dereference near-NULL. If the
 * engine ever grows thread-group migration, `p->signal->thread_head` and
 * `p->thread_node` have to be modelled first.
 */
void layered_tp_cgroup_attach_task(void *cgrp, const char *cgrp_path, void *leader)
{
	unsigned long long ctx[4];

	ctx[0] = (unsigned long long)(unsigned long)cgrp;
	ctx[1] = (unsigned long long)(unsigned long)cgrp_path;
	ctx[2] = (unsigned long long)(unsigned long)leader;
	ctx[3] = 0; /* threadgroup = false */
	tp_cgroup_attach_task(ctx);
}

/*
 * tp_btf/task_rename(p, buf).
 *
 * Fired when a task's comm changes. scx_layered marks the task for
 * re-layering (a rename can change which comm-prefix rule matches) and parses
 * the new name for an embedded SCXCMD join/leave command.
 */
void layered_tp_task_rename(void *p, const char *new_comm)
{
	unsigned long long ctx[2];

	ctx[0] = (unsigned long long)(unsigned long)p;
	ctx[1] = (unsigned long long)(unsigned long)new_comm;
	tp_task_rename(ctx);
}

/*
 * Configure antistall, mirroring scx_layered's `--disable-antistall` and
 * `--antistall-sec` CLI options.
 *
 * @timer_interval_ns overrides `layered_timers[ANTISTALL_TIMER].interval_ns`,
 * which production hardcodes at 15s. Shortening it is a SIMULATION
 * ACCELERATOR, not a production configuration: it lets a test reach the
 * antistall scan without simulating 15 seconds of wall-equivalent time. Pass
 * 0 to keep the production interval.
 *
 * Must be called before Simulator::run(), because start_layered_timers()
 * reads the interval during ops.init.
 */
void layered_set_antistall(int enable, unsigned long long sec,
			   unsigned long long timer_interval_ns)
{
	enable_antistall = !!enable;
	/* `sec` is applied verbatim, including 0 — production accepts
	 * `--antistall-sec 0`, and it is the only way a test can reach the
	 * antistall path without simulating multiple seconds of delay. */
	antistall_sec = sec;
	if (timer_interval_ns)
		layered_timers[ANTISTALL_TIMER].interval_ns = timer_interval_ns;
}

/* ---------------------------------------------------------------------------
 * Topology publication (the userspace half of scx_layered's init)
 * ---------------------------------------------------------------------------*/

/* Harness-configured topology, mirroring the engine's own layout. */
static u32 layered_cpus_per_llc = 1;
static u32 layered_llcs_per_node = 1;
static u32 layered_threads_per_core = 1;

static u32 layered_cpu_llc(u32 cpu)
{
	return layered_cpus_per_llc ? cpu / layered_cpus_per_llc : 0;
}

static u32 layered_cpu_node(u32 cpu)
{
	return layered_llcs_per_node ? layered_cpu_llc(cpu) / layered_llcs_per_node : 0;
}

static u32 layered_cpu_core(u32 cpu)
{
	return layered_threads_per_core ? cpu / layered_threads_per_core : cpu;
}

static u32 layered_abs_diff(u32 a, u32 b)
{
	return a > b ? a - b : b - a;
}

/*
 * Stable insertion sort of @ids by (primary, secondary) distance keys, exactly
 * reproducing the `radiate` / `radiate_cpu` closures in scx_layered's
 * main.rs::init_cpu_prox_map(). Rust's sort_by_key is stable and the input is
 * ascending, so equal-distance ids keep ascending order; insertion sort with a
 * strict `>` comparison has the same property.
 */
struct layered_prox_key {
	u32 id;
	u32 primary;
	u32 secondary;
};

static void layered_sort_prox(struct layered_prox_key *keys, u32 n)
{
	u32 i, j;

	for (i = 1; i < n; i++) {
		struct layered_prox_key k = keys[i];

		j = i;
		while (j > 0 &&
		       (keys[j - 1].primary > k.primary ||
			(keys[j - 1].primary == k.primary &&
			 keys[j - 1].secondary > k.secondary))) {
			keys[j] = keys[j - 1];
			j--;
		}
		keys[j] = k;
	}
}

/*
 * Build cpuc->prox_map for @cpu: self, then same-core CPUs, then same-LLC,
 * then same-node, then the rest — each group ordered by core-distance then
 * cpu-distance, as production does.
 */
static void layered_fill_cpu_prox_map(struct cpu_ctx *cpuc, u32 cpu, u32 nr_cpus)
{
	struct layered_prox_key core_g[LAYERED_MAX_SIM_CPUS];
	struct layered_prox_key llc_g[LAYERED_MAX_SIM_CPUS];
	struct layered_prox_key node_g[LAYERED_MAX_SIM_CPUS];
	struct layered_prox_key sys_g[LAYERED_MAX_SIM_CPUS];
	u32 nr_core = 0, nr_llc = 0, nr_node = 0, nr_sys = 0;
	u32 my_core = layered_cpu_core(cpu);
	u32 my_llc = layered_cpu_llc(cpu);
	u32 my_node = layered_cpu_node(cpu);
	struct cpu_prox_map *pmap = &cpuc->prox_map;
	u32 other, idx = 0, i;

	for (other = 0; other < nr_cpus && other < LAYERED_MAX_SIM_CPUS; other++) {
		struct layered_prox_key k = {
			.id = other,
			.primary = layered_abs_diff(my_core,
						    layered_cpu_core(other)),
			.secondary = layered_abs_diff(cpu, other),
		};

		if (other == cpu)
			continue;
		if (layered_cpu_core(other) == my_core)
			core_g[nr_core++] = k;
		else if (layered_cpu_llc(other) == my_llc)
			llc_g[nr_llc++] = k;
		else if (layered_cpu_node(other) == my_node)
			/* The node group radiates by node distance in
			 * production; within a single node that degenerates to
			 * the same ascending order the cpu-distance sort gives
			 * for a contiguous layout. */
			node_g[nr_node++] = k;
		else
			sys_g[nr_sys++] = k;
	}

	layered_sort_prox(core_g, nr_core);
	layered_sort_prox(llc_g, nr_llc);
	layered_sort_prox(node_g, nr_node);
	layered_sort_prox(sys_g, nr_sys);

	pmap->cpus[idx++] = (u16)cpu;
	for (i = 0; i < nr_core; i++)
		pmap->cpus[idx++] = (u16)core_g[i].id;
	pmap->core_end = idx;
	for (i = 0; i < nr_llc; i++)
		pmap->cpus[idx++] = (u16)llc_g[i].id;
	pmap->llc_end = idx;
	for (i = 0; i < nr_node; i++)
		pmap->cpus[idx++] = (u16)node_g[i].id;
	pmap->node_end = idx;
	for (i = 0; i < nr_sys; i++)
		pmap->cpus[idx++] = (u16)sys_g[i].id;
	pmap->sys_end = idx;
}

/* Build llcc->prox_maps[*]: self, then same-node LLCs, then the rest. */
static void layered_fill_llc_prox_maps(struct llc_ctx *llcc, u32 llc, u32 nr_llcs)
{
	u32 my_node = layered_llcs_per_node ? llc / layered_llcs_per_node : 0;
	u16 order[MAX_LLCS];
	u32 idx = 0, node_end, other, m;

	order[idx++] = (u16)llc;
	for (other = 0; other < nr_llcs && other < MAX_LLCS; other++) {
		u32 other_node =
			layered_llcs_per_node ? other / layered_llcs_per_node : 0;

		if (other != llc && other_node == my_node)
			order[idx++] = (u16)other;
	}
	node_end = idx;
	for (other = 0; other < nr_llcs && other < MAX_LLCS; other++) {
		u32 other_node =
			layered_llcs_per_node ? other / layered_llcs_per_node : 0;

		if (other != llc && other_node != my_node)
			order[idx++] = (u16)other;
	}

	/*
	 * Production randomises each of the NUM_PROXIMITY_MAPS orders with a
	 * per-(map, llc) fastrand seed so different LLCs spread their stealing.
	 * The simulator's bpf_get_prandom_u32() is deterministically 0, so
	 * layered only ever selects prox_maps[0]; filling all of them with the
	 * same deterministic distance order keeps the observable behaviour
	 * identical and the run reproducible.
	 */
	for (m = 0; m < NUM_PROXIMITY_MAPS; m++) {
		struct llc_prox_map *pmap = &llcc->prox_maps[m];
		u32 i;

		for (i = 0; i < idx; i++)
			pmap->llcs[i] = order[i];
		pmap->node_end = node_end;
		pmap->sys_end = idx;
	}
}

/* Build nodec->prox_map: every other node, in ascending id order. The engine
 * models no inter-node distances, so ascending id is the only ordering the
 * simulated hardware justifies. */
static void layered_fill_node_prox_map(struct node_ctx *nodec, u32 node, u32 nr)
{
	struct node_prox_map *pmap = &nodec->prox_map;
	u32 other, idx = 0;

	for (other = 0; other < nr && other < MAX_NUMA_NODES; other++) {
		if (other != node)
			pmap->nodes[idx++] = (u16)other;
	}
	pmap->sys_end = idx;
}

/*
 * Per-CPU layer scan orders.
 *
 * Production shuffles each CPU's order with `fastrand::seed(cpu)` so that
 * different CPUs start their scan at different layers, spreading contention.
 * Reproducing fastrand's exact stream here would be a fragile transcription,
 * so the simulator uses a deterministic per-CPU ROTATION instead: it keeps the
 * same intent (no two adjacent CPUs scan in the same order) while staying
 * exactly reproducible run to run. Slots past the end are filled with
 * MAX_LAYERS, the sentinel production uses for "no layer".
 */
static void layered_fill_one_order(u32 *dst, const u32 *src, u32 n, u32 cpu)
{
	u32 i;

	for (i = 0; i < MAX_LAYERS; i++)
		dst[i] = n ? src[(i + cpu) % n] : MAX_LAYERS;
	for (i = n; i < MAX_LAYERS; i++)
		dst[i] = MAX_LAYERS;
}

/* ---------------------------------------------------------------------------
 * Layer configuration state (the harness's stand-in for a LayerSpec list)
 * ---------------------------------------------------------------------------*/

/* Per-layer explicit CPU assignment, or "not set" -> auto-allocate. */
static bool layered_layer_cpus_explicit[MAX_LAYERS];
static u64 layered_layer_cpu_words[MAX_LAYERS][MAX_CPUS / 64];

static void layered_layer_set_cpu(u32 layer_id, u32 cpu)
{
	layered_layer_cpu_words[layer_id][cpu / 64] |= 1ULL << (cpu % 64);
}

static bool layered_layer_test_cpu(u32 layer_id, u32 cpu)
{
	return !!(layered_layer_cpu_words[layer_id][cpu / 64] & (1ULL << (cpu % 64)));
}

/* ---------------------------------------------------------------------------
 * Public setup API (called from Rust; see DynamicScheduler::layered*)
 * ---------------------------------------------------------------------------*/

static void layered_register_hash_maps(void)
{
	scx_test_map_clear_all();

	INIT_SCX_TEST_MAP(&layered_layer_match_dbg_map, layer_match_dbg);
	scx_test_map_register(&layered_layer_match_dbg_map, &layer_match_dbg);

	INIT_SCX_TEST_MAP(&layered_gpu_tgid_map, gpu_tgid);
	scx_test_map_register(&layered_gpu_tgid_map, &gpu_tgid);

	INIT_SCX_TEST_MAP(&layered_gpu_tid_map, gpu_tid);
	scx_test_map_register(&layered_gpu_tid_map, &gpu_tid);

	INIT_SCX_TEST_MAP(&layered_cgroup_match_bitmap_map, cgroup_match_bitmap);
	scx_test_map_register(&layered_cgroup_match_bitmap_map, &cgroup_match_bitmap);
}

/*
 * Reset the layer table.
 *
 * `struct layer` is ~10MB (MAX_LAYER_MATCH_ORS * NR_LAYER_MATCH_KINDS
 * * MAX_PATH-sized strings), so `layers[]` is ~165MB of BSS. Zeroing the whole
 * thing on every load would touch every page of it; instead only the scalar
 * tail past `matches` is cleared here, and each `struct layer_match` is
 * zeroed individually when it is (re)configured.
 */
static void layered_reset_layers_internal(void)
{
	u32 i;
	unsigned long tail_off = __builtin_offsetof(struct layer, nr_match_ors);

	for (i = 0; i < MAX_LAYERS; i++) {
		memset((char *)&layers[i] + tail_off, 0,
		       sizeof(struct layer) - tail_off);
		layers[i].id = i;
		layered_layer_cpus_explicit[i] = false;
		memset(layered_layer_cpu_words[i], 0,
		       sizeof(layered_layer_cpu_words[i]));
	}
	nr_layers = 0;
}

void layered_reset_layers(void)
{
	layered_reset_layers_internal();
}

/*
 * Add a layer. Mirrors scx_layered's `LayerSpec` -> `struct layer` publication
 * in main.rs::init_layers(). Returns the new layer id, or -1 if MAX_LAYERS is
 * exhausted.
 *
 * `kind` is a `enum layer_kind` value; `growth_algo` an `enum
 * layer_growth_algo`. Zero `slice_ns` / `max_exec_ns` mean "inherit the global
 * default", exactly as production's per-layer overrides do.
 */
int layered_add_layer(const char *name, int kind, int preempt, int preempt_first,
		      int excl, unsigned int weight, unsigned long long slice_ns_arg,
		      unsigned long long min_exec_ns_arg,
		      unsigned long long max_exec_ns_arg, int growth_algo,
		      int is_protected)
{
	struct layer *layer;
	u32 id = nr_layers;
	int i;

	if (id >= MAX_LAYERS)
		return -1;

	layer = &layers[id];
	layer->id = id;
	layer->kind = kind;
	layer->preempt = !!preempt;
	layer->preempt_first = !!preempt_first;
	layer->excl = !!excl;
	layer->is_protected = !!is_protected;
	layer->weight = weight ? weight : DEFAULT_LAYER_WEIGHT;
	layer->slice_ns = slice_ns_arg ? slice_ns_arg : slice_ns;
	layer->min_exec_ns = min_exec_ns_arg;
	layer->max_exec_ns = max_exec_ns_arg ? max_exec_ns_arg : max_exec_ns;
	layer->growth_algo = growth_algo;
	layer->nr_match_ors = 0;
	layer->task_place = PLACEMENT_STD;
	/* Production defaults from scx_layered's LayerCommon defaults. */
	layer->disallow_open_after_ns = (u64)-1;
	layer->disallow_preempt_after_ns = (u64)-1;
	layer->yield_step_ns = 0;
	layer->xllc_mig_min_ns = 0;

	for (i = 0; i < MAX_LAYER_NAME - 1 && name && name[i]; i++)
		layer->name[i] = name[i];
	layer->name[i < 0 ? 0 : i] = '\0';

	nr_layers = id + 1;
	return (int)id;
}

/*
 * Attach a match rule to (layer_id, or_id). Rules within an OR group are
 * ANDed; OR groups are tried in order. A layer with one empty OR group is the
 * catch-all, matching every task — the same shape scx_layered's "default"
 * layer spec has.
 *
 * `str_arg` carries the string for the *_PREFIX / *_SUFFIX / *_CONTAINS kinds,
 * `int_arg` the scalar for the numeric kinds. Returns 0, or a negative errno.
 */
int layered_add_layer_match(unsigned int layer_id, unsigned int or_id, int kind,
			    const char *str_arg, long long int_arg, int exclude)
{
	struct layer *layer;
	struct layer_match_ands *ands;
	struct layer_match *match;
	u32 and_id;
	int i;

	if (layer_id >= nr_layers || or_id >= MAX_LAYER_MATCH_ORS)
		return -EINVAL;

	layer = &layers[layer_id];
	ands = &layer->matches[or_id];
	and_id = (u32)ands->nr_match_ands;
	if (and_id >= NR_LAYER_MATCH_KINDS)
		return -E2BIG;

	match = &ands->matches[and_id];
	memset(match, 0, sizeof(*match));
	match->kind = kind;
	match->exclude = !!exclude;

	switch (kind) {
	case MATCH_CGROUP_PREFIX:
	case MATCH_CGROUP_SUFFIX:
	case MATCH_CGROUP_CONTAINS: {
		char *dst = kind == MATCH_CGROUP_PREFIX   ? match->cgroup_prefix
			    : kind == MATCH_CGROUP_SUFFIX ? match->cgroup_suffix
							  : match->cgroup_substr;

		if (!str_arg)
			return -EINVAL;
		for (i = 0; i < MAX_PATH - 1 && str_arg[i]; i++)
			dst[i] = str_arg[i];
		dst[i] = '\0';
		break;
	}
	case MATCH_COMM_PREFIX:
	case MATCH_PCOMM_PREFIX:
	case MATCH_SCXCMD_JOIN: {
		char *dst = kind == MATCH_PCOMM_PREFIX ? match->pcomm_prefix
						       : match->comm_prefix;

		if (!str_arg)
			return -EINVAL;
		for (i = 0; i < MAX_COMM - 1 && str_arg[i]; i++)
			dst[i] = str_arg[i];
		dst[i] = '\0';
		break;
	}
	case MATCH_NICE_ABOVE:
	case MATCH_NICE_BELOW:
	case MATCH_NICE_EQUALS:
		match->nice = (int)int_arg;
		break;
	case MATCH_USER_ID_EQUALS:
		match->user_id = (u32)int_arg;
		break;
	case MATCH_GROUP_ID_EQUALS:
		match->group_id = (u32)int_arg;
		break;
	case MATCH_PID_EQUALS:
		match->pid = (u32)int_arg;
		break;
	case MATCH_PPID_EQUALS:
		match->ppid = (u32)int_arg;
		break;
	case MATCH_TGID_EQUALS:
		match->tgid = (u32)int_arg;
		break;
	case MATCH_IS_GROUP_LEADER:
		match->is_group_leader = !!int_arg;
		break;
	case MATCH_IS_KTHREAD:
		match->is_kthread = !!int_arg;
		break;
	case MATCH_NUMA_NODE:
		match->numa_node_id = (u32)int_arg;
		break;
	default:
		/*
		 * No silent success: kinds the harness cannot configure
		 * (namespace / GPU / hint / EWMA matchers, which need
		 * substrate the simulator does not model) are rejected rather
		 * than silently producing a rule that never matches.
		 */
		memset(match, 0, sizeof(*match));
		return -ENOTSUP;
	}

	ands->nr_match_ands = (int)and_id + 1;
	if (or_id >= layer->nr_match_ors)
		layer->nr_match_ors = or_id + 1;
	return 0;
}

/*
 * Declare how many OR groups a layer has.
 *
 * `layered_add_layer_match()` grows `nr_match_ors` implicitly, but a group
 * with ZERO AND rules — the catch-all shape, which matches every task — has
 * no match call to grow it. Without this the catch-all layer would silently
 * match nothing and every task would fail `maybe_refresh_layer()`.
 */
int layered_set_layer_nr_match_ors(unsigned int layer_id, unsigned int nr_ors)
{
	if (layer_id >= nr_layers || nr_ors > MAX_LAYER_MATCH_ORS)
		return -EINVAL;
	if (nr_ors > layers[layer_id].nr_match_ors)
		layers[layer_id].nr_match_ors = nr_ors;
	return 0;
}

/*
 * Pin a layer to an explicit CPU set, overriding the automatic allocation.
 * `words` is a little-endian bitmap, `nr_words` its length.
 */
int layered_set_layer_cpus(unsigned int layer_id, const unsigned long long *words,
			   unsigned int nr_words)
{
	unsigned int w;

	if (layer_id >= nr_layers || !words)
		return -EINVAL;

	memset(layered_layer_cpu_words[layer_id], 0,
	       sizeof(layered_layer_cpu_words[layer_id]));
	for (w = 0; w < nr_words && w < MAX_CPUS / 64; w++)
		layered_layer_cpu_words[layer_id][w] = words[w];
	layered_layer_cpus_explicit[layer_id] = true;
	return 0;
}

/*
 * Compute the static CPU allocation for every layer that did not get an
 * explicit set, then publish it into `struct layer`.
 *
 * OPEN layers may run anywhere by definition, so they get every CPU.
 * GROUPED / CONFINED layers get a contiguous, weight-proportional slice —
 * the steady state scx_layered's allocator converges toward for a uniform
 * workload, computed once instead of continuously. Every layer is guaranteed
 * at least one CPU so no layer's DSQ can be permanently unservable.
 */
static void layered_auto_allocate_cpus(u32 nr_cpus)
{
	u32 total_weight = 0, assigned = 0, id, cpu;

	for (id = 0; id < nr_layers; id++) {
		if (layered_layer_cpus_explicit[id])
			continue;
		if (layers[id].kind == LAYER_KIND_OPEN) {
			for (cpu = 0; cpu < nr_cpus; cpu++)
				layered_layer_set_cpu(id, cpu);
			continue;
		}
		total_weight += layers[id].weight;
	}
	if (!total_weight)
		return;

	for (id = 0; id < nr_layers; id++) {
		u32 share;

		if (layered_layer_cpus_explicit[id] ||
		    layers[id].kind == LAYER_KIND_OPEN)
			continue;

		share = (nr_cpus * layers[id].weight) / total_weight;
		if (share == 0)
			share = 1;
		if (assigned + share > nr_cpus)
			share = assigned < nr_cpus ? nr_cpus - assigned : 1;

		for (cpu = 0; cpu < share; cpu++)
			layered_layer_set_cpu(id, (assigned + cpu) % nr_cpus);
		assigned += share;
	}
}

/* Publish a layer's CPU set into the fields the BPF side reads. */
static void layered_publish_layer_cpus(u32 id, u32 nr_cpus)
{
	struct layer *layer = &layers[id];
	u32 cpu, n;

	memset((void *)layer->cpus, 0, sizeof(layer->cpus));
	memset(layer->nr_llc_cpus, 0, sizeof(layer->nr_llc_cpus));
	for (n = 0; n < MAX_NUMA_NODES; n++)
		layer->node[n].nr_cpus = 0;
	layer->nr_cpus = 0;

	for (cpu = 0; cpu < nr_cpus; cpu++) {
		if (!layered_layer_test_cpu(id, cpu))
			continue;
		((volatile unsigned char *)layer->cpus)[cpu / 8] |=
			(unsigned char)(1 << (cpu % 8));
		layer->nr_cpus++;
		layer->nr_llc_cpus[layered_cpu_llc(cpu)]++;
		layer->node[layered_cpu_node(cpu)].nr_cpus++;
	}
	/* Tell the BPF side its cpumask kptrs are stale (userspace sets this
	 * before running refresh_layer_cpumasks; see update_bpf_layer_cpumask). */
	layer->refresh_cpus = 1;
}

/*
 * Derive the rodata layer summaries scx_layered's userspace computes:
 * the per-kind layer counts, the weight-ordered iteration order, and the
 * minimum open-layer disallow windows.
 */
static void layered_finalize_layer_rodata(void)
{
	u32 op[MAX_LAYERS], on[MAX_LAYERS], gp[MAX_LAYERS], gn[MAX_LAYERS];
	u32 id, i, j, cpu;
	u32 order[MAX_LAYERS];

	nr_op_layers = nr_on_layers = nr_gp_layers = nr_gn_layers = 0;
	nr_excl_layers = 0;
	min_open_layer_disallow_open_after_ns = (u64)-1;
	min_open_layer_disallow_preempt_after_ns = (u64)-1;

	for (id = 0; id < nr_layers; id++) {
		struct layer *layer = &layers[id];

		if (layer->kind == LAYER_KIND_OPEN) {
			if (layer->preempt)
				op[nr_op_layers++] = id;
			else
				on[nr_on_layers++] = id;
			if (layer->disallow_open_after_ns <
			    min_open_layer_disallow_open_after_ns)
				min_open_layer_disallow_open_after_ns =
					layer->disallow_open_after_ns;
			if (layer->disallow_preempt_after_ns <
			    min_open_layer_disallow_preempt_after_ns)
				min_open_layer_disallow_preempt_after_ns =
					layer->disallow_preempt_after_ns;
		} else if (layer->kind == LAYER_KIND_GROUPED) {
			if (layer->preempt)
				gp[nr_gp_layers++] = id;
			else
				gn[nr_gn_layers++] = id;
		}
		if (layer->excl)
			nr_excl_layers++;
	}

	/* Weight-ascending iteration order, stable in layer id (main.rs). */
	for (i = 0; i < nr_layers; i++)
		order[i] = i;
	for (i = 1; i < nr_layers; i++) {
		u32 k = order[i];

		j = i;
		while (j > 0 && layers[order[j - 1]].weight > layers[k].weight) {
			order[j] = order[j - 1];
			j--;
		}
		order[j] = k;
	}
	for (i = 0; i < MAX_LAYERS; i++)
		layer_iteration_order[i] = i < nr_layers ? order[i] : 0;

	/* Per-CPU scan orders (see layered_fill_one_order). */
	for (cpu = 0; cpu < layered_nr_sim_cpus && cpu < LAYERED_MAX_SIM_CPUS; cpu++) {
		struct cpu_ctx *cpuc = &layered_cpu_ctxs[cpu];
		u32 ogp[MAX_LAYERS], ogn[MAX_LAYERS];
		u32 n_ogp = 0, n_ogn = 0;

		for (i = 0; i < nr_op_layers; i++)
			ogp[n_ogp++] = op[i];
		for (i = 0; i < nr_gp_layers; i++)
			ogp[n_ogp++] = gp[i];
		for (i = 0; i < nr_on_layers; i++)
			ogn[n_ogn++] = on[i];
		for (i = 0; i < nr_gn_layers; i++)
			ogn[n_ogn++] = gn[i];

		layered_fill_one_order(cpuc->ogp_layer_order, ogp, n_ogp, cpu);
		layered_fill_one_order(cpuc->ogn_layer_order, ogn, n_ogn, cpu);
		layered_fill_one_order(cpuc->op_layer_order, op, nr_op_layers, cpu);
		layered_fill_one_order(cpuc->on_layer_order, on, nr_on_layers, cpu);
		layered_fill_one_order(cpuc->gp_layer_order, gp, nr_gp_layers, cpu);
		layered_fill_one_order(cpuc->gn_layer_order, gn, nr_gn_layers, cpu);
	}
}

/*
 * Publish the topology tables. Callable from Rust after load and before
 * Simulator::run(); `layered_setup()` calls it with a flat 1-LLC / 1-node /
 * no-SMT layout so the scheduler is usable without any extra configuration.
 */
void layered_set_topology(unsigned int nr_cpus, unsigned int cpus_per_llc,
			  unsigned int nr_numa_nodes, unsigned int threads_per_core)
{
	u32 cpu, llc, node, total_llcs;

	if (nr_cpus == 0 || nr_cpus > LAYERED_MAX_SIM_CPUS)
		return;
	if (cpus_per_llc == 0)
		cpus_per_llc = nr_cpus;
	if (nr_numa_nodes == 0)
		nr_numa_nodes = 1;
	if (threads_per_core == 0)
		threads_per_core = 1;

	total_llcs = (nr_cpus + cpus_per_llc - 1) / cpus_per_llc;
	if (total_llcs > MAX_LLCS)
		total_llcs = MAX_LLCS;
	if (nr_numa_nodes > total_llcs)
		nr_numa_nodes = total_llcs;
	if (nr_numa_nodes > MAX_NUMA_NODES)
		nr_numa_nodes = MAX_NUMA_NODES;

	layered_nr_sim_cpus = nr_cpus;
	layered_cpus_per_llc = cpus_per_llc;
	layered_llcs_per_node = (total_llcs + nr_numa_nodes - 1) / nr_numa_nodes;
	layered_threads_per_core = threads_per_core;

	nr_cpu_ids = nr_cpus;
	nr_possible_cpus = nr_cpus;
	nr_llcs = total_llcs;
	nr_nodes = nr_numa_nodes;
	smt_enabled = threads_per_core > 1;
	has_little_cores = false;

	/* all_cpus bitmap + per-CPU identity, LLC/node maps, SMT siblings. */
	memset((void *)all_cpus, 0, sizeof(all_cpus));
	memset((void *)numa_cpumasks, 0, sizeof(numa_cpumasks));
	memset((void *)cpu_llc_id_map, 0, sizeof(cpu_llc_id_map));
	memset((void *)llc_numa_id_map, 0, sizeof(llc_numa_id_map));

	for (cpu = 0; cpu < nr_cpus; cpu++) {
		struct cpu_ctx *cpuc = &layered_cpu_ctxs[cpu];

		((volatile unsigned char *)all_cpus)[cpu / 8] |=
			(unsigned char)(1 << (cpu % 8));
		cpu_llc_id_map[cpu] = layered_cpu_llc(cpu);
		numa_cpumasks[layered_cpu_node(cpu)][cpu / 64] |=
			1ULL << (cpu % 64);

		cpuc->cpu = (s32)cpu;
		/* MAX_LAYERS is production's "no layer yet" sentinel. */
		cpuc->layer_id = MAX_LAYERS;
		cpuc->llc_id = layered_cpu_llc(cpu);
		cpuc->node_id = layered_cpu_node(cpu);
		cpuc->is_big = false;
		layered_fill_cpu_prox_map(cpuc, cpu, nr_cpus);
	}

	/*
	 * __sibling_cpu[cpu] is the SMT partner, or -1 when SMT is off. The
	 * engine lays siblings out as consecutive blocks of threads_per_core
	 * CPUs (see engine.rs build_cpus), so the partner of `cpu` within its
	 * core block is the other thread. With >2 threads per core the kernel
	 * only records one partner; take the next one, wrapping in-core.
	 */
	for (cpu = 0; cpu < MAX_CPUS; cpu++)
		__sibling_cpu[cpu] = -1;
	if (threads_per_core > 1) {
		for (cpu = 0; cpu < nr_cpus; cpu++) {
			u32 base = (cpu / threads_per_core) * threads_per_core;
			u32 off = (cpu - base + 1) % threads_per_core;

			__sibling_cpu[cpu] = (s32)(base + off);
		}
	}

	for (llc = 0; llc < total_llcs; llc++) {
		llc_numa_id_map[llc] =
			layered_llcs_per_node ? llc / layered_llcs_per_node : 0;
		layered_fill_llc_prox_maps(&layered_llc_ctxs[llc], llc, total_llcs);
	}
	for (node = 0; node < nr_numa_nodes; node++)
		layered_fill_node_prox_map(&layered_node_ctxs[node], node,
					   nr_numa_nodes);

	/* fallback_cpus[node] — the CPU layered parks work on when a node has
	 * no layer CPUs. Production picks one from the node; use its first. */
	for (node = 0; node < MAX_NUMA_NODES; node++)
		fallback_cpus[node] = 0;
	for (cpu = 0; cpu < nr_cpus; cpu++) {
		u32 n = layered_cpu_node(cpu);

		if (fallback_cpus[n] == 0)
			fallback_cpus[n] = cpu;
	}
}

/*
 * Combined setup, called automatically by DynamicScheduler::load() before
 * ops.init. Establishes production's default rodata, a flat topology, and a
 * single catch-all OPEN layer so `layered` is runnable with no further
 * configuration. Tests override topology and layers afterwards.
 */
void layered_setup(unsigned int num_cpus)
{
	/* Clear every static map array so a reloaded .so starts clean. */
	memset(layered_cpu_ctxs, 0, sizeof(layered_cpu_ctxs));
	memset(layered_node_ctxs, 0, sizeof(layered_node_ctxs));
	memset(layered_llc_ctxs, 0, sizeof(layered_llc_ctxs));
	memset(layered_layer_cpumasks, 0, sizeof(layered_layer_cpumasks));
	memset(layered_layer_node_cpumasks, 0, sizeof(layered_layer_node_cpumasks));
	memset(layered_hint_to_layer, 0, sizeof(layered_hint_to_layer));
	memset(layered_antistall_dsq, 0, sizeof(layered_antistall_dsq));
	memset(layered_antistall_max_delay, 0, sizeof(layered_antistall_max_delay));
	memset(layered_timer_wrappers, 0, sizeof(layered_timer_wrappers));
	memset(layered_task_ctxs, 0, sizeof(layered_task_ctxs));
	memset(layered_task_ctx_in_use, 0, sizeof(layered_task_ctx_in_use));
	memset(layered_task_hints, 0, sizeof(layered_task_hints));
	memset(layered_task_hint_in_use, 0, sizeof(layered_task_hint_in_use));

	layered_sim_timer_cb = NULL;
	layered_sim_timer_map = NULL;
	layered_sim_timer_ptr = NULL;
	layered_timer_fires = 0;
	/* Restore the production antistall timer interval; a previous run in
	 * this process may have shortened it via layered_set_antistall(). */
	layered_timers[ANTISTALL_TIMER].interval_ns = 15ULL * NSEC_PER_SEC;

	/* rodata defaults, mirroring scx_layered's Opts defaults (main.rs). */
	debug = 0;
	slice_ns = 20000000;			/* --slice-us 20000 */
	max_exec_ns = 20 * 20000000;
	monitor_disable = true;			/* no stats reader in-sim */
	enable_antistall = true;
	antistall_sec = 3;
	enable_match_debug = false;
	enable_gpu_support = false;
	nr_cgroup_regexes = 0;
	lo_fb_wait_ns = 5000000;
	lo_fb_share_ppk = 128;
	percpu_kthread_preempt = true;
	percpu_kthread_preempt_all = false;
	membw_event = 0;
	task_hint_map_enabled = false;
	enable_hi_fb_thread_name_match = false;
	kfuncs_supported_in_syscall = true;
	layered_root_tgid = 0;
	system_cpu_util_ewma = 0;
	layer_refresh_seq_avgruntime = 0;
	/*
	 * ext_sched_class_addr / idle_sched_class_addr let layered skip
	 * preempting CPUs running a higher scheduling class. The simulator has
	 * no other scheduling classes, so leaving them 0 (production's
	 * "couldn't resolve the symbols" state) disables the check — which is
	 * the correct answer here, not a stub.
	 */
	ext_sched_class_addr = 0;
	idle_sched_class_addr = 0;

	layered_register_hash_maps();
	layered_reset_layers_internal();
	layered_set_topology(num_cpus, 0, 1, 1);

	/* Default configuration: one catch-all OPEN layer, as scx_layered's
	 * own example configs end with. An OR group with zero AND rules
	 * matches every task (see match_layer()). */
	layered_add_layer("default", LAYER_KIND_OPEN, /*preempt=*/0,
			  /*preempt_first=*/0, /*excl=*/0,
			  DEFAULT_LAYER_WEIGHT, 0, 0, 0, GROWTH_ALGO_LINEAR,
			  /*is_protected=*/0);
	layers[0].nr_match_ors = 1;
	layers[0].matches[0].nr_match_ands = 0;
}

/*
 * ops.init shim.
 *
 * Runs the layer/CPU publication that scx_layered's userspace performs just
 * before attach, then the real BPF ops.init, then the two post-attach steps
 * userspace performs immediately after (refresh_layer_cpumasks via
 * BPF_PROG_RUN, and one refresh_node_ctx per node). Splitting it this way
 * keeps every step on the same side of ops.init as it is in production.
 */
int layered_init(void)
{
	u32 id, node, i;
	int ret;

	/* Pre-init: finalize the layer table and the static CPU allocation. */
	layered_auto_allocate_cpus(layered_nr_sim_cpus);
	for (id = 0; id < nr_layers; id++)
		layered_publish_layer_cpus(id, layered_nr_sim_cpus);
	layered_finalize_layer_rodata();

	ret = layered_bpf_init();
	if (ret)
		return ret;

	/* Post-attach: publish the layer cpumasks into the BPF kptrs. */
	refresh_layer_cpumasks(NULL);

	/* Post-attach: tell each node which layers have no CPUs on it. */
	for (node = 0; node < nr_nodes && node < MAX_NUMA_NODES; node++) {
		struct refresh_node_ctx_arg arg;

		memset(&arg, 0, sizeof(arg));
		arg.node_id = node;
		arg.init = 1;
		arg.nr_llcs = 0;
		for (i = 0; i < nr_llcs && i < MAX_LLCS; i++) {
			if (llc_numa_id_map[i] == node)
				arg.llcs[arg.nr_llcs++] = i;
		}
		arg.nr_empty_layer_ids = 0;
		for (id = 0; id < nr_layers && id < MAX_LAYERS; id++) {
			if (layers[id].node[node].nr_cpus == 0)
				arg.empty_layer_ids[arg.nr_empty_layer_ids++] = id;
		}
		ret = refresh_node_ctx(&arg);
		if (ret)
			return ret;
	}

	return 0;
}
