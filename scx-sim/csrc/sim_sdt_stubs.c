/*
 * sim_sdt_stubs.c - Simulator implementations of BPF arena / SDT task storage
 *
 * In the real kernel, scx_task_alloc/data/free use BPF arena memory and
 * task-local storage maps for per-task scheduler context. In the simulator,
 * we use malloc and a simple hash table keyed by task_struct pointer.
 *
 * These are strong definitions that override the __weak stubs in
 * lib/scxtest/overrides.c.
 *
 * This file does NOT include vmlinux.h or BPF headers to avoid type
 * conflicts. It only needs opaque pointers and basic types.
 */

/* Deterministic bump allocator — replaces glibc calloc/free to avoid
 * nondeterministic PMU branch counts from glibc's heap management. */
#include "sim_arena.h"

/* Use kern_types.h for basic types (u32, u64, etc.) */
#include "kern_types.h"

/* Opaque — we only handle pointers, never dereference task_struct here */
struct task_struct;

/* Declared in sim_task.c — gets PID from task pointer */
extern int sim_task_get_pid(struct task_struct *p);

/* RBC counter pause/resume — defined in Rust (kfuncs.rs), resolved via
 * -rdynamic. Pauses the PMU counter so kfunc branches are not counted. */
extern void sim_rbc_pause(void);
extern void sim_rbc_resume(void);

/*
 * Hash table for mapping task_struct* → allocated per-task context.
 *
 * Open-addressing with linear probing. Sized to ~50% load factor at
 * up to 8 192 concurrent live tasks. Bumped from 2 048 → 16 384 as part
 * of the Phase 1 BPF infra scale-up (tg
 * `scxsim-bpf-infra-scale-up-phase1`, design doc §Phase 1 item 4 in
 * `experiments/lavd_cpubw_stalls_202604/SCXSIM_REAL_CGROUP_BW_LIBRARY_DESIGN.md`).
 *
 * Why decoupled from CBW_NR_CGRP_MAX: per-task SDT slots are scaled by
 * task count, not cgroup count. The previous 2 048 limit happened to
 * coincide with `CBW_NR_CGRP_MAX = 2048` from the production cgroup_bw
 * library, but the two ceilings are unrelated. Phase 2 will compile in
 * the real cgroup_bw.bpf.c which can register up to 2 048 cgroups, each
 * potentially generating an entire task graph -- the SDT table sees the
 * sum of per-cgroup task counts, not the cgroup count itself.
 *
 * Why 16 384 specifically: gives 1.5x headroom over the BPF-map-pressure
 * follow-up §1 worst case (~10 000 concurrent tasks across 2 048 cgroups
 * in the cpu-bw-stall-bug stress matrix). Open-addressing probe distance
 * stays bounded under 50% load so determinism holds.
 *
 * Memory cost: SDT_HASH_SLOTS * sizeof(struct sdt_entry) = 16 384 * 16
 * bytes = 256 KB BSS. Negligible vs the 32 MB arena.
 */
#define SDT_HASH_SLOTS 16384
#define SDT_HASH_MASK (SDT_HASH_SLOTS - 1)

struct sdt_entry {
	struct task_struct *key; /* NULL = empty slot */
	void *data;             /* malloc'd per-task context */
};

static struct sdt_entry sdt_table[SDT_HASH_SLOTS];
static u64 sdt_data_size;
static int sdt_initialized;

/*
 * Test-only fault injection for scx_task_alloc().
 *
 * When nonzero, scx_task_alloc() returns NULL for the task whose PID
 * matches `sim_sdt_fail_pid`, reproducing the real arena / SDT-table
 * allocation failure behind scx GitHub #3564 (scx_lavd
 * `lavd_init_task` -> `scx_task_alloc()` returns NULL ->
 * `scx_bpf_error("task_ctx_stor first lookup failed")` + return
 * -ENOMEM). Default 0 = never fail (production behavior, byte-identical).
 *
 * The check lives AFTER sim_rbc_pause() (see scx_task_alloc below), so it
 * adds ZERO retired-branch-conditional counts to scheduler RBC
 * accounting and is therefore determinism-neutral when disabled. Exposed
 * (non-static) so the Rust test harness can arm/disarm it via FFI.
 */
int sim_sdt_fail_pid;

/*
 * Hash a task's PID for deterministic hash table placement.
 *
 * We hash by PID rather than pointer address because pointer addresses
 * are not deterministic between simulation runs (calloc returns different
 * addresses depending on heap state). Using PID ensures the hash table
 * lookup path is identical across runs, enabling deterministic instruction
 * counts.
 */
static unsigned long sdt_hash_pid(int pid)
{
	/* Multiplicative hash — golden ratio constant */
	unsigned long v = (unsigned long)pid;
	v ^= v >> 16;
	v *= 0x9e3779b97f4a7c15UL;
	v ^= v >> 32;
	return v & SDT_HASH_MASK;
}

/*
 * Find a slot in the hash table for the given task.
 *
 * Uses PID-based hashing for deterministic probe sequences, but stores
 * and matches by task_struct pointer for correctness (PIDs are unique
 * per-task but the pointer is the actual key).
 */
