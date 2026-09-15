# First calibration run: `sched_basic_proportional`, simulator vs live guest

**Date:** 2026-08-12
**Outcome:** `GAP (DISAGREE)` — a real, interpretable result. Not a pass, not void.
**Reproduce:** `cargo test -p scxsim-calibration --features sim -- --nocapture`

The first application of the pre-registered rejection rule in
`scxsim-calibration` to actual data from both backends. Every tolerance below
was committed in `Metric::spec()` before this data existed. Nothing was widened,
re-derived or special-cased after seeing results.

## Result

```text
calibration `sched_basic_proportional`   ktstr 85c72e1   guest kernel 6.14.11

  occupancy                sim=0.9995    vm=1.0005    gap  +0.1%   +/-5%            agree
  cpu_time        [cg_0]   sim=11.994s   vm=12.011s   gap  +0.1%   +/-10%           agree
  cpu_time        [cg_1]   sim=11.994s   vm=12.002s   gap  +0.1%   +/-10%           agree
  off_cpu_time    [cg_0]   sim=0.0005    vm=0.0036    gap +85.4%   +/-10%           DISAGREE
  off_cpu_time    [cg_1]   sim=0.0005    vm=0.0043    gap +88.0%   +/-10%           DISAGREE
  migrations               sim=0         vm=16        gap +100%    +/-25% or +/-2   DISAGREE
  context_switches         sim=48067     vm=-                                       not-measured
  wake_latency    [p99]    sim=5.701us   vm=-                                       not-measured

  negative control: live occupancy x3 -> DISAGREE (rejected as required)
  outcome: GAP (DISAGREE)
```

## How much of the rule actually bound

**Four of the six registry metrics were comparable; two were `NotMeasured`.**
The eight table rows flatter that: `cpu_time` and `off_cpu_time` are reported
per cgroup, so the four comparable rows cover three distinct quantities.

The negative control rejected a 3x-wrong occupancy through the same tolerance
the real comparison used, so the bounds demonstrably can reject something and
the run is citable. Had it not, every metric here would be uncitable —
*including the ones that agreed*.

### The two `NotMeasured` are different kinds of gap

**`wake_latency` — the live run did not sample it.** The guest sidecar carries
`p99_wake_latency_us: 0.0` immediately beside `wake_measured: false`. The zero
is a placeholder. The simulator *does* produce wake latencies here (p99 5.7us,
n=2), so the two numbers are sitting right next to each other and comparing them
would yield a confident verdict about a quantity nobody measured. `crate::vm`
reads the flag and returns `None`;
`vm::tests::unmeasured_wake_latency_is_none_not_zero` fails if that ever
degrades to `Some(0)`. This was the most available way to fake a passing metric
in this whole exercise.

Even had it been measured, N=1 could not have supported a percentile verdict:
`MinSamples::PERCENTILE` is 100.

**`context_switches` — the two sides count different populations.** ktstr's
sidecar has no per-task counter. Its `monitor.schedstat_deltas.total_sched_count`
= 1978 is VM-wide: every task in the guest, kernel threads and the monitor
included. The simulator's 48067 is two workload tasks. Passing the 1978 in would
have compared two different things and reported the difference as fidelity.

## Findings

### F1 — the simulator models roughly 7x too little off-CPU time

0.05% simulated against 0.36%/0.43% live, against a 10% bound.

The simulator has no IRQs, no timer ticks and no competing guest work, so a task
that never sleeps is never off-CPU. A real spinner on a real CPU loses ~0.4% to
interference. Both sides compute the same thing — `(wall - cpu) / wall`,
matching ktstr's `derive_off_cpu_ns` — so this is not a definitional artefact.

One measurement artefact was found and ruled out: `Trace::total_runtime` closes
a task's running interval on preempt/yield/sleep/completion but **not** on
`SimulationEnd`, so a task still on-CPU at the end loses its final partial
slice. That is bounded by one slice — 500us against 12s, 0.004% — roughly ten
times smaller than the discrepancy, and it biases off-CPU time *up*, i.e. in the
direction that would shrink this gap rather than create it.

This is the finding I am most confident is genuinely the simulator rather than
the scheduler difference below: no interference source exists in scx-sim at all,
for any scheduler.

