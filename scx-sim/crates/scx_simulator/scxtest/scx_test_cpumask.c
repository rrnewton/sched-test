#include <stdbool.h>

#include "kern_types.h"
#include "sim_rbc_guard.h"

#ifndef BITS_PER_LONG
#define BITS_PER_LONG (sizeof(unsigned long) * 8)
#endif

/* NR_CPUS comes from kern_types.h — one definition for all three cpumask
 * translation units. See the comment there before changing it. */

/*
 * Deliberately NOT `NR_CPUS / BITS_PER_LONG`: vmlinux.h — which every
 * scheduler is compiled against — declares `struct cpumask { unsigned long
 * bits[128]; }` (CONFIG_NR_CPUS=8192). Matching that literal keeps the
 * engine's view of a cpumask at least as large as the scheduler's. Only the
 * first NR_CPUS bits are ever populated; `bpf_cpumask_full()` below relies on
 * exactly that.
 */
struct cpumask {
	unsigned long bits[128];
};

static __thread struct cpumask all_cpus = { 0 };
static __thread struct cpumask idle_smtmask = { 0 };
static __thread struct cpumask idle_cpumask = { 0 };

/*
 * Per-NUMA-node CPU membership, published by the engine from the scenario's
 * MachineTopology. Without it the `*_node` kfuncs below have no way to be
 * node-scoped and can only answer node-blind — which is what they did before
 * the engine had a NUMA model at all, silently, for any scheduler that called
 * them. (scx_layered does not: it uses nodec->cpumask and
 * lookup_layer_node_cpumask. scx_cosmos and anything else using the kernel's
 * node-scoped idle API does.)
 *
 * `node_cpus_valid` stays false until the engine publishes, so a caller that
 * never set up a topology keeps the old machine-wide answer rather than
 * getting an empty mask.
 */
static __thread struct cpumask node_cpus[MAX_SIM_NUMA_NODES] = { { { 0 } } };
static __thread bool node_cpus_valid = false;

/* Scratch results for the node-scoped mask getters. A kfunc returns a
 * borrowed pointer the scheduler reads immediately, exactly as the kernel's
 * scx_bpf_get_idle_cpumask_node() does; one buffer per mask kind per thread
 * is enough for that lifetime and keeps the substrate allocation-free. */
static __thread struct cpumask node_idle_cpumask = { 0 };
static __thread struct cpumask node_idle_smtmask = { 0 };

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

static void cpumask_and(struct cpumask *dst, const struct cpumask *a,
			const struct cpumask *b)
{
	for (unsigned int i = 0; i < sizeof(dst->bits) / sizeof(dst->bits[0]); i++)
		dst->bits[i] = a->bits[i] & b->bits[i];
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

/*
 * Publish which NUMA node a CPU is on. Called once per CPU at engine setup
 * from the scenario's MachineTopology; `scx_test_clear_cpu_nodes()` resets
 * between runs. Nodes at or above MAX_SIM_NUMA_NODES are dropped rather than
 * folded into node 0 — folding would make a too-large machine look correct.
 */
void scx_test_set_cpu_node(int cpu, unsigned int node)
{
	if (node >= MAX_SIM_NUMA_NODES)
		return;
	cpumask_set_cpu(cpu, &node_cpus[node]);
	node_cpus_valid = true;
}

void scx_test_clear_cpu_nodes(void)
{
	for (unsigned int n = 0; n < MAX_SIM_NUMA_NODES; n++)
		for (int i = 0; i < (int)(sizeof(node_cpus[n].bits) /
					  sizeof(node_cpus[n].bits[0])); i++)
			node_cpus[n].bits[i] = 0;
	node_cpus_valid = false;
}

/* True when `cpu` is on `node`, or when no topology was published (in which
 * case every CPU counts, preserving the node-blind answer). */
static bool cpu_on_node(int cpu, int node)
{
	if (!node_cpus_valid || node < 0 || node >= MAX_SIM_NUMA_NODES)
		return true;
	return cpumask_test_cpu(cpu, &node_cpus[node]);
}

/* --- BPF kfuncs (called from scheduler .so, RBC guard required) --- */

const struct cpumask *scx_bpf_get_idle_smtmask_node(int node)
{
	/* No guard needed — just returns a pointer, no branches. */
	if (!node_cpus_valid || node < 0 || node >= MAX_SIM_NUMA_NODES)
		return &idle_smtmask;
	cpumask_and(&node_idle_smtmask, &idle_smtmask, &node_cpus[node]);
	return &node_idle_smtmask;
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
			       int node,
			       u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, cpus_allowed) &&
		    cpumask_test_cpu(i, &idle_cpumask) && cpu_on_node(i, node)) {
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
			      int node,
			      u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	for (int i = 0; i < NR_CPUS; i++) {
		if (cpumask_test_cpu(i, cpus_allowed) && cpu_on_node(i, node))
			RBC_GUARD_RETURN(i);
	}
	RBC_GUARD_RETURN(-1);
}

s32 scx_bpf_pick_any_cpu(const struct cpumask *cpus_allowed,
			 u64 flags __attribute__((unused)))
{
	RBC_GUARD_START;
	/* The un-suffixed form is machine-wide. Passing -1 rather than 0 is
	 * load-bearing now that the node form is node-scoped: 0 would confine
	 * it to node 0, which is the bug class mb sim-dox34 belongs to. */
	RBC_GUARD_RETURN(scx_bpf_pick_any_cpu_node(cpus_allowed, -1, flags));
}

const struct cpumask *scx_bpf_get_online_cpumask(void)
{
	return &all_cpus;
}

const struct cpumask *scx_bpf_get_possible_cpumask(void)
{
	return &all_cpus;
}

const struct cpumask *scx_bpf_get_idle_cpumask_node(int node)
{
	if (!node_cpus_valid || node < 0 || node >= MAX_SIM_NUMA_NODES)
		return &idle_cpumask;
	cpumask_and(&node_idle_cpumask, &idle_cpumask, &node_cpus[node]);
	return &node_idle_cpumask;
}
