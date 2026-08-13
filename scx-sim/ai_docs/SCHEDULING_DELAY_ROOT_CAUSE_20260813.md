# Why the simulator modelled 25-58x too little scheduling delay

Follow-up to `SCHEDULING_DELAY_CALIBRATION_20260812.md`, which measured the gap
against a bound registered before either side could compute the quantity.

**Answer: the simulator has a kernel-overhead model for the enqueue-to-run path
and it was applied to wakeups only.** Every preempted or slice-expired task was
re-dispatched charged a flat 250 ns.

## The constant, and that it is computed rather than emergent

The headline figure of ~150 us is not a constant; it is `603 dispatches x 250 ns`.
The per-episode distribution is what identifies it:

```text
cg_0  n=602  min=250  max=250  DISTINCT VALUES = 1
cg_1  n=601  min=250  max=250  DISTINCT VALUES = 1
```

Every enqueue-to-dispatch interval was exactly 250 ns. It also held at 249.99 ns
before the lowering fix, across 24029 dispatches — so it survived a 40x change in
dispatch count untouched. A quantity that does not move when the thing it is
summed over changes by 40x is not emergent.

## The mechanism

| where | what |
|---|---|
| `engine.rs` `start_running` | the wakeup-latency floor — 3 us log-normal, 3.5% Pareto tail — gated on `enqueued_at_ns` being `Some` |
| `engine.rs` `handle_task_wake` | the ONLY place `enqueued_at_ns` was written |
| `engine.rs` `start_running` | clears it after every dispatch |

So a task that is preempted or exhausts its slice reached dispatch with
`enqueued_at_ns == None`, the floor block was skipped, and only the fixed
dispatch overheads were charged — `dsq_consume_ns` 100 + `running_overhead_ns`
50 + the rest, totalling 250 ns.

Measured on the calibration workload:

```text
cg_0 pid=1: woke=1  enqueued=602  scheduled=603  preempted=597  slept=0
cg_1 pid=2: woke=1  enqueued=601  scheduled=602  preempted=596  slept=0
```

**One dispatch in 603 got the model.** `overhead.enabled` is `true`; the gate
that failed was `enqueued_at_ns`, not the switch.

The structural reason it was missed: the slice-expired path re-enqueues through
the shared `stop_and_reenqueue` spine, whose `pre_enqueue` / `post_enqueue`
callbacks receive `SimulatorState` but **not** the task table — so the site that
needed to write the field could not reach it. The field name compounds it: it is
called `enqueued_at_ns`, implying any enqueue, while its own comment says "Track
wakeup time".

## The kernel does the opposite, and the live data proves it

`sched_info_enqueue` restamps `last_queued` on every enqueue, including the
re-enqueue of a preempted task. This does not need to rest on a citation: the
workers in this fixture wake once and never sleep again (`slept=0`). If the
kernel only charged run_delay on wakeups they would report ~0. They report
3.694 ms and 8.683 ms across ~640 dispatches.

## Is it the same quantity? Yes — quantified, because this is where off_cpu_time died

Same contamination mechanism (hypervisor steal), three orders of magnitude less
exposure, for a structural reason.

```text
host_dilation 1.0006324  ->  0.0632% of wall is undelivered vCPU time
                         ->  7.58 ms steal per vCPU over 12s, 15.17 ms total
```

That total is the same order as the entire run_delay signal (12.38 ms), so it
cannot be waved away. But steal only enters run_delay during an interval the task
is **already queued**, so it is 0.0632% *of the queued time*, not of wall:

| metric | value | steal inside it | share |
|---|---|---|---|
| cg_0 run_delay | 3.694 ms | ~2.3 us | 0.063% |
| cg_1 run_delay | 8.683 ms | ~5.5 us | 0.063% |
| cg_0 off_cpu_time | 43.0 ms | 7.58 ms | **18%** |

`off_cpu_time` is `wall - cpu`, so it swallows all steal regardless of queue
state — which is why it was 83-91% non-scheduling and run_delay is ~0.06%.
**The comparison is valid; this is a modelling failure, not a measurement
artefact.** (Caveat: the 0.063% assumes steal is uniform in time and uncorrelated
with queueing. `host_state_reload` is 264359 per vCPU, so exits are frequent; a
correlation would raise it, but not by the three orders of magnitude required.)

## Is the under-modelling uniform? No — and that is the good news

Simulated run-delay per dispatch, 2 CPUs, sweeping worker count:

| workers | ns per dispatch |
|---|---|
| 2 (uncontended) | 247.5 |
| 4 | 19,485,574 |
| 8 | 59,004,227 |
| 16 | 141,259,873 |
| 32 | 286,061,539 |

Under contention the simulator's queueing is **emergent and correct** — 4 workers
on 2 CPUs waiting ~19.5 ms against a 20 ms slice is exactly one slice. The
constant-250 ns regime is specific to the uncontended case, where there is
nothing to queue behind and only per-dispatch overhead remains.

So the defect is an **additive ~4 us per dispatch**, not a multiplicative error
on queueing:

- **uncontended** — ~94% of the signal; 25-58x low, and zero gradient (one
  distinct value: the simulator could not express any difference in delay
  between two loads at all)
- **contended** — 4.2 us against 19.5 ms, i.e. 0.02%; negligible

