/*
 * cosmos_wrapper.c - Wrapper to compile scx_cosmos as userspace C
 *
 * This file includes the simulator wrapper infrastructure and then
 * the actual scheduler source. The header guards in common.bpf.h
 * prevent re-inclusion, so our overridden macros take effect.
 *
 * NOTE: This file is compiled with -Dconst= to strip const qualifiers.
 * BPF schedulers declare globals as "const volatile" (patched by the
 * BPF loader). Stripping const makes them writable from Rust.
 */
#include "sim_wrapper.h"
#include "sim_task.h"

/* Simulator CPU count; sizes the per-CPU map registration (SCX_REGISTER_PERCPU). */
#define MAX_SIM_CPUS 128

/*
 * Simulated PMU kfunc stubs.
 *
 * The new COSMOS API uses scx_pmu_read() kfunc instead of the old
 * start_readings map + bpf_perf_event_read_value() approach.
 *
 * We simulate PMU counters using scx_bpf_now() as a monotonic counter.
 * The delta between event_start and event_stop represents task runtime
 * in nanoseconds, which serves as a simulated "event count".
 *
 * Tasks with runtime > perf_threshold are classified as "event heavy"
 * and routed to the least-busy-event CPU.
 */
extern u64 scx_bpf_now(void);

/* Per-task PMU baseline storage (indexed by task pointer hash) */
#define PMU_TASK_HASH_SIZE 1024
static u64 pmu_task_baseline[PMU_TASK_HASH_SIZE];

static inline unsigned int pmu_task_hash(struct task_struct *p)
{
	return ((unsigned long)p >> 4) % PMU_TASK_HASH_SIZE;
}

/*
 * scx_pmu_install - Install a PMU event for tracking.
 * In simulation, this is a no-op since we use scx_bpf_now() as counter.
 */
int scx_pmu_install(u64 event)
{
	(void)event;
	return 0;
}

/*
 * scx_pmu_uninstall - Uninstall a PMU event.
 * No-op in simulation.
 */
int scx_pmu_uninstall(u64 event)
{
	(void)event;
	return 0;
}

/*
 * scx_pmu_task_init - Initialize per-task PMU tracking.
 * No-op in simulation.
 */
int scx_pmu_task_init(struct task_struct *p)
{
	(void)p;
	return 0;
}

/*
 * scx_pmu_task_fini - Finalize per-task PMU tracking.
 * No-op in simulation.
 */
int scx_pmu_task_fini(struct task_struct *p)
{
	(void)p;
	return 0;
}

/*
 * scx_pmu_event_start - Record baseline counter when task starts running.
 * Stores current scx_bpf_now() value as baseline.
 */
int scx_pmu_event_start(struct task_struct *p, bool update)
{
	(void)update;
	pmu_task_baseline[pmu_task_hash(p)] = scx_bpf_now();
	return 0;
}

/*
 * scx_pmu_event_stop - Mark end of PMU event tracking for task.
 * The actual reading happens in scx_pmu_read().
 */
int scx_pmu_event_stop(struct task_struct *p)
{
	(void)p;
	return 0;
}

/*
 * scx_pmu_read - Read PMU counter delta for a task.
 *
 * Returns the difference between current time and baseline (task runtime).
 * If clear=true, resets the baseline for the next measurement.
 */
int scx_pmu_read(struct task_struct *p, u64 event, u64 *value, bool clear)
{
	unsigned int hash = pmu_task_hash(p);
	u64 now = scx_bpf_now();
	u64 baseline = pmu_task_baseline[hash];

	(void)event;

	/* Return delta since event_start */
	if (now >= baseline)
		*value = now - baseline;
	else
		*value = 0;

	if (clear)
		pmu_task_baseline[hash] = now;

	return 0;
}

/*
 * bpf_map_lookup_elem override: route COSMOS map lookups to the test-map
 * registry (the simulator's stand-in for kernel BPF maps).
 */
static void *cosmos_map_lookup(void *map, const void *key)
{
	return scx_test_map_lookup_elem(map, key);
}
#undef bpf_map_lookup_elem
#define bpf_map_lookup_elem(map, key) cosmos_map_lookup((void *)(map), key)

