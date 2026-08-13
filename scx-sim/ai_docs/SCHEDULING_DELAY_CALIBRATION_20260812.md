# Scheduling delay, wired on both sides — and why the first result barely tests anything

Follow-up to `CALIBRATION_FIRST_RUN_20260812.md` and to the off-CPU
reclassification. Closes `mb sim-a4y1j`.

`Metric::SchedulingDelay` had a tolerance and no measurement: the bound was
registered first, deliberately, before either side could compute the quantity.
This wires both sides and reports what came out.

Reproduce everything below with:

```bash
cd scx-sim
cargo test -p scxsim-calibration --features sim \
  --test calibrate_sched_basic_proportional the_findings_as_first_measured \
  -- --nocapture
```

## Result

```text
  scheduling_delay  [cg_0]   sim=6.007ms   vm=3.694ms   gap +62.6%   +/-20.0% or +/-4.000ms   agree
  scheduling_delay  [cg_1]   sim=6.009ms   vm=8.683ms   gap +30.8%   +/-20.0% or +/-4.000ms   agree
```

Both agree. **Both are also outside the relative arm.** What admits them is the
4 ms absolute floor, and that is the whole story of this run.

## The two definitions, checked before the numbers were compared

This is the step the off-CPU metric skipped, and skipping it is what produced a
"7x infidelity" that turned out to be mostly virtualization overhead.

**Live.** `mean_run_delay_us` is the mean over the cgroup's WORKERS of each
worker's whole-run delta in `task->sched_info.run_delay`, read per worker from
`/proc/self/task/<tid>/schedstat` field 2. Source, not inference — ktstr
`src/workload/worker/sched.rs` for the read, `src/assert/reductions.rs:191-200`
for the reduction, and `src/assert/stats_types.rs:425-436` states it outright:
"the mean is the average per-worker total queued-to-run delay, and
`worst_run_delay_us` selects the single worker with the largest total
queued-to-run delay (NOT the worst single dispatch)". The kernel accumulates
that field in `sched_info_arrive()` at each dispatch as `now - last_queued`,
with `last_queued` stamped by `sched_info_enqueue()` on every enqueue including
the re-enqueue of a preempted task.

**Simulated.** Sum over episodes of `TaskScheduled - EnqueueTask`, per task,
averaged over the cgroup's tasks. Same shape: a per-task total, then a mean
across tasks. It is the rule `TraceStats::sched_latencies` uses and
`crates/scx_simulator/tests/rundelay_tracking.rs` validates — including that it
accumulates across preempt/re-enqueue cycles, which is what makes it a total
rather than a first-wakeup latency.
`the_calibrations_run_delay_is_the_simulators_own_rule` pins the two
implementations together so they cannot drift.

**Verdict on the definitions: they are the same physical quantity.** Two
residual differences, both pushing the simulator DOWN:

| difference | measured size |
|---|---|
| direct dispatch charges no wakeup cost in the simulator | **1 of 24029 dispatches** — cannot explain anything |
| no IRQs, timer ticks, kernel threads or host in the simulator | not isolable on this fixture; makes the simulated figure a FLOOR |

The populations match, unlike context switches: both sides count the workload's
workers and nothing else.

## The finding: this fixture cannot discriminate

Three facts, each asserted by a test so it fails when it stops being true.

**1. The agreement rests entirely on the absolute arm.** Gaps are +62.6% and
+30.8% against a 20% relative arm. The 4 ms floor is what carries them — and
4 ms is *larger than cg_0's entire live value* of 3.694 ms, so the check would
accept any simulated figure from 0 to 7.694 ms for that cgroup.
(`scheduling_delay_agrees_only_on_the_absolute_arm`)

**2. The live side cannot reproduce itself to better than 2.35x.** cg_0 and
cg_1 run the identical workload — one spinner each, two cgroups, two CPUs — and
the guest measured 3.694 ms against 8.683 ms. That between-worker spread is
larger than either cgroup's gap to the simulator (1.63x and 1.44x). No
comparison against a single run of this scenario can resolve a difference
smaller than the reference's own spread, whatever the tolerance says.
(`the_live_sides_own_spread_exceeds_the_gap_being_measured`)

**3. The simulator lands between the two live values.** 6.007 / 6.009 ms
against 3.694 and 8.683. The two simulated figures are nearly identical, as they
should be for a symmetric workload; the live pair is not.

## The bound was not adjusted, and should not be

The tolerance is the one committed in #95, derived from what a policy comparison
needs: f = 0.20 resolves a 1.5x policy difference, and the 4 ms arm is one
scheduler tick (`TICK_INTERVAL_NS`, `CONFIG_HZ` 250) below which no policy
conclusion can rest on the difference. Both arguments still hold. Moving either
number now, having seen the result, would destroy the only property that made
pre-registering it worthwhile.

What the finding argues for is a **different scenario**, not a different bound:
one with more workers than CPUs, whose runqueue waits are tens to hundreds of
milliseconds, where the relative arm is what binds and the tick floor is
irrelevant. That is already item 3 on `CALIBRATION_FIRST_RUN`'s next-steps list;
this is a second, independent reason for it. Item 2 on that list — N > 1 on the
live side — is what would let the 2.35x spread be quantified as a variance
rather than observed as an anecdote.

## What this establishes

**Does:** scheduling delay is the same quantity on both sides, both sides
produce it, the comparison runs under the pre-registered rule, and the residual
definitional difference has been measured at 0.004% rather than assumed small.

**Does not:** establish that the simulator's scheduling delay is accurate. It
establishes that on an uncontended two-on-two scenario, at N=1, with different
schedulers on the two sides, the two figures are within one scheduler tick of
each other. That is a floor-level check, and the tests say so in those words
rather than letting a green row imply more.
