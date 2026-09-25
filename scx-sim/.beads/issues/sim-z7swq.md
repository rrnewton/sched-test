---
title: 'scxsim: bpf_get_prandom_u32() is the constant 0 — lavd never skips cross-domain stealing and layered always uses proximity map 0'
status: open
priority: 1
issue_type: bug
labels:
- kernel-fidelity
- lavd
- layered
depends_on:
  sim-2ae49c: related
created_at: 2026-09-25T05:18:55.266172544+00:00
updated_at: 2026-09-25T05:21:26.484322684+00:00
---

# Description

DEFECT

`crates/scx_simulator/scxtest/overrides.h` defines `bpf_get_prandom_u32()` as `0`, with the comment "This is a static helper for some reason, so we have to define it here". Every scheduler the simulator compiles gets that override.

The simulator does have a seeded PRNG for this helper: `sim_bpf_get_prandom_u32` in `crates/scx_simulator/src/unsafe_impl/kfuncs.rs`. But nothing references it except its own unit test.

A redirect to it was written for sim-2ae49c: commit 6b06a99a, "Fix bpf_get_prandom_u32 routing to deterministic PRNG". It never reached integration. Commit 9036784d, on an unmerged PR branch, drops it again. Yet sim-2ae49c is closed as if the fix had landed: its checklist says "FIXED: was hardcoded to 0 by overrides.h macro; now routed to deterministic Rust PRNG via sim_wrapper.h redirect (6b06a99)". `ai_docs/prng_replay_divergence_analysis.md` likewise lists `sim_bpf_get_prandom_u32()` as "called by scheduler C code during callbacks".

PRODUCTION

`bpf_get_prandom_u32()` returns a pseudo-random u32 on every call.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

It returns 0 on every call.

CONSEQUENCE

Randomised decisions become fixed ones (DIVERGES):
- lavd `prob_x_out_of_y(x, y)` is always true for x > 0, so lavd never skips cross-domain stealing.
- lavd `do_core_compaction` and cgroup_bw's `scx_cgroup_bw_reenqueue` draw a constant.
- layered `try_consume_layer` always uses proximity map 0.

Each of these is a scheduling policy that production randomises precisely to avoid the bias that a constant produces.

FIX DIRECTION

- Route `bpf_get_prandom_u32()` to `sim_bpf_get_prandom_u32`, seeded per simulation so that runs stay reproducible.
- Remove the constant from overrides.h, or override it after overrides.h, for the simulator build.
- Check first whether 6b06a99a can be reused.
- Draw from a stream separate from `SimulatorState::rng`, which also feeds tick jitter, context-switch noise and the per-round interleave seed. `ai_docs/prng_replay_divergence_analysis.md` (Option B) gives the reason: then the engine's stream, and the interleave seed derived from it, do not depend on how often the scheduler draws. That doc's replay-retry findings apply as soon as scheduler code draws at all.
- Correct sim-2ae49c's checklist and the analysis doc once the redirect lands.

ACCEPTANCE

- With a fixed seed, lavd's `prob_x_out_of_y` returns both true and false within one run, and the same seed reproduces the same trace.
- A layered test sees `try_consume_layer` visit more than one proximity map.
- The coverage re-measure shows the previously dead branches behind these draws as COVERED.
- Until this is fixed, the override carries a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row K01 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