### F2 — zero migrations against the guest's sixteen

A zero is exactly what a broken derivation would also produce, so this was
verified before being reported. `the_zero_migration_count_is_an_observation_not_an_unpopulated_field`
proves it: 48067 dispatches, the run spans both CPUs, and each task sat on
exactly one of them (pid 1 → CPU 1, pid 2 → CPU 0) for twelve simulated seconds.
`simple` places each task once and never rebalances.

The estimators also differ, and neither side is wrong: ktstr counts migrations
in userspace by calling `sched_getcpu()` once per spin iteration (~35us apart on
this workload) and noticing changes; the simulator counts every placement change
the kernel made. That difference explains a few counts, not 0 against 16.

Attribution here is genuinely uncertain — see the caveat.

### F3 — what agreed is the least informative part of the run

`occupancy` and `cpu_time` agree to 0.1%. But two saturated spinners on two
dedicated CPUs for 12s must total ~24s of CPU under any scheduler that is not
broken. On this workload those metrics are close to unfalsifiable, and their
agreement should not be read as evidence of fidelity. The metrics that
discriminate are the ones that failed.

This is an argument for calibrating a scenario with contention, not for
loosening anything.

## The caveat that bounds all of it

**The two backends ran different schedulers.** The guest ran ktstr's
`scx-ktstr`; the simulator ran `simple`, because scx-sim does not have
scx-ktstr. Both are minimal global-DSQ schedulers, so the fairness question is
meaningful on either, but any discrepancy above is attributable to the scheduler
difference at least as much as to simulator infidelity.

Concretely: F1 survives the caveat (scx-sim has no interference model for *any*
scheduler). F2 largely does not — "`simple` never rebalances" is a statement
about `simple`, and scx-ktstr is a different scheduler that may well migrate.

Until scx-sim can run scx-ktstr, this is a **scenario-level** calibration.
Nothing here supports a sentence of the form "the simulator is N% off".

## Provenance

| | |
|---|---|
| live half | `scxsim-calibration/vm_runs/sched_basic_proportional-6.14.11-85c72e1.ktstr.json`, committed verbatim |
| guest | source-built 6.14.11, `scx-ktstr`, ktstr `85c72e1`, `passed: true` |
| simulated half | produced on demand; `ktstr ops -> IR -> Scenario -> Simulator::run`, `simple` |
| lowering fidelity | EXACT — zero approximations, so no discrepancy above is the lowering's |
| samples | **N = 1 per side** |

The live sidecar was recovered from
`$CARGO_TARGET_DIR/ktstr/<kernel>-<commit>/` after the guest run that produced
it. Before that, the only VM data in the tree was two integers in a `println!`
in a test — which is why the fixture is now committed verbatim rather than
summarised. See `vm_runs/README.md`.

## What this run does and does not establish

**Does:** the calibration harness works end to end against real dual-backend
data; the negative control rejects; `NotMeasured` is honoured rather than
assumed-equal; and there are two concrete, reproducible discrepancies to
investigate.

**Does not:** establish that the simulator is calibrated. Four of six metrics
were comparable, from one run on each side, between two different schedulers,
on a workload whose agreeing metrics are nearly unfalsifiable. This is a
starting point with a rejection rule attached, not a fidelity certificate.

## Next, in dependency order

1. **Make the schedulers match.** Either add `scx-sim/schedulers/ktstr/wrapper.c`
   so the simulator can run scx-ktstr, or point the ktstr test at a scheduler
   both backends have. Until then F2 cannot be attributed and no future finding
   can be either. This is the top blocker.
2. **Get N > 1 on the live side.** Every percentile metric is unreachable at
   N=1, and no run-to-run variance is visible, so a 0.1% agreement cannot be
   distinguished from luck. The guest path is reproducible (see
   `vm_runs/README.md`); this is a matter of running it repeatedly and keeping
   the sidecars.
3. **Calibrate a scenario with contention** — more workers than CPUs — so the
   agreeing metrics have a chance to disagree, and so wake latency exists to
   measure on both sides.
4. **Decide whether F1 is worth modelling.** An interference model is a large
   change to scx-sim and may not be worth it; the alternative is to document
   off-CPU time as a known, bounded divergence under Principle 1 rather than
   leave it as a recurring calibration failure.
