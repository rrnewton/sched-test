---
title: stress.py generates invalid workload x CPU-count combos (affinity exceeds -c N), inflating 'other' findings
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T13:44:19.080659120+00:00
updated_at: 2026-08-12T13:44:19.080659120+00:00
---

# Description

validate.sh's stress.py smoke run reports ~22% (10/46) 'other' findings that are NOT simulator bugs: stress.py pairs fixed workloads (simple_wake, two_runners, dsq_contention, lavd_dsq_stress) with a randomized CPU count, and when the workload pins a task to a CPU index >= that count scxsim correctly rejects it at parse time:

  error: failed to parse workload: invalid value: task "producer" has CPU affinity for CPU 1, but only 1 CPUs are configured (valid range: 0..0)

Observed classes (all 10 findings from one run, master seed 3002975999):
  5x producer -> CPU 1 with -c 1
  2x worker0  -> CPU 2 with -c 2
  1x worker0  -> CPU 1 with -c 1
  1x cpu0     -> CPU 2 with -c 2
  1x cpu0     -> CPU 1 with -c 1

Impact: these are harness misconfigurations counted as bugs. They pollute the finding stream, waste ~20% of stress runs, and make the 'N bugs found' number meaningless as a regression signal.

Fix: stress.py should compute each workload's minimum required CPU count (max affinity index + 1) and only sample CPU counts >= that minimum -- or skip the combo -- rather than launching a run that is guaranteed to fail at parse.

Found during: tg catchup-validate-baseline (green-baseline validation before upstream scx sync), repo SHA 4a149b6.
