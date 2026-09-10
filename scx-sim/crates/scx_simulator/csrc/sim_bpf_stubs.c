/*
 * sim_bpf_stubs.c - Strong stub implementations for BPF helpers
 *
 * These override the __weak stubs in overrides.c with implementations
 * that actually work for the simulator. Used by schedulers that need
 * cpumask manipulation and kptr exchange.
 *
 * Every function here is a BPF kfunc in the real kernel. In the
 * simulator, each function pauses the PMU RBC counter on entry and
 * resumes it on exit, matching the kernel semantics where kfunc
 * execution is "kernel code" and should not be counted as scheduler
 * overhead.
 *
 * This file does NOT include sim_wrapper.h or any BPF headers to avoid
 * conflicts with bpf_helper_defs.h (which defines bpf_timer_* as static
 * function pointers). We only need basic types and the cpumask struct.
 */

/* Use kern_types.h for basic types (u32, s32, etc.) */
#include "kern_types.h"
#include "sim_kconfig_defaults.h"
#include <stdbool.h>
#include <stddef.h>

/* Reproduce the cpumask struct definition from scx_test_cpumask.c */
#ifndef BITS_PER_LONG
#define BITS_PER_LONG (sizeof(unsigned long) * 8)
#endif

/* NR_CPUS comes from kern_types.h — one definition for all three cpumask
 * translation units. See the comment there before changing it. */

struct cpumask {
	unsigned long bits[128];
};

/* bpf_cpumask is just a cpumask in the test infrastructure */
struct bpf_cpumask {
	unsigned long bits[128];
};

/* Deterministic bump allocator — replaces glibc calloc/free to avoid
 * nondeterministic PMU branch counts from glibc's heap management. */
#include "sim_arena.h"

/*
 * RBC counter pause/resume — defined in Rust (kfuncs.rs), resolved
 * from the main binary via -rdynamic. These disable/enable the PMU
 * retired-branch-conditional counter so that kfunc branches are not
 * counted as scheduler overhead.
 */
extern void sim_rbc_pause(void);
extern void sim_rbc_resume(void);

/* --- cpumask helpers --- */

struct bpf_cpumask *bpf_cpumask_create(void)
{
	struct bpf_cpumask *m;
	sim_rbc_pause();
	m = (struct bpf_cpumask *)sim_arena_calloc(sizeof(struct bpf_cpumask));
	sim_rbc_resume();
	return m;
}

void bpf_cpumask_release(struct bpf_cpumask *cpumask)
{
	sim_rbc_pause();
	sim_arena_free(cpumask);
	sim_rbc_resume();
}

void bpf_cpumask_set_cpu(u32 cpu, struct bpf_cpumask *cpumask)
{
	sim_rbc_pause();
	if (cpu < NR_CPUS)
		cpumask->bits[cpu / BITS_PER_LONG] |= (1UL << (cpu % BITS_PER_LONG));
	sim_rbc_resume();
}

void bpf_cpumask_clear_cpu(u32 cpu, struct bpf_cpumask *cpumask)
{
	sim_rbc_pause();
	if (cpu < NR_CPUS)
		cpumask->bits[cpu / BITS_PER_LONG] &= ~(1UL << (cpu % BITS_PER_LONG));
	sim_rbc_resume();
}

void bpf_cpumask_clear(struct bpf_cpumask *cpumask)
{
	sim_rbc_pause();
	__builtin_memset(cpumask, 0, sizeof(struct bpf_cpumask));
	sim_rbc_resume();
}

void bpf_cpumask_setall(struct bpf_cpumask *cpumask)
{
	sim_rbc_pause();
	__builtin_memset(cpumask, 0xff, sizeof(struct bpf_cpumask));
	sim_rbc_resume();
}

bool bpf_cpumask_test_cpu(u32 cpu, const struct cpumask *cpumask)
{
	bool r;
	sim_rbc_pause();
	if (cpu >= NR_CPUS)
		r = false;
	else
		r = !!(cpumask->bits[cpu / BITS_PER_LONG] & (1UL << (cpu % BITS_PER_LONG)));
	sim_rbc_resume();
	return r;
}

bool bpf_cpumask_empty(const struct cpumask *cpumask)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		if (cpumask->bits[i]) {
			sim_rbc_resume();
			return false;
		}
	}
	sim_rbc_resume();
	return true;
}

u32 bpf_cpumask_first(const struct cpumask *cpumask)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		if (cpumask->bits[i]) {
			unsigned long v = cpumask->bits[i];
			u32 bit = 0;
			while (!(v & 1)) {
				v >>= 1;
				bit++;
			}
			sim_rbc_resume();
			return i * (sizeof(unsigned long) * 8) + bit;
		}
	}
	sim_rbc_resume();
	/* No bits set — return >= nr_cpu_ids to signal "none found". */
	return 128 * sizeof(unsigned long) * 8;
}

u32 bpf_cpumask_weight(const struct cpumask *cpumask)
{
	u32 count = 0;
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		unsigned long v = cpumask->bits[i];
		while (v) {
			count += v & 1;
			v >>= 1;
		}
	}
	sim_rbc_resume();
	return count;
}

bool bpf_cpumask_and(struct bpf_cpumask *dst, const struct cpumask *src1,
		     const struct cpumask *src2)
{
	bool result = false;
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		dst->bits[i] = src1->bits[i] & src2->bits[i];
		if (dst->bits[i])
			result = true;
	}
	sim_rbc_resume();
	return result;
}