static struct sdt_entry *sdt_find_slot(struct task_struct *p)
{
	int pid = p ? sim_task_get_pid(p) : 0;
	unsigned long idx = sdt_hash_pid(pid);
	for (unsigned long i = 0; i < SDT_HASH_SLOTS; i++) {
		unsigned long slot = (idx + i) & SDT_HASH_MASK;
		if (sdt_table[slot].key == p || sdt_table[slot].key == (void *)0)
			return &sdt_table[slot];
	}
	return (void *)0; /* table full — should never happen */
}

/*
 * Initialize the per-task allocator. Called once during scheduler init.
 *
 * In the real kernel, this sets up the radix-tree allocator with arena
 * pages. In the simulator, we just record the data size for malloc.
 */
int scx_task_init(u64 data_size)
{
	sim_rbc_pause();
	sdt_data_size = data_size;
	__builtin_memset(sdt_table, 0, sizeof(sdt_table));
	sdt_initialized = 1;
	sim_rbc_resume();
	return 0;
}

/*
 * Allocate per-task scheduler context for a task.
 *
 * Returns a pointer to zero-initialized memory of sdt_data_size bytes,
 * or NULL on failure.
 */
void *scx_task_alloc(struct task_struct *p)
{
	struct sdt_entry *entry;
	void *data;

	sim_rbc_pause();

	/*
	 * Test-only fault injection (scx GitHub #3564 reproducer). Placed
	 * inside the sim_rbc_pause()/resume() window so it is RBC-neutral
	 * and does not perturb determinism when disabled (sim_sdt_fail_pid
	 * == 0). See sim_sdt_fail_pid declaration above.
	 */
	if (sim_sdt_fail_pid && p && sim_task_get_pid(p) == sim_sdt_fail_pid) {
		sim_rbc_resume();
		return (void *)0;
	}

	if (!sdt_initialized || !p) {
		sim_rbc_resume();
		return (void *)0;
	}

	data = sim_arena_calloc(sdt_data_size);
	if (!data) {
		sim_rbc_resume();
		return (void *)0;
	}

	entry = sdt_find_slot(p);
	if (!entry) {
		sim_arena_free(data);
		sim_rbc_resume();
		return (void *)0;
	}

	/* If slot already occupied by this key, free old data */
	if (entry->key == p && entry->data)
		sim_arena_free(entry->data);

	entry->key = p;
	entry->data = data;
	sim_rbc_resume();
	return data;
}

/*
 * Look up existing per-task context for a task.
 *
 * Returns NULL if the task has no allocated context.
 */
void *scx_task_data(struct task_struct *p)
{
	struct sdt_entry *entry;

	sim_rbc_pause();

	if (!sdt_initialized || !p) {
		sim_rbc_resume();
		return (void *)0;
	}

	entry = sdt_find_slot(p);
	if (!entry || entry->key != p) {
		sim_rbc_resume();
		return (void *)0;
	}

	sim_rbc_resume();
	return entry->data;
}

/*
 * Free per-task context when a task exits.
 */
void scx_task_free(struct task_struct *p)
{
	struct sdt_entry *entry;

	sim_rbc_pause();

	if (!sdt_initialized || !p) {
		sim_rbc_resume();
		return;
	}

	entry = sdt_find_slot(p);
	if (!entry || entry->key != p) {
		sim_rbc_resume();
		return;
	}

	sim_arena_free(entry->data);
	entry->key = (void *)0;
	entry->data = (void *)0;
	sim_rbc_resume();
}

/*
 * Arena subprogram initialization — no-op in the simulator.
 *
 * In the kernel, this works around a BPF verifier limitation by
 * forcing an LD.IMM instruction referencing the arena. Not needed
 * in userspace.
 */
void scx_arena_subprog_init(void)
{
}

/*
 * Reset global state to allow deterministic re-runs.
 *
 * The simulator binary links sim_sdt_stubs.c into the main executable,
 * so its static variables persist across simulation runs. This function
 * resets the SDT hash table state to what it would be after a fresh
 * scx_task_init() call with the same data_size.
 *
 * NOTE: This does NOT reset sdt_initialized to 0 because scx_task_init()
 * is only called during scheduler load (lavd_setup), not at the start of
 * each simulation run. If we set sdt_initialized=0, scx_task_alloc would
 * fail. Instead, we clear the hash table while keeping the initialization
 * state intact.
 */
void sim_sdt_reset(void)
{
	/* Clear the hash table to the same state as after scx_task_init().
	 * memset is deterministic (same instruction count regardless of
	 * current table contents), unlike iterating and checking each slot. */
	__builtin_memset(sdt_table, 0, sizeof(sdt_table));
	/* Reset the arena so the next run's allocations get the same
	 * addresses as the first run (deterministic bump pointer). */
	sim_arena_reset();
	/* Note: sdt_initialized and sdt_data_size are NOT reset here.
	 * They are set during scheduler load (scx_task_init) and must
	 * persist across simulation runs with the same scheduler. */
}
