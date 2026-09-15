/*
 * sim_arena.c - Storage for the deterministic bump allocator.
 *
 * See sim_arena.h for the allocator interface and rationale.
 */

#include "sim_arena.h"

/* Page-aligned arena buffer. Lives in BSS (zero-initialized). */
char sim_arena_buf[SIM_ARENA_SIZE]
	__attribute__((aligned(4096)));

/* Current allocation offset into sim_arena_buf. */
unsigned long sim_arena_offset;

/*
 * Floor below which sim_arena_reset() will not reclaim; see sim_arena.h.
 * Zero until the engine calls sim_arena_mark_persistent(), so a build that
 * never marks behaves exactly as before.
 */
unsigned long sim_arena_floor;

void sim_arena_mark_persistent(void)
{
	sim_arena_floor = sim_arena_offset;
}
