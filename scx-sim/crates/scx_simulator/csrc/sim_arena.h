/*
 * sim_arena.h - Deterministic bump allocator for PMU RBC counting.
 *
 * Replaces glibc calloc/free in simulator stubs (sim_bpf_stubs.c,
 * sim_sdt_stubs.c) with a branchless bump allocator. glibc's malloc
 * has nondeterministic conditional branches for arena selection and
 * bin management that vary between process invocations, polluting the
 * PMU retired-branch-conditional counter.
 *
 * The arena is a statically-allocated buffer with a bump pointer.
 * Allocation is a single pointer increment with no conditional branches
 * in the fast path. Free is a no-op — the entire arena is reset between
 * simulation runs via sim_arena_reset().
 *
 * This ensures perfectly deterministic RBC counts for all scheduler
 * callbacks that allocate memory (init, init_task, etc.).
 */

#ifndef SIM_ARENA_H
#define SIM_ARENA_H

/*
 * Arena capacity: 32 MiB, bumped from the original 4 MiB as part of the
 * Phase 1 BPF infra scale-up (tg `scxsim-bpf-infra-scale-up-phase1`,
 * design doc §Phase 1 item 4). Sized to comfortably host:
 *   - Up to 8 192 per-task contexts (~16 MiB at ~2 KiB each, matching
 *     the 16 384-slot SDT hash table in `sim_sdt_stubs.c` at ~50% load)
 *   - Up to 2 048 per-cgroup contexts allocated by Phase 2's compiled-in
 *     `scx_cgroup_bw_init` (each `scx_cgroup_ctx_t` ~512 B + per-LLC
 *     children ~256 B, ~1.5 MiB total)
 *   - Up to 2 048 atq instances per Phase 1 item 7 (`scx_atq_create`
 *     allocates an atq head ~256 B, plus per-task `scx_atq_node`
 *     entries ~64 B; with the cpu-bw-stall-bug stress matrix's worst
 *     case of ~10 000 backlogged tasks, ~1 MiB total)
 *   - 128 cpumasks (~128 KiB)
 *   - Headroom for future BPF-library compile-ins (Phase 2/3)
 *
 * All allocations are 16-byte aligned for SIMD compatibility.
 *
 * Memory cost: 32 MiB BSS. Comfortable on the dev host (192 GiB+) and on
 * VM/baremetal hosts the matrix targets. The previous 4 MiB ceiling
 * silently capped scxsim at ~2 000 live tasks; Phase 1's whole point is
 * removing that ceiling so the cpu-bw-stall-bug high-cgroup-count
 * stress regime (BPF-map-pressure §1) becomes testable in scxsim.
 */
#define SIM_ARENA_SIZE (32UL * 1024 * 1024)
#define SIM_ARENA_ALIGN 16

/* Defined in sim_arena.c */
extern char sim_arena_buf[SIM_ARENA_SIZE];
extern unsigned long sim_arena_offset;

/*
 * Allocate `size` bytes of zero-initialized memory from the arena.
 *
 * Branchless fast path: unconditionally bumps the pointer and returns.
 * The only conditional is the overflow check, which should never fire
 * in normal operation.
 *
 * Returns NULL only if the arena is exhausted (a bug — should never
 * happen with properly-sized arena).
 */
static inline void *sim_arena_calloc(unsigned long size)
{
	unsigned long aligned_size = (size + SIM_ARENA_ALIGN - 1) &
				     ~(unsigned long)(SIM_ARENA_ALIGN - 1);
	unsigned long offset = sim_arena_offset;
	unsigned long new_offset = offset + aligned_size;

	if (__builtin_expect(new_offset > SIM_ARENA_SIZE, 0))
		return (void *)0; /* arena exhausted — should not happen */

	sim_arena_offset = new_offset;

	/* Zero the memory to match calloc semantics. Use __builtin_memset
	 * to avoid calling glibc's memset (which has nondeterministic
	 * alignment branches). The compiler emits inline rep stosb. */
	__builtin_memset(sim_arena_buf + offset, 0, aligned_size);

	return sim_arena_buf + offset;
}

/*
 * Free is a no-op — the arena is bulk-reset between simulation runs.
 */
static inline void sim_arena_free(void *ptr)
{
	(void)ptr;
}

/*
 * Floor below which sim_arena_reset() will not reclaim.
 *
 * A scheduler's `<name>_setup()` runs ONCE, when the .so is dlopen'd, and
 * may allocate objects the scheduler holds for its whole lifetime --
 * tickless and cosmos both create their primary-CPU `bpf_cpumask` there via
 * enable_primary_cpu(). Those allocations come out of this arena.
 *
 * Resetting the bump pointer all the way to 0 between runs therefore did two
 * wrong things at once: it zeroed a live object the scheduler still pointed
 * at, and it handed the same bytes out again to the next run's allocations.
 * The visible symptom was that is_primary_cpu() returned false forever, so
 * tickless never reached init_timer() and its entire timer path went
 * unexecuted -- see mb sim-hfvmf.
 *
 * Recording a floor after setup keeps the determinism guarantee intact (the
 * floor is fixed once the .so is loaded, so every run still starts its
 * allocations at the same address) while leaving setup-time objects alone.
 */
extern unsigned long sim_arena_floor;

/*
 * Freeze everything allocated so far as scheduler-lifetime state.
 *
 * Called by the engine immediately after `<name>_setup()`. Idempotent.
 */
void sim_arena_mark_persistent(void);

/*
 * Reset the arena for a new simulation run.
 *
 * Zeros the per-run portion and rewinds the bump pointer to the persistent
 * floor, so the next run's allocations return the same addresses as the
 * first run's while setup-time allocations survive.
 */
static inline void sim_arena_reset(void)
{
	if (sim_arena_offset > sim_arena_floor)
		__builtin_memset(sim_arena_buf + sim_arena_floor, 0,
				 sim_arena_offset - sim_arena_floor);
	sim_arena_offset = sim_arena_floor;
}

#endif /* SIM_ARENA_H */