/*
 * Include COSMOS interface header, then the scheduler source.
 * common.bpf.h is already included (header guard set), so our
 * BPF_STRUCT_OPS and SCX_OPS_DEFINE overrides are in effect.
 *
 * We include a patched copy of main.bpf.c that guards against
 * division-by-zero in update_freq(). BPF division-by-zero returns 0;
 * native C crashes with SIGFPE. The patched copy is generated into OUT_DIR
 * by build_schedulers (and into this dir by the legacy config.mk/make path).
 * The ANGLE include resolves it via -I (-I<OUT_DIR> for the cargo build,
 * -I<this dir> for make) rather than the includer's directory, so a stale
 * gitignored source-tree copy never shadows the freshly generated one and an
 * embedder can build cosmos from a read-only copy of sim.
 */
#include "intf.h"
#include <cosmos_main_patched.c>


/*
 * scx_bpf_cpu_node(): map a CPU to its NUMA node id.
 *
 * Upstream sched-ext/scx 36d589bb ("scx_cosmos: Enable full built-in
 * NUMA-aware idle CPU selection") introduced calls to scx_bpf_cpu_node()
 * behind __COMPAT_scx_bpf_cpu_node(). That COMPAT macro calls the kfunc when
 * bpf_ksym_exists(scx_bpf_cpu_node) holds; because this wrapper defines
 * scx_bpf_cpu_node (below), libbpf's !!sym is true, so the macro calls it — the
 * simulator must provide it (this function), or the call would jump through a
 * NULL weak __ksym symbol and SIGSEGV (test_numa_topology).
 *
 * Resolve the node from the wrapper's cpu_node_map (populated by
 * cosmos_configure_numa()); fall back to node 0 when the CPU is unmapped
 * (NUMA disabled, or a single-node topology). bpf_map_lookup_elem is the
 * cosmos_map_lookup override defined above, so this stays consistent with
 * the rest of the wrapper's map handling.
 */
s32 scx_bpf_cpu_node(s32 cpu)
{
	u32 key = (u32)cpu;
	u32 *node = bpf_map_lookup_elem(&cpu_node_map, &key);

	return node ? (s32)*node : 0;
}

/*
 * Register the COSMOS BPF maps with the test-map registry. task_ctx_stor
 * (TASK_STORAGE) and cpu_node_map (HASH) are create-on-demand; node_ctx_stor
 * (ARRAY) is kernel-preallocated, so it is pre-seeded with zeroed entries that
 * init_node() looks up during cosmos_init(); cpu_ctx_stor (PERCPU_ARRAY, one
 * entry per CPU) is likewise seeded.
 */
/*
 * gpu_pid_map is registered lazily by cosmos_add_gpu_task() rather than in
 * cosmos_register_maps(), so runs without GPU tasks (the common case) do not
 * pay the extra map registration. This matters for reproducibility: registering
 * an otherwise-unused map for every run perturbs the C-heap allocation pattern
 * enough to expose a latent address-sensitivity in exact two-run trace
 * comparisons (test_cosmos_domain_determinism); see mb sim-c63e46. The
 * scheduler observes identical behaviour either way — an empty/absent gpu_pid_map
 * both make gpu_node_by_pid() return -ENOENT. This flag is reset per run in
 * cosmos_register_maps() (which scx_test_map_clear_all()s the whole registry).
 *
 * The other maps moved to the generic SCX_REGISTER_* macros (which own their
 * descriptors internally); gpu_pid_map keeps a caller-named descriptor because
 * it is registered from cosmos_add_gpu_task(), not from cosmos_register_maps().
 */
static struct scx_test_map gpu_pid_test_map;
static bool gpu_pid_map_registered;

/*
 * Register the COSMOS BPF maps with the test map infrastructure.
 */
