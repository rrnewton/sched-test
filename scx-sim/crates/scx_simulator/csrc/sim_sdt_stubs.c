/*
 * sim_sdt_stubs.c - Simulator implementations of BPF arena / SDT task storage
 *
 * In the real kernel, the scx_task_* API (scx/lib/sdt_task.bpf.c) keeps each
 * task's scheduler context in BPF arena memory, found through a task-local
 * storage map. In the simulator, the context comes from the deterministic sim
 * arena and is found through a hash table keyed by task_struct pointer.
 *
 * These are strong definitions that override the __weak stubs in
 * scxtest/overrides.c (vendored in this crate).
 *
 * This file does NOT include vmlinux.h or BPF headers to avoid type
 * conflicts. It only needs opaque pointers and basic types.
 */

/* Deterministic bump allocator — replaces glibc calloc/free to avoid
 * nondeterministic PMU branch counts from glibc's heap management. */
#include "sim_arena.h"

/* Use kern_types.h for basic types (u32, u64, etc.) */
#include "kern_types.h"

#include <errno.h>
#include <stdio.h>

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
 * Memory cost: SDT_HASH_SLOTS * sizeof(struct sdt_entry) = 16 384 * 24
 * bytes = 384 KB BSS. Negligible vs the 32 MB arena.
 *
 * Removal uses backward-shift deletion (sdt_remove), not a plain clear: a
 * cleared slot would end the probe chain of every entry that had been
 * displaced past it, and those live tasks would then look up as having no
 * data.
 */
#define SDT_HASH_SLOTS 16384
#define SDT_HASH_MASK (SDT_HASH_SLOTS - 1)

struct sdt_entry {
	struct task_struct *key; /* NULL = empty slot */
	void *data;             /* arena-allocated per-task context */
	unsigned long home;     /* sdt_hash_pid() of key's PID at insert */
};

static struct sdt_entry sdt_table[SDT_HASH_SLOTS];
static u64 sdt_data_size;
static u64 sdt_align;
static int sdt_initialized;

/*
 * Upstream reports API misuse and failed lookups with scx_err_loc(): a line on
 * the program's BPF stderr stream, which scx userspace forwards to its own
 * stderr. It is a report, not an abort. The simulator writes the line to
 * stderr directly. Call only between sim_rbc_pause() and sim_rbc_resume().
 */
#define sdt_err(fmt, ...) fprintf(stderr, fmt "\n", ##__VA_ARGS__)

/*
 * Count of failed scx_task_data() lookups reported on stderr. Exposed
 * (non-static) so tests can tell the reporting lookup from the quiet one.
 */
unsigned long sim_sdt_missing_data_reports;

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

/* Home slot of a task: where its probe sequence starts. */
static unsigned long sdt_home(struct task_struct *p)
{
	return sdt_hash_pid(p ? sim_task_get_pid(p) : 0);
}

/*
 * Find a slot in the hash table for the given task.
 *
 * Uses PID-based hashing for deterministic probe sequences, but stores
 * and matches by task_struct pointer for correctness (PIDs are unique
 * per-task but the pointer is the actual key).
 */
static struct sdt_entry *sdt_find_slot(struct task_struct *p, unsigned long home)
{
	for (unsigned long i = 0; i < SDT_HASH_SLOTS; i++) {
		unsigned long slot = (home + i) & SDT_HASH_MASK;
		if (sdt_table[slot].key == p || sdt_table[slot].key == (void *)0)
			return &sdt_table[slot];
	}
	return (void *)0; /* table full — should never happen */
}

/*
 * Remove an entry without breaking any other entry's probe chain (Knuth's
 * Algorithm R). Walk the cluster after the hole; an entry whose home slot
 * does not lie cyclically in (hole, slot] probed past the hole to get where
 * it is, so move it back into the hole and continue from its old slot. The
 * stored home is used rather than re-hashing the key, because the key's
 * task_struct may already have been freed.
 */
static void sdt_remove(struct sdt_entry *entry)
{
	unsigned long hole = (unsigned long)(entry - sdt_table);
	unsigned long slot = hole;

	for (unsigned long i = 1; i < SDT_HASH_SLOTS; i++) {
		slot = (slot + 1) & SDT_HASH_MASK;
		if (sdt_table[slot].key == (void *)0)
			break;
		if (((slot - sdt_table[slot].home) & SDT_HASH_MASK) >=
		    ((slot - hole) & SDT_HASH_MASK)) {
			sdt_table[hole] = sdt_table[slot];
			hole = slot;
		}
	}
	sdt_table[hole] = (struct sdt_entry){ 0 };
}

