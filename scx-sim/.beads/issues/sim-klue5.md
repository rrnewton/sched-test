---
title: 'scx_layered upstream: SMT shrink dampening + core rounding creates an unreachable-target fixed point'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T21:10:36.026876893+00:00
updated_at: 2026-08-12T22:08:02.929922044+00:00
---

# Description

Found while writing scxsim Tier-3 SMT tests (tg layered-tier3-drive). This is
an UPSTREAM behaviour, faithfully reproduced by scxsim — not a scxsim bug.

With alloc_unit == 2 (SMT), a layer sitting at 4 CPUs / 2 cores whose computed
target is 2 CPUs can never shrink:

  main.rs::refresh_cpumasks() shrink dampening (CPU space):
      dampened = cur - (cur - target).div_ceil(2)
               = 4   - ceil(2/2)  = 3 CPUs
  main.rs::calc_raw_demands() conversion to alloc units (rounds UP):
      target_units = dampened.div_ceil(au) = ceil(3/2) = 2 cores
  unified_alloc grants 2 cores = 4 CPUs -> cur unchanged -> fixed point.

Each pass recomputes the identical values, so it is a true fixed point rather
than slow convergence. Verified by instrumenting the scxsim control loop: au,
node_caps, demands and targets are byte-identical on every cycle.

Consequence: with SMT enabled, a layer at 2 cores cannot release a core to a
layer that wants it, even when the configured cpus_range asks for exactly
that. The requested split is simply unreachable from an even start. Layers can
still shrink when the gap is large enough that the halved distance crosses a
core boundary (e.g. 6 -> 4 CPUs works, 4 -> 2 does not).

The interaction is between dampening in CPU space and rounding UP to alloc
units. Dampening in alloc units, or rounding the dampened target DOWN when it
is already below the current unit count, would both remove it — but either is
an upstream policy change, not something scxsim should paper over locally.

scxsim reproduces this faithfully and the test
smt_allocation_keeps_whole_cores_and_hits_the_shrink_fixed_point pins it down
explicitly so a future "fix" to the simulator that makes 6/2 reachable will
fail loudly as a divergence from upstream.

Worth reporting upstream. Not blocking Tier 3.

# Notes

Test renamed to smt_allocation_hits_the_shrink_fixed_point (the whole-core half moved to smt_core_transfer_moves_whole_cores_only, which forces a real transfer first).
