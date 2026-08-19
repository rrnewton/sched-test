---
title: mitosis tests model cells as cgroup cpusets, which is not what a mitosis cell is
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T23:32:41.570729474+00:00
updated_at: 2026-08-12T23:32:41.570729474+00:00
---

# Description

Surfaced by sim-4qlh5. Now that the engine enforces cgroup cpusets, three mitosis tests fail — and the reason is a modelling equivalence that was never true, only invisible.

## What the tests do

`test_mitosis_cpu_borrowing_select_cpu`, `test_mitosis_cpu_borrowing_enqueue` and `test_mitosis_demand_rebalancing` build cells with the cgroup builder:

    .cgroup("busy_cell", &[CpuId(0), CpuId(1)])
    .cgroup("idle_cell", &[CpuId(2), CpuId(3)])

and put every task in `busy_cell`. They then assert the tasks DO run on the idle cell's CPUs — cross-cell borrowing.

## Why that cannot hold once cpusets are enforced

A cgroup cpuset is a hard kernel constraint: a task in a cgroup confined to {0,1} cannot run on {2,3}, ever. So "borrow an idle CPU from another cell" and "confine this cgroup by cpuset" are contradictory requests. The tests were only passing because the simulator ignored the cpuset.

A mitosis **cell** is not a cgroup cpuset. It is a cpumask the scheduler assigns and manages itself, and the scheduler is free to widen it — that is what borrowing IS. Expressing a cell as a cgroup cpuset takes a scheduler-owned, mutable decision and encodes it as a kernel-owned, immutable constraint. Those differ precisely on the behaviour these tests exist to check.

## What was done for now

- `cpu_borrowing_select_cpu` / `cpu_borrowing_enqueue` carried a TODO(sim-b7d70) instructing exactly this flip once isolation took effect, so they now assert `cross_cell_events.is_empty()`. **Note the attribution changed:** isolation now holds because the cgroup cpuset is ENFORCED (sim-4qlh5), not because CSS iterators are populated for timer callbacks. sim-b7d70 is UNCHANGED, and these tests no longer exercise mitosis's own cell-assignment path at all.
- `demand_rebalancing` asserts each of 8 tasks is scheduled >= 100 times. Calibrated for 8 tasks on 8 CPUs; the tasks are now correctly confined to the 4 CPUs their cgroup declares, and hog 1 measures 74. Lowering the threshold to fit would be fitting a number to a scenario whose intent changed. `#[ignore]`d with a reason pointing here.

## What is actually needed

Decide how a mitosis cell should be expressed in a scenario. If cells are scheduler-assigned cpumasks, these scenarios should NOT use cgroup cpusets to denote them — they should give the tasks an unconfined cgroup and let mitosis assign cells, which is also the only way the borrowing behaviour can be exercised at all. That is a mitosis-owner call, not a cpuset-enforcement one.

## Two more tests, same class, arriving with PR #60 (2026-08-19)

`tests/mitosis_cell_migration.rs` was written 2026-07-22, before cpuset
enforcement landed, and merged onto integration during the PR-queue drain. Two
of its four tests are the same modelling error and are `#[ignore]`d with the
same reasoning rather than having their bounds lowered:

- `test_multi_cell_topology_all_tasks_run` — two 4-CPU cells as cgroup cpusets,
  asserts the workload spreads over both. Measured after enforcement: 2 CPUs.
- `test_uncontended_cross_cell_migration_forward_progress` — additionally
  depends on a runtime `cgroup_migrate` re-narrowing the cpumask, which is
  sim-r7eou and still open. Measured: migrant ran 196447325ns of a ~400ms sim.

Bisected to 9501a60 (PR #79) and confirmed by running all four green at PR #60's
own tip 870eb46. The other two tests in that file pass and are not affected.

Whatever decision this issue reaches about how a mitosis cell should be
expressed applies to these two as well — there are now five tests waiting on it,
not three.
