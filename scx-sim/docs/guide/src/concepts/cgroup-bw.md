# Cgroup Bandwidth

> **Status — stub.** This page will cover how scxsim models cgroup-v2
> `cpu.max` (quota/period), throttling, the accounting timer, and
> replenishment.

Planned content:

- The cgroup hierarchy: per-cgroup quota, period, runtime budget.
- How the accounting timer is armed (initial period vs MIN-bound
  re-arm); how it can fire as fast as 1ms under continuous
  throttling.
- Throttle and replenish transitions; `TraceKind::CgroupBwReplenish`
  events.
- The `enable_cpu_bw` LAVD-side switch and where it's surfaced (TOML
  config sidecar; see [Scheduler Config](../running-simulations/scheduler-config.md)).
- The Bug-1 canonical reproducer
  (`crates/scx_simulator/tests/fixtures/h6/bug1_canonical.{json,toml}`)
  as the worked example; this will be linked from
  [Recipes → Reproducing a Stall Bug](../recipes/repro-stall.md).
