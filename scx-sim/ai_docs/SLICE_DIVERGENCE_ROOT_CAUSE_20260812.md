# The 68x slice divergence: root cause, fix, and what it invalidates

**Date:** 2026-08-12
**Verdict:** not a simulator fidelity gap. A defect in the IR lowering, which I wrote.
**After the fix:** 68x → **1.70x**, and the residual is a real scheduler difference.

## What was measured

The first wprof trace comparison found, for `sched_basic_proportional`:

| | live | sim | |
|---|---|---|---|
| workload on-CPU | 24042.9 ms | 23988.0 ms | agree to 0.23% |
| workload slices | 710 | 48067 | **68x** |
| mean slice | 33.863 ms | 0.499 ms | |

Same total CPU, reached through completely different scheduling.

## The experiment that settled it

Hold topology, cgroups, pids, task count, duration and scheduler constant. Vary
exactly one thing: the length of the workload's `Phase::Run` chunk. Built
through the real `lower()` + `to_scenario()` path so nothing else could drift.
Scheduler is `simple`, which requests `SCX_SLICE_DFL` = 20 ms on every
`scx_bpf_dsq_insert` (`scx_simple.bpf.c:71,82,93`).

Two hypotheses predicting different shapes:

1. *The simulator ignores the scheduler's requested slice* → slice count
   insensitive to phase length, a flat line.
2. *The workload phase ends the slice* → slice count tracks
   `duration / phase_len`.

`cargo run -p scxsim-calibration --features sim --example slice_sweep`:

```text
    phase len     slices   mean slice     on-CPU    slices if PHASE governed
      0.100ms      19904      0.100ms   1994.9ms                       20000
      0.500ms       4013      0.498ms   1998.1ms                        4000
      1.000ms       2013      0.993ms   1999.0ms                        2000
      5.000ms        405      4.916ms   1990.9ms                         400
     20.000ms        149     13.341ms   1987.8ms                         100
     50.000ms        121     16.356ms   1979.1ms                          40
   1000.000ms        102     19.322ms   1970.9ms                           2
```

Below 20 ms the count tracks the phase prediction almost exactly. At and above
20 ms it plateaus and the mean pins to **19.322 ms — `SCX_SLICE_DFL`**.

**The engine implements `min(phase, scheduler_slice)`.** That is correct
kernel-like behaviour: a task that finishes its work chunk yields, otherwise it
is preempted at slice expiry. Hypothesis 1 is eliminated — it predicted a flat
line and the line is not flat.

End-to-end arithmetic: at the 500 us default, 1 s gives 4013 slices, so 12 s
gives 48156. Observed in the real comparison: 48067. Within 0.2%.

## Root cause

`DEFAULT_SLICE = 500us` in `scxsim-workload-ir/src/lower.rs`, applied to
`SourceWorkType::SpinWait`.

ktstr's `SpinWait` is a **continuous busy loop with no yield point**. Lowering
it to `Phase::Run(500us)` with `Repeat::Forever` inserts ~48000 voluntary
yields per run that the real workload does not have. The simulator then
correctly ends the slice at each one, and the scheduler's 20 ms request never
gets to bind.

The constant's own comment said it was "chosen to be long enough that the
scheduler makes a decision about it and short enough that a repeat loop stays
responsive" — responsiveness reasoning for a workload with something to respond
to. `SpinWait` has nothing. The number was never derived from ktstr.

### The worse half: the lowering called this EXACT

`SpinWait` sat in the arm labelled *"exact: pure time, nothing dropped"* and
`ir.fidelity.is_exact()` returned true. The calibration and the trace
comparison both assert on that and both passed — while the lowering had
invented the single most consequential parameter in the run.

A fidelity report that says "exact" while fabricating the scheduling quantum is
worse than no report, because downstream code trusts it. `sched_basic_proportional`
was chosen as the first calibration subject *precisely because* it lowered
exactly.

## The fix

`SpinWait` now lowers to a run phase that outlasts the scenario, so the only
thing that can end its slice is the scheduler:

```rust
fn continuous_run(ctx: &Ctx) -> Phase { Phase::Run(ctx.scenario_duration) }
```

`DEFAULT_SLICE` survives only for work types that have a genuine yield point
but do not say how much work sits between yields, and its doc now says that
every such use must record an approximation.

### Result

