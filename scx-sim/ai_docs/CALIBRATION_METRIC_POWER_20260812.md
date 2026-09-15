# Which calibration metrics can actually disagree, for `sched_basic_proportional`

**2026-08-12.** tg `which-calibration-metrics-can-actually-disagree`.

A metric that cannot disagree is not a check. This measures, rather than
argues, which of the six registered calibration metrics are capable of
registering a disagreement for the one scenario we have calibrated.

**Answer up front: none of the six is currently doing useful verification work
for this workload.** Two are unmeasured, one is an invalid cross-backend
comparison, two are structurally pinned, and one is structurally guaranteed to
disagree. The three rows that read `agree` in our first calibration agree
because two machines were fully busy, not because the simulator is faithful.

## Method

Sweep a parameter that *should* move a metric and observe whether it does.
All runs at `ba8d028`, `cargo nextest run -p scxsim-calibration --features sim
--test calibrate_sched_basic_proportional`, seed pinned.

**One methodological rule, learned the hard way earlier today: a flat metric
under a knob that is not wired proves nothing.** So knob liveness is
established first, and any knob that fails to engage is reported as
inconclusive for the metrics it was meant to test — not as evidence of pinning.

## Step 1 — knob liveness

| Knob | Change | Effect | Live? |
|---|---|---|---|
| `SCX_SIM_RUN_JITTER_CV_PPM` | 0 → 200k → 800k | slices 47,924 → 48,067 → 46,640 | **Yes** |
| `SCX_SIM_WAKEUP_FLOOR_NS` | → 50,000 | wake p99 5.701µs → **95.024µs** | **Yes, strongly** |
| `SCX_SIM_OVERHEAD=0` | all overhead off | occupancy 0.9995 → **1.0000**; slices → 48,080; wake → 0 | **Yes** |
| `SCX_SIM_INVOL_CSW_NS` | 1,000 → 200,000 (**200x**) | *nothing changed, not one digit* | Parsed and reaches `Scenario::builder()`, but **no effect on this workload** |
| `SCX_SIM_RBC_NS` | 10 → 1,000 (**100x**) | *nothing changed* | Same |

The last two are not dead knobs — `OverheadConfig::from_env()` is read from the
same `Scenario::builder()` call as the noise config that demonstrably works
(`safe/scenario.rs:825-826`), and both variables are parsed (`:466-481`). They
have no effect **because this workload never incurs the costs they price.**

That is itself the central finding. **Two always-runnable spinners on two CPUs
never involuntarily switch, never migrate, and never wake.** Each CPU selects
the same task forever; the 48,067 "time slices" are slice expiries followed by
re-selection of the same task, so no context-switch cost is charged, no
migration penalty applies, and only 2 wake events occur in twelve seconds.
Making a context switch 200x more expensive costs nothing when you never take
one.

## Step 2 — per-metric verdict

| Metric | Tolerance | Verdict | Evidence |
|---|---|---|---|
| **occupancy** | ±5% | **Structurally pinned.** Full achievable range is 0.9995 → 1.0000 — **0.05%, one hundredth of its own tolerance.** It cannot fail. | `SCX_SIM_OVERHEAD=0` sweep |
| **cpu_time** | ±10% | **Structurally pinned.** 99.95% of `2 x 12s` capacity under every sweep, including 80% CV jitter and 200x CSW cost. Saturating spinners consume the machine; total CPU is `wall x nr_cpus x occupancy` and is not a scheduling outcome. | jitter + CSW sweeps |
| **off_cpu_time** | ±10% | **Invalid, and doubly so.** Already established as dominated by virtualization overhead on the VM side. Additionally, on the sim side it is computed as `wall − runtime` — the arithmetic complement of `cpu_time`, so it is pinned by the same capacity argument and carries no independent information. | derivation + sweeps |
| **migrations** | ±25% or ±2 | **Structurally guaranteed to disagree.** Sim is 0 by construction (2 tasks, 2 CPUs, nothing to migrate). VM reports 16, and the harness itself notes the two sides use different estimators — kernel-exact vs userspace-sampled. Permanent DISAGREE that carries no information. | baseline; harness footnote |
| **context_switches** | ±15% or ±5 | **Not measured** (no VM counterpart) **and invariant on the sim side** — 48,067 identical across the CSW and RBC sweeps. | sweeps |
| **wake_latency** | ±20% per percentile | **Not measured** (no VM counterpart), but the **sim side is live and highly responsive**: 5.701µs → 95.024µs under `WAKEUP_FLOOR_NS`. Only n=2 samples, so the harness correctly refuses to score it. | `WAKEUP_FLOOR_NS` sweep |

