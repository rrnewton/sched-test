#include <stdbool.h>

#include "kern_types.h"
#include "sim_rbc_guard.h"

#ifndef BITS_PER_LONG
#define BITS_PER_LONG (sizeof(unsigned long) * 8)
#endif

#ifndef NR_CPUS
#define NR_CPUS 128
#endif

struct cpumask {
	unsigned long bits[128];
};

static __thread struct cpumask all_cpus = { 0 };
static __thread struct cpumask idle_smtmask = { 0 };
static __thread struct cpumask idle_cpumask = { 0 };

static void cpumask_set_cpu(int cpu, struct cpumask *mask)
{
	if (cpu < 0 || cpu >= NR_CPUS) {
		return;
	}
	mask->bits[cpu / BITS_PER_LONG] |= (1UL << (cpu % BITS_PER_LONG));
}

static void cpumask_clear_cpu(int cpu, struct cpumask *mask)
{
	if (cpu < 0 || cpu >= NR_CPUS) {
		return;
	}
	mask->bits[cpu / BITS_PER_LONG] &= ~(1UL << (cpu % BITS_PER_LONG));
}

static bool cpumask_test_cpu(int cpu, const struct cpumask *mask)
{
	if (cpu < 0 || cpu >= NR_CPUS) {
		return false;
	}
	return (mask->bits[cpu / BITS_PER_LONG] & (1UL << (cpu % BITS_PER_LONG))) != 0;
}

/* --- Engine-facing functions (NOT kfuncs, no RBC guard) --- */

void scx_test_set_all_cpumask(int cpu)
{
	cpumask_set_cpu(cpu, &all_cpus);
}

void scx_test_set_idle_smtmask(int cpu)
{
	cpumask_set_cpu(cpu, &idle_smtmask);
}

void scx_test_clear_idle_smtmask(int cpu)
{
	cpumask_clear_cpu(cpu, &idle_smtmask);
}

void scx_test_set_idle_cpumask(int cpu)
{
	cpumask_set_cpu(cpu, &idle_cpumask);
}

void scx_test_clear_idle_cpumask(int cpu)
{
	cpumask_clear_cpu(cpu, &idle_cpumask);
}

void scx_test_cpumask_set(int cpu, struct cpumask *cpumask)
{
	cpumask_set_cpu(cpu, cpumask);
}

/* --- BPF kfuncs (called from scheduler .so, RBC guard required) --- */

const struct cpumask *scx_bpf_get_idle_smtmask_node(int node __attribute__((unused)))
{
	/* No guard needed — just returns a pointer, no branches. */
	return &idle_smtmask;
}

const struct cpumask *scx_bpf_get_idle_smtmask(void)
{
	return &idle_smtmask;
}

const struct cpumask *scx_bpf_get_idle_cpumask(void)
{
	return &idle_cpumask;
}

bool scx_bpf_test_and_clear_cpu_idle(s32 cpu)
{
	RBC_GUARD_START;
	if (cpumask_test_cpu(cpu, &idle_cpumask)) {
		cpumask_clear_cpu(cpu, &idle_cpumask);
		RBC_GUARD_RETURN(true);
	}
	RBC_GUARD_RETURN(false);
}

bool bpf_cpumask_test_cpu(u32 cpu, const struct cpumask *cpumask)
{
	RBC_GUARD_START;
	RBC_GUARD_RETURN(cpumask_test_cpu(cpu, cpumask));
}

/*
 * bpf_cpumask_full - is every CPU set?
 *
 * The kernel's cpumask_full() tests bits [0, nr_cpu_ids), NOT the whole
 * fixed-size bitmap. Our struct cpumask is a fixed 128 unsigned longs
 * (8192 bits) but only the simulated CPUs are ever populated, so a naive
 * "all 8192 bits set" test would answer false for every mask and silently
 * change scheduler behaviour (scx_layered's
 * maybe_init_task_unprotected_mask() uses it to decide whether a task has
 * ANY placement restriction).
 *
 * `all_cpus` is the simulator's online set, populated by the engine via
 * scx_test_set_all_cpumask() for exactly the simulated CPUs, so it is the
 * faithful stand-in for the [0, nr_cpu_ids) bound: a mask is "full" iff it
 * covers every online CPU.
 *
 * Lives here rather than in scx-sim/csrc/sim_bpf_stubs.c (where the other
 * bpf_cpumask_* kfuncs are) because `all_cpus` is defined in this
 * translation unit; the scheduler .so resolves it from the main binary at
 * dlopen time via -rdynamic, like the other kfuncs below.
 */
bool bpf_cpumask_full(const struct cpumask *cpumask)
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, &all_cpus) && !cpumask_test_cpu(i, cpumask))
			RBC_GUARD_RETURN(false);
	}
	RBC_GUARD_RETURN(true);
}

s32 scx_bpf_pick_idle_cpu_node(const struct cpumask *cpus_allowed,
			       int node __attribute__((unused)),
			       u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, cpus_allowed) && cpumask_test_cpu(i, &idle_cpumask)) {
			RBC_GUARD_RETURN(i);
		}
	}
	RBC_GUARD_RETURN(-1);
}

s32 scx_bpf_pick_idle_cpu(const struct cpumask *cpus_allowed, u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, cpus_allowed) && cpumask_test_cpu(i, &idle_cpumask)) {
			RBC_GUARD_RETURN(i);
		}
	}
	RBC_GUARD_RETURN(-1);
}

s32 scx_bpf_pick_any_cpu_node(const struct cpumask *cpus_allowed,
			      int node __attribute__((unused)),
			      u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, cpus_allowed))
			RBC_GUARD_RETURN(i);
	}
	RBC_GUARD_RETURN(-1);
}

s32 scx_bpf_pick_any_cpu(const struct cpumask *cpus_allowed,
			 u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	RBC_GUARD_RETURN(scx_bpf_pick_any_cpu_node(cpus_allowed, 0, flags));
}

const struct cpumask *scx_bpf_get_online_cpumask(void)
{
	return &all_cpus;
}

const struct cpumask *scx_bpf_get_possible_cpumask(void)
{
	return &all_cpus;
}

const struct cpumask *scx_bpf_get_idle_cpumask_node(int node __attribute__((unused)))
{
	return &idle_cpumask;
}
