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
 * Arena capacity: 4 MiB, sufficient for:
 *   - 1024 per-task contexts (up to ~2 KiB each = ~2 MiB)
 *   - 128 cpumasks (1024 bytes each = ~128 KiB)
 *   - Headroom for future allocations
 *
 * All allocations are 16-byte aligned for SIMD compatibility.
 */
#define SIM_ARENA_SIZE (4UL * 1024 * 1024)
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
 * Reset the arena for a new simulation run.
 *
 * Zeros the used portion and resets the bump pointer. This ensures
 * the next run's allocations return the same addresses as the first
 * run (since the bump pointer starts from the same position).
 */
static inline void sim_arena_reset(void)
{
	if (sim_arena_offset > 0)
		__builtin_memset(sim_arena_buf, 0, sim_arena_offset);
	sim_arena_offset = 0;
}

#endif /* SIM_ARENA_H */
