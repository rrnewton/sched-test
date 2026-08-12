---
title: No guard against a NEW capability-skip appearing unnoticed (sim-hdsgn proposal 4)
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T14:54:05.815800628+00:00
updated_at: 2026-08-12T14:54:05.815800628+00:00
---

# Description

sim-hdsgn proposed four fixes for tests that report PASS while asserting nothing. Proposals 1-3 are implemented (scx_perf::capability + validate.sh probe). Proposal 4 is NOT:

  'Add a CI assertion that the expected number of capability-skips matches an allowlist, so a newly-silent test cannot hide.'

Current state after the fix: a capability skip is loud (SCXSIM-CAPABILITY-SKIP line, plus a row in validate.sh's end-of-run SKIPPED summary), and it cannot happen at all unless SCXSIM_ALLOW_MISSING_CAPS declares the capability. That is a large improvement over the invisible green passes, but it is still monitoring rather than enforcement:

  - Nothing pins WHICH tests are allowed to skip for a given declared capability. If someone adds a tenth test that routes through capability::absent(PMU, ..), it silently joins the existing PMU skip set on any machine that declares pmu missing.
  - Nothing pins the COUNT. On a capability-less CI runner the whole PMU test group can go quiet and the run stays green, with only a summary line to notice.

Suggested implementation: commit an allowlist (capability -> expected set of test names). Have validate.sh collect the SCXSIM-CAPABILITY-SKIP lines from the nextest output and diff them against the allowlist for the declared capabilities, failing on any test that skipped without being listed. The marker string is already stable and greppable (scx_perf::capability::SKIP_MARKER) specifically so this is cheap to add.

Found during task fix-tests-asserting-nothing.
