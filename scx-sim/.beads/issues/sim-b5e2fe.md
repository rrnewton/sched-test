---
title: 'REGRESSION: test_batch_concurrent_preemptive_smoke SIGABRT - kfunc called outside simulator context'
status: closed
priority: 0
issue_type: bug
created_at: 2026-03-14T11:14:40.437477446+00:00
updated_at: 2026-03-14T11:16:38.396129012+00:00
closed_at: 2026-03-14T11:16:38.396128882+00:00
---

# Description

The test test_batch_concurrent_preemptive_smoke crashes with SIGABRT in preemptive interleave mode. The panic message is 'kfunc called outside of simulator context (SIM_ARC not installed)' from kfuncs.rs:1163. This is a REGRESSION from the safety refactor: the test passes on the sched-test3 baseline. The sim_callback\!/with_sim pattern fails to install SIM_ARC on the thread-local before the scheduler C code calls kfuncs. This also manifests in stress.py with the repro: scxsim run lavd_dsq_stress.json -s simple -c 8 --seed 905015833 --preemptive.
