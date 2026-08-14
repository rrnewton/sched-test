---
title: 'Tests report PASS while asserting nothing: ~10 capability-gated silent skips violate No Silent Failures'
status: open
priority: 1
issue_type: task
created_at: 2026-08-12T14:12:51.613213627+00:00
updated_at: 2026-08-12T14:12:51.613213627+00:00
---

# Description

Investigation `investigate-14-skipped-tests` found a population of tests that are NOT counted in nextest's 'skipped' number because they report PASS while executing no assertions. They sit inside the '989 passed'. This is the same failure mode as the silently-dying ASLR gate.

Confirmed by running with --no-capture:

compare.rs (empirically silent-skipping on this box RIGHT NOW):
- test_load_real_bpf_trace   -> prints '=== Skipping real BPF trace test ===', returns, PASS
- test_full_real_vs_sim_comparison -> prints '=== Skipping full comparison test ===', returns, PASS
Both gate on `bpf_trace.log` existing in CWD, which is essentially never true. These two are dead in every run anyone has ever done.

scx_perf/src/lib.rs (7 of 10 tests; machine-dependent):
test_rdpmc_basic, test_rdpmc_matches_read, test_rbc_counter_lifecycle, test_rbc_timer_set_period, test_rbc_timer_signal_delivery, test_hw_breakpoint_basic, test_hw_breakpoint_signal_delivery.
Pattern: `match make_counter_or_skip() { Some(p)=>p, None=>return }` plus 'counter reads 0 (likely VM/container)' early-returns. On THIS bare-metal box they genuinely run (verified: no skip message emitted). In CI/VM they all silently pass asserting nothing.

perfetto_pb.rs:
- test_perfetto_pb_ingestible_by_trace_processor gates on trace_processor_shell. Present at ~/bin here so it really runs; absent in CI -> silent pass.

Extra defect found in the same code: test_rdpmc_basic has
    if count == 0 { eprintln!(...); return; }
    assert!(count > 0, ...);
The guard makes the assertion vacuous — it can never fail. Same shape appears in the sibling rdpmc/RBC tests.

Why this is a rule violation, not a style nit: scx-sim/CLAUDE.md 'No Silent Failures' says a missing HW breakpoint in replay must PANIC, not degrade. And 'NEVER skip a validation step because a tool is missing... A skipped check is a lie.' These gates are exactly the prohibited silent degrade.

Proposed fix: replace `return` gates with an explicit capability decision:
1. Probe capability once, centrally.
2. If capable -> run and assert (loud failure on regression).
3. If not capable -> mark the test genuinely skipped so it shows in the skipped COUNT (e.g. #[ignore] with a capability-detecting harness, or nextest filter by a 'requires-pmu' marker), never a green PASS.
4. Add a CI assertion that the expected number of capability-skips matches an allowlist, so a newly-silent test cannot hide.
Also delete or repoint the two compare.rs bpf_trace.log tests — they have never run.
