---
title: scxsim ignores cgroup cpuset when placing tasks — it is advisory metadata, not a kernel constraint
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T23:05:24.996967277+00:00
updated_at: 2026-08-12T23:05:24.996967277+00:00
---

# Description

A cgroup's `cpuset` reaches the simulator intact and is then never enforced. Tasks in a cgroup confined to CPUs {2,3} run on CPU 0.

## Evidence

ktstr's `sched_cpuset_split` — two cgroups on disjoint halves of a 4-CPU machine — replayed through `ktstr-scenario-replay`. Actual placement measured from `TaskScheduled` trace events:

    nr_cpus=4  placement={"cg_0": {1}, "cg_1": {0}}
       declared cpuset "cg_0" -> Some([CpuId(0), CpuId(1)])
       declared cpuset "cg_1" -> Some([CpuId(2), CpuId(3)])

cg_1 is declared confined to {2,3} and ran on CPU 0. CPUs 2 and 3 saw no activity at all. The two cgroups the scenario requires to be disjoint both ran inside the same half.

## Root cause

- `engine.rs:1499` — `ffi::task_setup_cpumask(task.raw(), def.allowed_cpus.as_deref())` is the only place a task's cpumask is narrowed, and it reads `TaskDef::allowed_cpus`.
- `engine.rs:1696` — a cgroup's cpuset goes into `cgroup_registry.create(...)` and is advertised to the BPF scheduler via `cgroup_init`. It never constrains placement.
- Nothing intersects a task's allowed CPUs with its containing cgroup's cpuset, so `allowed_cpus` stays `None` for cpuset-confined tasks.

## Why this is a Principle 1 violation, not a missing feature

In the kernel, `cpuset.cpus` narrows every member task's effective cpumask (`cpuset_cpus_allowed`); the scheduler *cannot* place the task outside it. Enforcement is the kernel's job, so it is scxsim's job — see 'Model the Kernel, Not the Scheduler'. Today scxsim models it as something the scheduler may consult and obey, which is what the kernel specifically does not do.

## Why it is worse than an honest approximation

The workload IR's `FidelityReport` says **exact** for this scenario, correctly — the lowering does carry the cpuset faithfully into `Scenario.cgroups[].cpuset`. The loss is downstream of anything the report can see. A reader gets `fidelity: exact` plus a plausible per-cgroup CPU-time spread of 0.0003% and concludes the simulator reproduced a cpuset-confinement test. It ran two unconfined spinners. The per-cgroup CPU-time oracle cannot catch it either: CPU time is ~equal whether or not confinement holds, so the oracle agrees with the VM while the property under test is absent.

## Fix directions (not chosen here)

1. Intersect each task's allowed CPUs with its cgroup's cpuset at scenario setup, so `task_setup_cpumask` receives it. Faithful for static cpusets; does not cover `cgroup_cpuset_change_events`.
2. Enforce in the engine at placement time against the registry's current cpuset, which also covers runtime changes and matches where the kernel does it.

Whichever is chosen, `handle_cgroup_cpuset_change` should migrate affected running tasks the way the kernel does.

## Guard

`ktstr-scenario-replay` has an `#[ignore]`d correct-behaviour guard asserting observed placement is a subset of the declared cpuset. It fails today by design; un-ignore it when this is fixed.