| | live | sim before | sim after |
|---|---|---|---|
| workload on-CPU | 24042.9 ms | 23988.0 ms | 23999.7 ms (0.18%) |
| workload slices | 710 | 48067 (68x) | **1205 (1.70x)** |
| mean slice | 33.863 ms | 0.499 ms | **19.917 ms** |

Mean slice now pins to `SCX_SLICE_DFL`, which is the scheduler deciding rather
than the workload model deciding for it.

**The residual 1.70x is a real finding, not an artifact.** The live guest's
`scx-ktstr` runs tasks ~33.9 ms per slice against `simple`'s 19.9 ms. That is a
scheduler-level difference between two different schedulers — exactly the class
of question this comparison exists to surface, and it was invisible underneath
the self-inflicted 68x.

## What this invalidates

**Survives — unchanged, and in one case strengthened:**

- **Total CPU time, occupancy, throughput over the whole run.** Agreed to 0.23%
  before and 0.18% after. These are capacity-pinned for a saturating workload:
  two spinners on two dedicated CPUs total ~24 s of CPU under any scheduler
  that is not broken. They could not have detected this and cannot detect the
  next one either.
- **Fairness between cgroups over the whole run.** Both cgroups still get
  comparable CPU.
- **`migrations = 0`.** *Strengthened.* The simulator produced zero migrations
  at 48067 dispatches and still zero at 1205 — the same answer across a 40x
  change in dispatch rate, so it is a property of `simple`'s placement, not an
  artifact of dispatch frequency.
- **The unmodelled-overhead measurement** (kthread 4.080 ms, IRQ 22.002 ms,
  workqueue 21.584 ms). Live-side only; the fix does not touch it.

**Invalidated — any prior simulator result about these, on a `SpinWait`
workload, is wrong by up to 68x:**

- **Context-switch and dispatch rate.** Directly the defect.
- **Anything latency-sensitive: wake latency, scheduling delay, tail
  behaviour.** A task re-dispatched every 500 us has a completely different
  latency profile from one running 20 ms slices. Sim-side wake-latency numbers
  from before this fix should not be cited.
- **Fairness over short windows.** 68x finer interleaving makes the sim look
  far fairer at millisecond granularity than the live system is.
- **Anything about preemption**, since the tasks were almost never actually
  preempted — they voluntarily yielded.

**Off-CPU time is partially affected but the finding stands.** The gap narrowed
from 85.4%/88.0% to 77.5%/71.6% — the sim now has real preemption gaps — but it
remains DISAGREE at the 10% bound, and the live-side explanation (IRQ servicing
plus run delay, not hypervisor steal) is unchanged.

## The metric-power argument, which is the durable output

`cpu_time` and `occupancy` **agreed because they structurally cannot disagree**
for a saturating workload. They are capacity-pinned: the answer is ~24 s
whatever the scheduler does. An entire calibration passing on those two is
close to unfalsifiable, and this investigation is the proof — the largest
divergence we have measured sat underneath them, undetected, in metrics that
both agreed to 0.23%.

**Slice count and mean slice demonstrably CAN disagree — they did, by 68x.**
That makes them candidate discriminating metrics of exactly the kind the
registry is missing.

Of the two, **mean slice is the better metric**:

- It is duration-independent. Slice count scales with run length, so a
  tolerance on it silently means different things for a 1 s and a 12 s run.
- It is a `Duration`, so it fits `Quantity::Duration` and the existing
  comparison machinery without a new kind.
- It has a physical referent a reviewer can reason about — the scheduler's
  requested slice — so a tolerance can be argued from scheduling behaviour
  rather than fitted to observations.
- Slice count is recoverable from it and CPU time, so nothing is lost.

**I am not setting the tolerance.** I have seen the numbers, so any bound I
proposed would be fitted to them, which is the exact failure the pre-registered
rule exists to prevent. The derivation is handed to an agent that has not seen
the data, on the pattern used for the scheduling-delay tolerance — tg
`derive-mean-slice-tolerance-blind`.

## Next

1. **Audit the other work types for the same defect.** `YieldHeavy` and `Mixed`
   still use `DEFAULT_SLICE` for the chunk between yields. There the chunk
   length is genuinely unknown from ktstr, so the right fix is not a different
   constant — it is recording an approximation instead of claiming exact.
   Filed rather than done here to keep this change reviewable.
2. **Register mean slice** once the blind tolerance lands.
3. **Ask why `scx-ktstr` runs 33.9 ms slices** against `simple`'s 19.9 ms. Now
   that the artifact is gone, this is the real remaining divergence — and it
   needs the schedulers to match before it can be attributed.