void cosmos_register_maps(void)
{
	scx_test_map_clear_all();
	gpu_pid_map_registered = false;

	SCX_REGISTER_STORAGE(task_ctx_stor);

	/*
	 * cpu_util_map (BPF_MAP_TYPE_ARRAY): per-CPU user utilization in
	 * [0..1024], written periodically by cosmos userspace (main.rs poll
	 * loop) and read by is_cpu_busy(). Pre-seeded so the scheduler can
	 * always look it up; cosmos_set_cpu_util() lets tests play userspace's
	 * role and drive the busy/deadline-mode path.
	 */
	SCX_REGISTER_ARRAY(cpu_util_map, true);

	SCX_REGISTER_ARRAY(node_ctx_stor, true);
	SCX_REGISTER_ARRAY(cpu_node_map, false);
	SCX_REGISTER_PERCPU(cpu_ctx_stor, true);

	/*
	 * Upstream scx_cosmos dropped deferred CPU wakeups and removed the
	 * `wakeup_timer` object (sched-ext/scx 79f892807cff "Deprecate deferred
	 * CPU wakeup" + 225b98c0 "Remove unused wakeup_timer"). COSMOS now has
	 * no BPF timer, so there is nothing to wire up here and no
	 * cosmos_fire_timer symbol is emitted; the Rust side resolves
	 * `fire_timer` with try_get! into an Option, so its absence is expected
	 * (ffi.rs `fire_timer: Option<FireTimerFn>`).
	 */
}

/*
 * Combined setup function called from Rust before cosmos_init().
 * Registers maps and enables CPU 0 in the primary domain; the config globals are
 * written before run by the manifest apply_rodata path (scheduler_manifest.rs
 * cosmos.runtime.rodata), not here. num_cpus is unused (cosmos has no
 * CPU-count-derived rodata) but kept for the generic {prefix}_setup signature.
 */
void cosmos_setup(unsigned int num_cpus)
{
	struct cpu_arg arg = { .cpu_id = 0 };

	(void)num_cpus;

	cosmos_register_maps();
	enable_primary_cpu(&arg);
}

/*
 * Configure NUMA topology after setup.
 * Populates cpu_node_map with sequential grouping:
 * CPUs [0, cpus_per_node) → node 0, etc.
 * Enables NUMA-aware scheduling in COSMOS.
 */
void cosmos_configure_numa(unsigned int num_cpus, unsigned int nr_nodes)
{
	unsigned int cpus_per_node, cpu, node;

	if (nr_nodes <= 1)
		return;  /* leave numa_enabled=false */

	cpus_per_node = num_cpus / nr_nodes;
	for (cpu = 0; cpu < num_cpus; cpu++) {
		node = cpu / cpus_per_node;
		if (node >= nr_nodes)
			node = nr_nodes - 1;
		bpf_map_update_elem(&cpu_node_map, &cpu, &node, 0);
	}

	numa_enabled = true;
	nr_node_ids = nr_nodes;
}

/*
 * Test knob: select COSMOS's lightweight idle-CPU scan paths.
 *
 * Mirrors the production `scx_cosmos --flat-idle-scan` / `--preferred-idle-scan`
 * options (see scx_cosmos main.rs: rodata.flat_idle_scan / preferred_idle_scan).
 * When either is enabled and prev_cpu is not busy, pick_idle_cpu() routes to
 * pick_idle_cpu_flat()/pick_idle_cpu_pref_smt() instead of the
 * scx_bpf_select_cpu_and() kfunc path (main.bpf.c pick_idle_cpu(), line ~842).
 *
 * When @preferred is set, production fills preferred_cpus[] from the topology's
 * capacity/locality ordering. The simulator has no such ordering to import, so
 * we seed an identity ranking (preferred_cpus[i] = i) which is a valid ordering
 * and makes pick_idle_cpu_pref_smt() visit every CPU.
 */
void cosmos_set_idle_scan(unsigned int num_cpus, int flat, int preferred)
{
	unsigned int i;

	flat_idle_scan = flat ? true : false;
	preferred_idle_scan = preferred ? true : false;

	if (preferred) {
		for (i = 0; i < num_cpus && i < MAX_CPUS; i++)
			preferred_cpus[i] = i;
	}
}

