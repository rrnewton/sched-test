---
title: 'REGRESSION: cargo fmt failure in safe/mod.rs - import ordering'
status: closed
priority: 1
issue_type: bug
created_at: 2026-03-14T11:14:54.043380126+00:00
updated_at: 2026-03-14T11:17:07.686747116+00:00
closed_at: 2026-03-14T11:17:07.686747026+00:00
---

# Description

cargo fmt --check fails on crates/scx_simulator/src/safe/mod.rs due to incorrect import ordering: 'pub mod engine' appears before 'pub mod det_hashmap' but should appear after it alphabetically. This is a regression from the safety refactor module restructuring. The baseline (sched-test3) has clean fmt.
