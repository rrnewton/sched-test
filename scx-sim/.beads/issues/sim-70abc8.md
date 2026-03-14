---
title: Determinism failures persist across all schedulers and modes
status: open
priority: 1
issue_type: bug
depends_on:
  sim-e0791: related
created_at: 2026-03-14T17:15:02.657643074+00:00
updated_at: 2026-03-14T17:15:02.657643074+00:00
---

# Description

30-minute stress test found 2162 determinism failures across all 5 schedulers in both cooperative and off interleaving modes. sim-e0791 was previously closed but the issue remains. Divergences are primarily RBC count mismatches, indicating nondeterminism in branch counting. All workloads affected. Example: scxsim run workloads/two_runners.json -s simple -c 8 --seed 2530796772 --watchdog-timeout 2s --end-time 4s --determinism-check. Shows RBC: 29 vs 30 MISMATCH at checkpoint 506.