/* The live entry for @p, or NULL if it has none. Call while RBC-paused. */
static struct sdt_entry *sdt_lookup(struct task_struct *p)
{
	struct sdt_entry *entry;

	if (!sdt_initialized || !p)
		return (void *)0;

	entry = sdt_find_slot(p, sdt_home(p));
	return entry && entry->key == p ? entry : (void *)0;
}

/*
 * Initialize the per-task allocator. Called once during scheduler init.
 *
 * In the real kernel, this sets up the radix-tree allocator with arena
 * pages; in the simulator, per-task data comes from the sim arena, so only
 * the size and alignment are recorded. The argument checks are upstream's
 * (scx_alloc_init): align 0 means the default of 8, and an alignment below
 * 8 or not a power of two is reported and rejected with -EINVAL, leaving
 * any earlier configuration in place.
 */
int scx_task_init(u64 data_size, u64 align)
{
	sim_rbc_pause();

	if (!align)
		align = 8;
	if (align < 8 || (align & (align - 1))) {
		sdt_err("scx_task_init: invalid alignment %llu", align);
		sim_rbc_resume();
		return -EINVAL;
	}

	sdt_data_size = data_size;
	sdt_align = align;
	__builtin_memset(sdt_table, 0, sizeof(sdt_table));
	sdt_initialized = 1;
	sim_rbc_resume();
	return 0;
}

/*
 * Allocate per-task scheduler context for a task.
 *
 * Returns a pointer to zero-initialized memory of sdt_data_size bytes,
 * aligned to sdt_align, or NULL on failure.
 */
void *scx_task_alloc(struct task_struct *p)
{
	struct sdt_entry *entry;
	unsigned long home;
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

	data = sim_arena_calloc_aligned(sdt_data_size, sdt_align);
	if (!data) {
		sim_rbc_resume();
		return (void *)0;
	}

	home = sdt_home(p);
	entry = sdt_find_slot(p, home);
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
	entry->home = home;
	sim_rbc_resume();
	return data;
}

/*
 * Look up existing per-task context for a task: __scx_task_data() returns
 * NULL quietly, for callers where absence is expected; scx_task_data() also
 * reports the miss, as upstream does through scx_err_loc().
 */
static void *sdt_task_data(struct task_struct *p, int report_missing)
{
	struct sdt_entry *entry;
	void *data;

	sim_rbc_pause();
	entry = sdt_lookup(p);
	data = entry ? entry->data : (void *)0;
	if (!data && report_missing) {
		sim_sdt_missing_data_reports++;
		sdt_err("scx_task_data: no task data (pid %d)",
			p ? sim_task_get_pid(p) : 0);
	}
	sim_rbc_resume();
	return data;
}

void *__scx_task_data(struct task_struct *p)
{
	return sdt_task_data(p, 0);
}

void *scx_task_data(struct task_struct *p)
{
	return sdt_task_data(p, 1);
}

/*
 * Drop a task's context. Repeated frees and frees of a task that never had
 * context are no-ops, as upstream, where the first caller claims the data.
 */
static void sdt_task_free(struct task_struct *p)
{
	struct sdt_entry *entry;

	sim_rbc_pause();
	entry = sdt_lookup(p);
	if (entry) {
		sim_arena_free(entry->data);
		sdt_remove(entry);
	}
	sim_rbc_resume();
}

void scx_task_free(struct task_struct *p)
{
	sdt_task_free(p);
}

/*
 * The deferred free: upstream unlinks the data at once and returns it to the
 * allocator only after an RCU grace period, so a pointer borrowed inside a
 * read-side critical section stays valid. sim_arena_free() never reclaims
 * within a run, which keeps every borrowed pointer valid at least that long,
 * so the immediate path is already the deferred one.
 */
void scx_task_free_rcu(struct task_struct *p)
{
	sdt_task_free(p);
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
 * scx_task_init() call with the same data_size and align.
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
	/* Note: sdt_initialized, sdt_data_size and sdt_align are NOT reset here.
	 * They are set during scheduler load (scx_task_init) and must
	 * persist across simulation runs with the same scheduler. */
}
