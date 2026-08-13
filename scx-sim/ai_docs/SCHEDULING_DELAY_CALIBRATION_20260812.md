# Scheduling delay: the pre-registered bound rejects, and the simulator is 25-58x low

Follow-up to `CALIBRATION_FIRST_RUN_20260812.md` and to the off-CPU
reclassification. Closes `mb sim-a4y1j`.

`Metric::SchedulingDelay` had a tolerance and no measurement: the bound was
registered first, deliberately, before either side could compute the quantity.
This wires both sides and reports what came out.

Reproduce with:

```bash
cd scx-sim
cargo test -p scxsim-calibration --features sim \
  --test calibrate_sched_basic_proportional the_findings_as_first_measured \
  -- --nocapture
```

## Result

```text
  scheduling_delay  [cg_0]   sim=150.500us   vm=3.694ms   gap +95.9%   +/-20.0% or +/-4.000ms   agree
  scheduling_delay  [cg_1]   sim=150.250us   vm=8.683ms   gap +98.3%   +/-20.0% or +/-4.000ms   DISAGREE
```

| cgroup | sim | vm | \|delta\| | % of 4ms arm | relative | ratio | verdict |
|---|---|---|---|---|---|---|---|
| cg_0 | 150.5us | 3.694ms | 3.544ms | 89% | 95.9% | 24.55x | agree |
| cg_1 | 150.2us | 8.683ms | 8.533ms | **213%** | 98.3% | **57.79x** | **DISAGREE** |

**Both cgroups fail the relative arm.** cg_1 fails the absolute arm as well and
is rejected. cg_0 survives only because its live value is small enough that a
4 ms floor still covers a 24.55x error.

## The finding

**The simulator models 25-58x too little scheduling delay.** 150us against a
live 3.7-8.7ms.

The direction is the one the simulator's construction predicts. It has no IRQs,
no timer ticks, no kernel threads, no host, and only the scenario's own tasks
exist — so its scheduling delay is a **floor** on what a real machine shows,
not an estimate of it. That was written into `SimRun::run_delay`'s
documentation before this measurement existed, and `mb sim-a4y1j` flagged it
when the metric was first filed.

This is the same missing-interference story as `Metric::OffCpuTime`. The
difference that matters: off-CPU time was ruled *not comparable* because the
guest's version is dominated by virtualization overhead the simulator has no
counterpart for. Runqueue wait has no such escape — both sides measure the same
physical quantity (see below), so the gap is a fidelity result and not a
definitional artefact.

## Why an earlier version of this document said the opposite

An earlier measurement agreed on both cgroups, and this document reported that.
It was taken against a **lowering defect**, not against the simulator.

ktstr's `SpinWait` is a busy loop with no yield point. The IR lowering turned it
into repeating 500us `Run` chunks, inserting voluntary yields the real workload
does not have, so the phase ended every slice and the scheduler's 20 ms
`SCX_SLICE_DFL` never bound. That produced 24029 phase-bound dispatches per task
against the live guest's 710, and each one contributed a fraction of a
microsecond of queueing. Summed, the fractions came to 6.007ms — which happened
to land between the two live values and read as agreement.

`676b42f` ("fix the 68x slice divergence — it was the lowering, not the
simulator") corrected it. Dispatches per task fell 39.8x, from 24029 to 603, and
the simulated scheduling delay fell **39.9x** with them. The matching factor is
the mechanism, not a coincidence.

The lesson is not about this metric. A quantity that is a sum over dispatches
inherits any defect in the dispatch count, and will look plausible while doing
so.

## The bound was not moved, in either direction

`Tolerance::relative_or_absolute(0.20, 4_000_000.0)`. 20% is the relative error
at which two policies differing by 1.5x still have disjoint bands; 4 ms is one
scheduler tick (`TICK_INTERVAL_NS`, `CONFIG_HZ` 250), below which no policy
conclusion can rest on the difference.

It agreed when the data was wrong and rejects now that the data is right. That
is the entire reason for fixing a bound before the data exists, and it is why
neither arm was touched on seeing either result.

Verified unaltered at integration `39fd51e` after the #85 merge resolved a
three-hunk conflict in `Metric::spec()` against ktstr's `MeanSliceLength`: this
metric still reads `0.20 / 4_000_000.0`, and `MeanSliceLength` still reads
`0.10 / 50_000.0`.

## The two definitions, checked before the numbers were compared

This is the step off-CPU time skipped.

**Live.** `mean_run_delay_us` is the mean over the cgroup's WORKERS of each
worker's whole-run delta in `task->sched_info.run_delay`, read per worker from
`/proc/self/task/<tid>/schedstat` field 2. Established from ktstr's source, not
inferred: `src/workload/worker/sched.rs` for the read,
`src/assert/reductions.rs:191-200` for the reduction, and
`src/assert/stats_types.rs:425-436` stating it outright — "the mean is the
average per-worker total queued-to-run delay, and `worst_run_delay_us` selects
the single worker with the largest total queued-to-run delay (NOT the worst
single dispatch)". The kernel accumulates the field in `sched_info_arrive()` at
each dispatch as `now - last_queued`, with `last_queued` stamped by
`sched_info_enqueue()` on every enqueue including the re-enqueue of a preempted
task.

**Simulated.** Sum over episodes of `TaskScheduled - EnqueueTask`, per task,
averaged over the cgroup's tasks. Same shape. It is the rule
`TraceStats::sched_latencies` uses and
`crates/scx_simulator/tests/rundelay_tracking.rs` validates — including
accumulation across preempt/re-enqueue cycles, which is what makes it a total
rather than a first-wakeup latency.
`the_calibrations_run_delay_is_the_simulators_own_rule` pins the two
implementations together so they cannot drift.

**They are the same physical quantity.** One residual difference is measurable
and measured: a simulator dispatch that skipped the enqueue path charges no
wakeup cost where the kernel would. That is 1 dispatch of 603. Valued at a
generous 10us of wakeup path, it is 10us against a 3.5-8.5ms gap — three
orders of magnitude too small to explain it.

## What this establishes

**Does:** scheduling delay is the same quantity on both sides; both sides
produce it; the comparison runs under a bound fixed before the data existed;
that bound rejects; and the rejection is separable from fixture noise. The live
side's own between-worker spread on an identical workload is 2.35x, and the
gaps are 24.55x and 57.79x — an order of magnitude clear of it. This is the
first hard fidelity result the calibration harness has produced.

**Does not:** quantify the shortfall precisely. N=1 on each side, two different
schedulers (guest `scx-ktstr`, simulator `simple`), and an uncontended scenario
whose true runqueue wait is small in absolute terms. "25-58x low on an
uncontended two-on-two workload" is the honest statement; a single ratio is not.

**Next, in dependency order:** a contended scenario (more workers than CPUs) so
the quantity is large enough that the relative arm binds rather than the tick
floor; N > 1 on the live side so the 2.35x spread becomes a variance rather than
an anecdote; and matching schedulers, without which the residual is attributable
to policy as much as to fidelity.