### Scoreboard

- Capable of disagreeing **and** actually compared: **0 of 6.**
- Live and responsive but uncompared: **1** (`wake_latency` — needs VM-side capture and more samples).
- Structurally pinned: **3** (`occupancy`, `cpu_time`, `off_cpu_time`).
- Structurally always-disagreeing: **1** (`migrations`).
- Unmeasured and invariant: **1** (`context_switches`).

The `occupancy` figure is the sharpest way to state the problem: **its entire
achievable range for this workload is 1/100th of its tolerance.** A bound that
wide around a quantity that stiff is not a test, and the fact that it was
declared before the run — which is what normally makes a tolerance
trustworthy — does not help, because pre-registration protects against
choosing a bound to fit the data, not against choosing a metric that cannot
move.

**This does not make the calibration harness wrong.** Its four-verdict design,
pre-registered tolerances, and mandatory negative control are all sound, and
the negative control did correctly reject a 3x-scaled occupancy. The defect is
in the *pairing of this workload with these metrics*, not in the machinery.

## Step 3 — what workload would exercise them

Each pinned metric is pinned by a specific property of two saturating spinners.
Remove that property and the metric becomes informative.

| Add to the scenario | Unpins |
|---|---|
| **Blocking / sleep-wake cycles** | `wake_latency` gets real sample counts instead of n=2, and `off_cpu_time` becomes a genuine scheduling outcome rather than the complement of a capacity constant. This is the single highest-value change. |
| **Oversubscription** (more runnable tasks than CPUs) | `cpu_time` stops being capacity-pinned: with 3 tasks on 2 CPUs, per-cgroup CPU time becomes a *fairness decision* the scheduler makes and can get wrong. `context_switches` becomes real, and involuntary-CSW cost starts being charged, which also unpins `occupancy`. |
| **Asymmetric load or weights** | `cpu_time` gains a directional expectation — the simulator can be wrong in a way that a symmetric workload cannot express. |
| **More tasks than CPUs, or cpuset changes mid-run** | `migrations` becomes reachable on the sim side instead of structurally 0. |

**Recommendation, in priority order.** Add one oversubscribed scenario (say 3
or 4 spinners on 2 CPUs) and one sleep-wake scenario. Between them those two
unpin all five of the currently-uninformative metrics, and neither requires new
simulator capability — both are expressible in the existing IR. `sched_cpuset_split`
(4 cores, disjoint cpusets) is already ported and may partially exercise
migrations; it is worth calibrating next for that reason alone.

**Until such a scenario exists, the honest description of our first calibration
is: it demonstrates that the pipeline runs end to end and that the negative
control rejects, and it does not yet constitute evidence that the simulator is
faithful.** That is a real result — the plumbing works — but it should not be
cited as fidelity.

## The general rule

Before adopting a metric into the calibration table, **sweep a parameter it
ought to respond to and confirm it moves.** If the mechanism visibly engages
elsewhere while the metric stays flat, the metric is measuring the machine's
capacity rather than the scheduler's behaviour. Cheap to check, and it is the
calibration analogue of asking whether a test would fail if the code were
reverted.