/*
 * Test knob: install an asymmetric (big.LITTLE) per-CPU capacity table.
 *
 * Mirrors scx_cosmos userspace, which normalizes each CPU's capacity to
 * [1, 1024] and writes rodata.cpu_capacity[cpu] (main.rs ~line 614), setting
 * all_cpus_same_capacity=false when the machine has heterogeneous cores. With
 * this in place COSMOS's is_cpu_faster()/scale_by_cpu_capacity() compare real
 * per-CPU capacities (main.bpf.c line ~665/1357).
 */
void cosmos_set_cpu_capacity(unsigned int num_cpus, const unsigned long long *caps)
{
	unsigned int i;

	all_cpus_same_capacity = false;
	for (i = 0; i < num_cpus && i < MAX_CPUS; i++)
		cpu_capacity[i] = caps[i];
}

/*
 * Test knob: populate per-CPU SMT sibling masks.
 *
 * Mirrors scx_cosmos's init_smt_domains() (main.rs), which walks the topology's
 * SMT siblings and calls the enable_sibling_cpu SEC("syscall") prog once per
 * (cpu, sibling) pair. The simulator lays SMT siblings out as consecutive
 * blocks of @threads_per_core CPUs per core (engine.rs build_cpus), so we
 * reproduce that grouping here and invoke enable_sibling_cpu() for every
 * ordered sibling pair within each core, exactly as userspace does at init.
 */
/*
 * Test knob: set per-CPU user utilization (the signal cosmos userspace polls
 * and writes into cpu_util_map every --polling-ms; see main.rs). @util is on
 * the production [0..1024] scale. is_cpu_busy(cpu) returns true when
 * cpu_util_map[cpu] >= busy_threshold, switching COSMOS from per-CPU
 * round-robin queues to the global deadline queue (task_dl / shared DSQ).
 *
 * The simulator does not yet compute per-CPU utilization automatically
 * (mb sim-642cb2), so tests set it explicitly to match their workload — e.g.
 * a saturated oversubscribed run sets util near 1024.
 */
void cosmos_set_cpu_util(unsigned int num_cpus, unsigned long long util)
{
	unsigned int cpu;

	for (cpu = 0; cpu < num_cpus && cpu < MAX_CPUS; cpu++)
		bpf_map_update_elem(&cpu_util_map, &cpu, &util, 0);
}

void cosmos_enable_smt_siblings(unsigned int num_cpus, unsigned int threads_per_core)
{
	unsigned int base, a, b;

	if (threads_per_core < 2)
		return;

	for (base = 0; base + threads_per_core <= num_cpus; base += threads_per_core) {
		for (a = 0; a < threads_per_core; a++) {
			for (b = 0; b < threads_per_core; b++) {
				struct domain_arg arg;

				if (a == b)
					continue;
				arg.cpu_id = (s32)(base + a);
				arg.sibling_cpu_id = (s32)(base + b);
				enable_sibling_cpu(&arg);
			}
		}
	}
}

/*
 * Test knob: register a GPU task's preferred NUMA node in gpu_pid_map.
 *
 * Mirrors scx_cosmos userspace, which reads the NVML GPU-process list and
 * writes pid -> node entries into gpu_pid_map (see scx_cosmos main.rs GPU
 * affinity handling). With an entry present, gpu_node_by_pid() returns @node,
 * so cosmos_select_cpu()'s GPU-affinity branch (main.bpf.c ~1085) calls
 * pick_cpu_on_gpu_node() -> can_use_node() for @pid, exercising the per-node
 * cpumask restriction that is otherwise unreachable under sim (mb sim-c63e46).
 *
 * @node must be a valid NUMA node id (requires cosmos_with_numa()). Must be
 * called after construction (cosmos_setup/cosmos_configure_numa) and before
 * Simulator::run(). Registers gpu_pid_map with the test-map infra on first use
 * (see gpu_pid_map_registered above for why registration is lazy).
 */
void cosmos_add_gpu_task(unsigned int pid, unsigned int node)
{
	u32 key = pid, val = node;

	if (!gpu_pid_map_registered) {
		INIT_SCX_TEST_MAP(&gpu_pid_test_map, gpu_pid_map);
		scx_test_map_register(&gpu_pid_test_map, &gpu_pid_map);
		gpu_pid_map_registered = true;
	}
	bpf_map_update_elem(&gpu_pid_map, &key, &val, 0);
}