### Consequence for scx#3618: unaffected

That repro operates on waits of 480 ms and multi-hundred-ms to multi-second
scale (`pr3618_cpumax_unbounded_wait.rs:220,275,419`). A missing ~4 us per
dispatch is 0.0008% of a 480 ms wait — five orders of magnitude below the
effect, and its waits are throttle-driven queueing, which the sweep above shows
the simulator models correctly. Directly verified: with the fix applied, all
seven `pr3618_cpumax_unbounded_wait` tests pass unchanged, including
`the_wait_converges_for_a_given_quota` and
`does_a_lower_watchdog_preserve_the_gradient`. **Both the gradient and the
absolute magnitudes in scx#3618 stand.** The caveat is needed only for
microsecond-scale latency claims on uncontended workloads.

## The fix, and what it does not fix

One site: stamp `enqueued_at_ns` in `stop_and_reenqueue`, where the re-enqueue
actually happens.

```text
                       before      after       live
cg_0 scheduling_delay  150.5us     2.720ms     3.694ms     24.55x -> 1.36x
cg_1 scheduling_delay  150.2us     2.523ms     8.683ms     57.79x -> 3.44x
```

**It is necessary but not sufficient**, and the follow-up found both the
remaining sites and an error in how this residual was first measured.

Two sites in `handle_task_phase_complete` also failed to stamp. They are now
stamped too, and with that every `EnqueueTask` emitter in the engine charges the
modelled path:

| site | function | stamps |
|---|---|---|
| 4019 | `handle_task_wake` | yes, originally |
| 4112 | `handle_slice_expired` (via `stop_and_reenqueue`) | yes, this change |
| 4402 | `handle_task_phase_complete`, explicit `Phase::Yield` | yes, follow-up |
| 4402 | `handle_task_phase_complete`, plain Run -> Run | **no, by decision** |
| 4489 | `handle_task_phase_complete`, wake chain | only on an explicit yield |

**The Run -> Run boundary is deliberately NOT charged** (owner ruling,
2026-08-13). `wakeup_latency_floor_ns` models the cost of getting a task ONTO a
cpu; a task crossing a phase boundary never left one, so the boundary is a
scripting artifact rather than a kernel event. Charging it was tried and the
measurement was silent — the calibration fixture crosses it 9 times in ~1200
dispatches, and cg_0 moved 2.720ms -> 2.591ms against a live 3.694ms, slightly
further away and well inside noise. With no empirical signal the principled
model decides, because it generalises to phase structures nobody has tested.
`nr_yields` is the discriminator: an explicit `Phase::Yield` is a real
`sched_yield()` and is charged. `run_to_run_phase_boundary_is_deliberately_not_charged`
guards the non-charge so it cannot be "fixed" into an oversight.

**Correction to the first measurement of the residual.** It was originally
reported as 34-36% of `simple`/`cosmos` dispatches and 80-83% of `lavd`'s still
under 1 us. That was measured on a workload of repeating 50 ms `Run` phases, and
a `Run` phase followed by another `Run` phase makes the task dequeue and
re-enqueue at the boundary — a scripting artifact, the same anti-pattern
`676b42f` fixed on the lowering side. It manufactured the very dispatches it
then counted. On a continuous single-phase workload, before any further fix, the
residual was 4% and 0%:

| scheduler | chunked 50 ms | continuous | continuous, after the follow-up |
|---|---|---|---|
| simple | 34% | 4% | 2% |
| cosmos | 34% | 4% | 2% |
| lavd | **83%** | **0%** | 0% |

The `lavd` asymmetry was a property of the DISPATCH MIX, not of a LAVD-specific
uncharged path: LAVD preempts far less often (38 preemptions against `simple`'s
198 on the same scenario), so phase-boundary yields dominated its dispatches.

**There are no uncharged paths left.** What remains under 1 us is the modelled
log-normal's own lower tail: samples of 818-996 ns against a theoretical minimum
of `3000 * exp(-1.733)` = 530 ns. "Under 1 us" was a poor threshold — it sits
inside the distribution — which is why the regression tests assert on the mean.

Note also that the tolerance was not touched, and cg_1 still **rejects** after
the fix (70.9% relative, 6.16 ms against the 4 ms absolute arm). The bound was
derived blind and has now survived two measurements moving underneath it.

## Blast radius

1148 `scx_simulator` tests: 1147 pass. Two needed attention:

- `test_yield_keeps_task_runnable_and_making_progress` — the yielder completed 9
  of 10 iterations instead of 10. The scenario needed ~10 x 20 ms slices inside a
  200 ms run and was fitting the tenth only because re-dispatch was free. Fixed
  by giving the run headroom (`duration_ms` 200 -> 400); the assertion is
  unchanged at 10.
- `test_bug1_canonical_undersub_subprocess_reproduces_throttle` — **still
  failing, deliberately not touched.** It asserts `is_throttled == 1` sampled at
  end of run; it now reads 0 while `nr_throttled_periods` is 5/6, so throttling
  still happens and only the final period's phase has shifted. That is exactly
  the end-of-run sampling question already tracked as `mb sim-560f79` /
  `mb sim-1ei8j`, whose sibling test is `#[ignore]`d for the same reason. It
  perturbs a load-bearing repro, so it is the owner's call, not this task's.
