// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

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
