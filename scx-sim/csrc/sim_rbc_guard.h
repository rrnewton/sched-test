// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

/*
 * sim_rbc_guard.h - PMU RBC counter pause/resume for C kfunc stubs.
 *
 * Every BPF kfunc implemented as a C stub must pause the PMU RBC counter
 * on entry and resume it on exit. This ensures kfunc branches (kernel code)
 * are not counted as scheduler overhead, matching real kernel semantics.
 *
 * Usage:
 *
 *   ReturnType my_kfunc(args...) {
 *       RBC_GUARD_START;
 *       ... body ...
 *       RBC_GUARD_RETURN(value);
 *   }
 *
 *   void my_void_kfunc(args...) {
 *       RBC_GUARD_START;
 *       ... body ...
 *       RBC_GUARD_RETURN_VOID;
 *   }
 *
 * For early returns, use RBC_GUARD_RETURN / RBC_GUARD_RETURN_VOID at
 * every return point:
 *
 *   if (error) RBC_GUARD_RETURN(NULL);
 *
 * Trivial kfuncs with no conditional branches (e.g., just returning a
 * pointer) don't need the guard — there's nothing nondeterministic to
 * exclude.
 *
 * The pause/resume is re-entrant: nested calls from kfuncs that call
 * other kfuncs (or Rust kfuncs via with_sim) are handled correctly
 * via a depth counter in sim_rbc_pause/sim_rbc_resume.
 */

#ifndef SIM_RBC_GUARD_H
#define SIM_RBC_GUARD_H

/* Defined in Rust (kfuncs.rs), resolved from the main binary via -rdynamic. */
extern void sim_rbc_pause(void);
extern void sim_rbc_resume(void);

#define RBC_GUARD_START  sim_rbc_pause()

#define RBC_GUARD_RETURN(val) \
	do { sim_rbc_resume(); return (val); } while (0)

#define RBC_GUARD_RETURN_VOID \
	do { sim_rbc_resume(); return; } while (0)

#endif /* SIM_RBC_GUARD_H */
