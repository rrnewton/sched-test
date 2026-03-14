---
title: Preemptive mode wall-clock timeouts across all schedulers
status: open
priority: 3
issue_type: bug
created_at: 2026-03-14T17:14:54.864233703+00:00
updated_at: 2026-03-14T17:14:54.864233703+00:00
---

# Description

30-minute stress test found 873 wall-clock timeouts (120s process timeout exceeded) with all 5 schedulers under preemptive interleaving mode. Particularly concentrated on lavd (123 with lavd_dsq_stress, 105 with dsq_contention), simple (84 dsq_contention, 67 lavd_dsq_stress), cosmos (66 lavd_dsq_stress, 64 dsq_contention), mitosis (73 lavd_dsq_stress, 71 dsq_contention), tickless (64+ dsq_contention). Preemptive mode with PMU signals appears to cause extreme slowdown, likely from high preemption overhead making simulations take >120s wall-clock for a 4s simulation.