void bpf_cpumask_or(struct bpf_cpumask *dst, const struct cpumask *src1,
		    const struct cpumask *src2)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++)
		dst->bits[i] = src1->bits[i] | src2->bits[i];
	sim_rbc_resume();
}

void bpf_cpumask_xor(struct bpf_cpumask *dst, const struct cpumask *src1,
		     const struct cpumask *src2)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++)
		dst->bits[i] = src1->bits[i] ^ src2->bits[i];
	sim_rbc_resume();
}

void bpf_cpumask_copy(struct bpf_cpumask *dst, const struct cpumask *src)
{
	sim_rbc_pause();
	__builtin_memcpy(dst, src, sizeof(struct cpumask));
	sim_rbc_resume();
}

bool bpf_cpumask_subset(const struct cpumask *src1, const struct cpumask *src2)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		if (src1->bits[i] & ~src2->bits[i]) {
			sim_rbc_resume();
			return false;
		}
	}
	sim_rbc_resume();
	return true;
}

u32 bpf_cpumask_any_distribute(const struct cpumask *cpumask)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < NR_CPUS; i++) {
		if (cpumask->bits[i / BITS_PER_LONG] & (1UL << (i % BITS_PER_LONG))) {
			sim_rbc_resume();
			return i;
		}
	}
	sim_rbc_resume();
	return NR_CPUS;
}

u32 bpf_cpumask_any_and_distribute(const struct cpumask *src1,
				   const struct cpumask *src2)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < NR_CPUS; i++) {
		unsigned long bit = 1UL << (i % BITS_PER_LONG);
		unsigned long word = i / BITS_PER_LONG;
		if ((src1->bits[word] & bit) && (src2->bits[word] & bit)) {
			sim_rbc_resume();
			return i;
		}
	}
	sim_rbc_resume();
	return NR_CPUS;
}

bool bpf_cpumask_intersects(const struct cpumask *src1,
			    const struct cpumask *src2)
{
	unsigned int i;
	sim_rbc_pause();
	for (i = 0; i < 128; i++) {
		if (src1->bits[i] & src2->bits[i]) {
			sim_rbc_resume();
			return true;
		}
	}
	sim_rbc_resume();
	return false;
}

bool bpf_cpumask_test_and_set_cpu(u32 cpu, struct bpf_cpumask *cpumask)
{
	bool was_set;
	sim_rbc_pause();
	if (cpu >= NR_CPUS) {
		sim_rbc_resume();
		return false;
	}
	was_set = !!(cpumask->bits[cpu / BITS_PER_LONG] &
		     (1UL << (cpu % BITS_PER_LONG)));
	cpumask->bits[cpu / BITS_PER_LONG] |= (1UL << (cpu % BITS_PER_LONG));
	sim_rbc_resume();
	return was_set;
}

/*
 * scx_bpf_cpu_rq / scx_bpf_locked_rq: new kfuncs in common.bpf.h that
 * return a pointer to the CPU's runqueue. The simulator doesn't have real
 * runqueues; return NULL. Callers should handle NULL gracefully.
 */
struct rq;
struct rq *scx_bpf_cpu_rq(s32 cpu)
{
	(void)cpu;
	return (struct rq *)0;
}

struct rq *scx_bpf_locked_rq(void)
{
	return (struct rq *)0;
}

/*
 * bpf_cgroup_ancestor / bpf_cgroup_from_id: BPF kfuncs for cgroup lookup.
 * Return NULL in simulation — schedulers check for NULL returns.
 */
struct cgroup;
__attribute__((weak))
struct cgroup *bpf_cgroup_ancestor(struct cgroup *cgrp, int level)
{
	(void)cgrp; (void)level;
	return (struct cgroup *)0;
}

__attribute__((weak))
struct cgroup *bpf_cgroup_from_id(u64 id)
{
	(void)id;
	return (struct cgroup *)0;
}

/*
 * bpf_cgroup_acquire / bpf_cgroup_release: cgroup refcount kfuncs. The
 * single-threaded deterministic simulator models no cgroup refcounting, so
 * acquire is the identity and release is a no-op. Weak FUNCTION stubs (not
 * macros): bpf_experimental.h declares these extern __ksym, and a
 * function-like macro would mangle that declaration (see
 * schedulers/lavd/wrapper.c). Replace the former per-scheduler copies
 * (mitosis's sim_cgroup_acquire macro pair, lavd's local bpf_cgroup_release
 * fn).
 */
__attribute__((weak))
struct cgroup *bpf_cgroup_acquire(struct cgroup *cgrp)
{
	return cgrp;
}

__attribute__((weak))
void bpf_cgroup_release(struct cgroup *cgrp)
{
	(void)cgrp;
}

/*
 * LINUX_KERNEL_VERSION: __kconfig global declared in common.bpf.h.
 * With __kconfig stripped, it becomes a bare extern declaration.
 * Provide a definition so scheduler .so files link. Default 6.18.0 (encoded
 * major << 16 | minor << 8 | patch); an embedder overrides via
 * -DSIM_LINUX_KERNEL_VERSION (see sim_kconfig_defaults.h).
 */
int LINUX_KERNEL_VERSION = SIM_LINUX_KERNEL_VERSION;

/*
 * CONFIG_PREEMPT_RCU: __kconfig __weak bool from common.bpf.h.
 * Default false (simulator doesn't model preempt RCU); embedder overrides via
 * -DSIM_CONFIG_PREEMPT_RCU. Paired with LINUX_KERNEL_VERSION in
 * is_migration_disabled.
 */
bool CONFIG_PREEMPT_RCU = SIM_CONFIG_PREEMPT_RCU;
