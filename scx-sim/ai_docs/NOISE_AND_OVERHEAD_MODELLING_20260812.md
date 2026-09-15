# Modelling OS and hardware noise in scxsim: what to build, and what not to

**Research report, 2026-08-12.** Written before any implementation, because the
two grounding facts point in opposite directions and the obvious response to
one of them is wrong.

**Method.** Part 3's last column is derived by reading our own tree at
`ba8d028`, not from papers — every claim about what scxsim does today is a
file-and-line citation and can be checked. Literature claims carry a DOI I
verified through Crossref. Where I state a magnitude I could not fetch a
primary source for, it is marked **[order-of-magnitude, uncited]** and should
be treated as a starting point for measurement, not as evidence. Where I am
reasoning rather than reporting, the paragraph says so.

**One correction to my own method, recorded because it changes how to read the
inventory.** My first pass grepped the engine with `grep -E "irq\|interrupt"`.
In ERE `\|` is a *literal pipe*, so that returned zero hits and I nearly
reported interrupts as unmodelled. They are in fact among the better-modelled
things in the engine. Every count in this report was re-run with correct
alternation. This is the same failure class as the two measurement errors in
today's UB audit: the tool answered a different question than the one I thought
I asked.

---

## The two grounding facts, and why they cut in opposite directions

1. The simulator's per-cgroup CPU-time spread is **0.0002%**; the VM's is
   **0.0699%**. The simulator is ~350x tighter.
2. The one apparent fidelity gap we chased — excess off-CPU time on the VM
   side — turned out to be **virtualization overhead** (host steal,
   `host_dilation` 1.00063), **not scheduler overhead**.

Fact 1 says "the simulator is too clean." Fact 2 says the last time we believed
that, the missing quantity belonged to the measurement apparatus and not to the
system under study. A naive "add noise until the spread matches" would have
modelled the hypervisor. **That is the central risk this report exists to
prevent, and the ranking in §4 is built around it.**

## §0. The load-bearing result: symmetric noise cannot close that gap

This is the most important finding in the report and it is derivable without
any new measurement.

**scxsim already injects noise, and it is on by default.**
`NoiseConfig::default()` (`safe/scenario.rs:219-228`) sets `enabled: true`,
`tick_jitter: true` at 2 µs stddev, and `run_jitter: true` at
`run_jitter_cv_ppm: 200_000` — a **20% coefficient of variation** applied as
`duration * (1 + normal(0, cv))` to every `Phase::Run`. The doc comment says
plainly why it exists: *"Without this, Phase::Run(250μs) executes for exactly
250μs every time, producing unrealistically tight e2e latency distributions
(p50≈p99)."*

So the premise "the sim has no noise" is false. The question is why 20% CV
per-slice noise yields a 0.0002% aggregate spread.

**Because the statistic in question is immune to it.** Per-cgroup CPU time is a
*sum over many slices*. Zero-mean symmetric noise averages out at rate
`1/sqrt(N)`. The run executed **48,067 time slices**. Treating those as
independent jittered quanta:

```
CV_aggregate  ≈  0.20 / sqrt(48067)  ≈  0.00091  =  0.091%
```

Two conclusions follow, and they matter more than the arithmetic.

**First: adding more symmetric noise cannot fix this.** To move a 12-second
aggregate by the VM's 0.0699% using zero-mean per-slice jitter you would need a
per-slice CV of roughly 15 — i.e. slice durations swinging by an order of
magnitude — which would destroy every latency distribution in the simulator to
buy one number. Any quantity aggregated over ~10^4 events is the wrong target
for a noise model. **If we tune noise against the per-cgroup spread we will
badly mis-specify it.**

**Second — I originally recorded a "live defect" here, and MEASUREMENT
REFUTED IT. The corrected version is below; the original claim was wrong.**

I argued that the observed 0.0002% spread was ~450x tighter than the 0.091%
this model predicts, and that therefore `run_jitter` must not be reaching the
workload. Both halves were wrong, and the diagnostic I proposed is what showed
it. Running the calibration at `SCX_SIM_RUN_JITTER_CV_PPM` of 0, 200_000
(default) and 800_000:

| CV | total time slices | cg_0 | cg_1 | total CPU | % of 2x12s capacity |
|---|---|---|---|---|---|
| 0% | 47,924 | 11.994s | 11.994s | 23.988s | 99.95% |
| 20% | 48,067 | 11.994s | 11.994s | 23.988s | 99.95% |
| 80% | 46,640 | 11.993s | 11.994s | 23.987s | 99.95% |

**Jitter is reaching the workload.** The slice count moves by ~1,400 between
settings — the noise is applied and it changes scheduling behaviour. The
mechanism is live, not inert.

**The metric is what cannot respond, and my model of it was wrong.** I treated
per-cgroup CPU time as a *sum of N independent jittered work quanta*, which
gives the `1/sqrt(N)` shrinkage and the 0.091% figure. It is not that. These
are **saturating spinners under `HoldSpec::FULL`** — unbounded work bounded by
wall clock. Total CPU delivered is `wall_duration x nr_cpus x occupancy`, and
occupancy is 0.9995 at every jitter setting. The CPUs are never idle, so the
total is pinned at 99.95% of the 2x12s machine capacity **whatever the jitter
does**. Jitter reshuffles *which* task runs *when*; it cannot change how much
CPU a fully-busy machine delivers, nor how two symmetric consumers split it.

So there is no 450x anomaly, no defect, and 0.091% was never the right
prediction. **The conclusion of this section survives and is strengthened**:
do not tune noise against per-cgroup CPU time. But the reason is stronger than
"the response is small" — for a saturating workload the response is
*structurally zero*, and no noise parameter at any value will move it.

**Reusable diagnostic, worth more than the original claim.** "Slice count moves
but aggregate CPU time does not" is the signature of a **capacity-bound
metric**. Before adopting any calibration metric, vary a parameter it should
respond to. If the mechanism visibly engages elsewhere while the metric stays
fixed, the metric is measuring the machine's capacity rather than the
scheduler's behaviour, and it cannot discriminate between a good model and a
bad one.

**What could produce a 0.0699% spread is asymmetry, not variance** — something
that systematically favours one cgroup over the other. Steal time (fact 2),
IRQ affinity, migration asymmetry, or cross-cgroup interference are all
candidates. Those are *mean* effects. Chasing them with a variance model is a
category error.

I flag the coincidence that the VM's 0.0699% is the same order as the 0.091%
predicted by the sim's own per-slice noise — but I do **not** claim they are
the same phenomenon, and fact 2 is a direct warning against assuming so.

---

## §3. Where time actually goes, and what scxsim models today

*(Presented before §1 and §2 because it is the part that determines what we
build; the literature follows.)*

Every "scxsim today" cell is from `ba8d028`. Magnitudes marked **[o-o-m]** are
uncited order-of-magnitude figures.

| Consumer | Magnitude | Deterministic or distributional | Scheduler-visible? | scxsim today |
|---|---|---|---|---|
| **Timer tick** | 4 ms @ HZ=250 | Deterministic period, distributional delivery (1–10 µs jitter) | **Yes** — drives `ops.tick` | **Modelled.** `TICK_INTERVAL_NS = 4_000_000` (`engine.rs:431`); jitter 2 µs stddev, on by default |
| **Context switch** | ~1–5 µs direct; up to ~10^2–10^3 µs including cache-refill effects (Li/Ding/Shen 2007) | Distributional; heavy right tail from cache state | **Yes** — the cost the scheduler is trading against | **Modelled.** voluntary 500 ns / involuntary 1000 ns, ±100 ns jitter |
| **Hardirq / softirq** | 1–10 µs per handler **[o-o-m]** | Distributional, bursty, correlated with I/O | **Yes** — steals time from the running task | **Modelled, but NOT ambient.** `IrqType::{HardIrq,SoftIrq}`, injectable per-CPU and repeating; duration is charged and *stolen* (`irq_stolen_ns`), excluded from `clock_task` (`kfuncs.rs:2481`). Nothing fires unless a scenario injects it |
| **BPF kfunc / helper call** | 10–200 ns | Modelled deterministic by tier | **Yes** — the scheduler's own cost | **Modelled.** 4 tiers: TRIVIAL 10 / SIMPLE 50 / MODERATE 100 / COMPLEX 200 ns |
| **Scheduler C code** | proportional to work done | Deterministic given the path | **Yes** | **Modelled, and unusually well.** RBC: 10 ns per *retired conditional branch*, measured by PMU. This is a real per-execution cost model, not a constant |
| **IPI delivery** | ~1–10 µs **[o-o-m]** | Distributional | **Yes** — `scx_bpf_kick_cpu` | **Modelled** at 200 ns — likely low by ~5x, see §4 |
| **Task migration** | 10–100 µs incl. cache/TLB refill **[o-o-m]** | Distributional | **Yes** — the core placement trade-off | **Modelled.** `migration_penalty_ns` 10 µs, `cross_llc_migration_penalty_ns` +25 µs |
| **Cache / memory hierarchy** | L1 ~1 ns, LLC ~10–20 ns, DRAM ~80–100 ns **[o-o-m]** | Distributional, strongly workload-correlated | **Indirectly** — via IPC, not as an event | **Not modelled directly.** Two proxies only: `dsq_consume_ns` 100 ns and the migration penalty above |
| **TLB** | miss ~10–100 ns; shootdown ~1–10 µs **[o-o-m]** | Distributional | Indirectly | **Not modelled.** Folded into the migration penalty |
| **Branch misprediction** | ~15–20 cycles **[o-o-m]** | Distributional | No | **Not modelled as a cost** — but RBC prices branches *retired*, which is the scheduler-relevant part |
| **Syscall entry/exit** | ~100 ns pre-mitigation; several hundred ns to µs with Spectre/Meltdown mitigations (Ren et al. SOSP'19) | Distributional | No, for our workloads | **Not modelled — and correctly so.** sched_ext schedulers are BPF callbacks, not syscall-driven. This is a *workload* cost, relevant only if we model syscall-heavy tasks |
| **Page faults** | minor ~1 µs, major ~10^2 µs–ms **[o-o-m]** | Heavy-tailed | **Yes** — blocks the task | **Not modelled** |
| **RCU** | callback batches, µs-scale **[o-o-m]** | Bursty | Weakly | **Not modelled.** Appears only as a `SOFTIRQ:rcu` trace label |
| **Frequency scaling / DVFS** | 10s of µs transition; up to ~2–3x throughput swing | Deterministic given governor state | **Yes** — `scx_bpf_cpuperf_set` | **PLUMBED BUT INERT.** `perf_lvl` is stored by the setter and returned by the getter (`kfuncs.rs:3270,3282`); **nothing in the engine scales work by it.** A scheduler can request a frequency, read it back, and observe no consequence. Same shape as the NUMA "publication only" verdict |
| **NUMA locality** | remote DRAM ~1.5–2x local latency **[o-o-m]** | Distributional | **Yes** | **Not modelled.** No engine NUMA concept (see `goal-simulated-numa-domains`) |
| **Load balancing** | — | — | N/A | **Not applicable.** sched_ext schedulers do their own; there is no CFS balancer to model |
| **Hypervisor exits / steal** | 1–10 µs per exit; steal unbounded under contention **[o-o-m]** | Distributional, host-dependent | Visible as missing time | **Not modelled — and MUST NOT BE.** See below |

### Hypervisor exits: an artefact of the apparatus, not a target

The off_cpu_time investigation found the VM's apparent excess off-CPU time was
dominated by host steal, with `host_dilation` at 1.00063 — virtualization
overhead, not scheduler overhead. **scxsim should never model this.** It is a
property of how we currently capture the live side, and modelling it would mean
teaching the simulator to reproduce our measurement rig.

The correct response is on the *measurement* side: subtract steal from the
live-side estimator, or capture on bare metal. Note this also means the VM's
0.0699% spread may itself be partly apparatus, which is a second reason not to
tune anything against it.

### The three findings in that table worth acting on

1. **DVFS is plumbed but inert.** The scheduler can set and read a frequency
   with no effect on elapsed time. This is worse than not modelling it: a
   scheduler that makes frequency decisions will appear to work while its
   decisions are consequence-free, and a test asserting on them would pass
   vacuously. Same failure shape as the NUMA and mitosis cell-0 findings.
2. **IRQ is modelled but never ambient.** The machinery is good — time charged,
   stolen, excluded from the task clock. But a scenario that injects no IRQs
   runs on a machine with no interrupts at all, which no real machine is.
3. **RBC is the best thing in the model and is the template.** 10 ns per
   retired conditional branch, PMU-measured, is exactly the "profile once,
   cheap model at simulation time" shape §2 describes — arrived at
   independently. The kfunc tiers, by contrast, are four hand-chosen constants.

---

## §1. Literature: modelling OS and hardware noise, and what it omits

**Petrini, Kerbyson & Pakin, "The Case of the Missing Supercomputer
Performance", SC'03** (DOI `10.1145/1048935.1050204`). The empirical origin:
ASCI Q ran at a fraction of predicted performance, and the cause was OS noise
— per-node interruptions of <1% each, amplified by collective synchronisation.
*The lesson for us is the amplification mechanism, not the noise:* small local
delays became large global ones only because of a synchronising structure.
scxsim has an analogous structure wherever tasks synchronise; it has none for
independent spinners, which is another reason the per-cgroup spread is the
wrong target.

**Ferreira, Bridges & Brightwell, "Characterizing application sensitivity to OS
interference using kernel-level noise injection", SC'08** (DOI
`10.1109/sc.2008.5219920`). Injects *controlled* noise at kernel level to
measure sensitivity. The method is the contribution: rather than reproduce a
system's noise, make noise a knob and measure the response curve. **We should
copy this before we copy any distribution** — knowing how sensitive a metric is
to noise tells you whether modelling it matters at all. §0 suggests per-cgroup
CPU time has a response curve near zero.

**Hoefler, Schneider & Lumsdaine, "Characterizing the Influence of System Noise
on Large-Scale Applications by Simulation", SC'10** (DOI `10.1109/sc.2010.12`).
The closest prior art to what we are contemplating, and the most instructive.
They **inject noise delays from traces gathered on real machines** into a
LogGPS simulation — measured noise replayed, not synthesised. Findings that
transfer directly:

- *"the scale at which noise becomes a bottleneck is system-specific and
  depends on the **structure of the noise**"* — not its mean, and not its
  variance alone. A distribution fitted to a mean will mispredict.
- Locally *random* noise produces **deterministic** slowdown at scale:
  *"outliers at small process counts quickly become the median at large process
  counts."*
- Different real systems have qualitatively different signatures — one showed
  *"low regular noise but reproducible longer interruptions."* Two systems with
  the same mean noise are not interchangeable.

**Their omission, and how they justify it, is the part worth stealing.** They
quantify their own measurement floor: benchmark loop overhead between **3.74 ns
and 32.9 ns** depending on system, and therefore *"we cannot reliably measure
noise frequencies higher than ~134 MHz on our most accurate system."* They then
state the justification explicitly — *"we assume that this limit is only of
theoretical interest because most noise has a much lower frequency."* That is
the template: **name the floor, quantify it, argue why what is below it does
not matter.** Our equivalent floor is the 10 ns RBC quantum and the 10 ns
TRIVIAL kfunc tier, and we have never written down what they exclude.

**gem5 SE vs FS mode** (gem5: Binkert et al. 2011). The canonical deliberate
omission in architectural simulation: syscall-emulation mode runs the
application and *emulates syscalls on the host*, omitting the OS entirely;
full-system mode boots a real kernel. The justification is explicitly
scope-based — if you are studying microarchitecture, OS time is noise; if you
are studying OS behaviour, SE mode is invalid. **scxsim sits on the FS side by
necessity** (the scheduler *is* the object of study), which means the omissions
we can justify are narrower than an architectural simulator's. *(Characterised
from the SE/FS design; I could not fetch a quotable primary statement — treat
the characterisation as mine.)*

**Sniper (DOI `10.1145/2063384.2063454`), interval simulation (Genbrugge,
Eyerman & Eeckhout, HPCA'10, DOI `10.1109/hpca.2010.5416636`), ZSim (Sanchez &
Kozyrakis, ISCA'13, DOI `10.1145/2485922.2485963`).** All three make the same
trade we are contemplating: replace cycle-accurate simulation of a subsystem
with an *analytical or statistical model* of its cost, validated against
hardware. They omit microarchitectural detail deliberately and justify it by
demonstrating bounded error against real machines. **The validation is the
price of the omission** — the omission is only defensible because the error is
measured. Ours currently is not.

## §2. Statistical cost models from profiling

The pattern — profile once, evaluate a cheap model at simulation time — is
mature, and interval simulation is its clearest expression: rather than
simulate the pipeline, model execution as intervals between miss events whose
costs come from a profile.

For our two cases:

**BPF helper / kfunc cost.** Currently four hand-chosen constants. The
replacement is a measurement: run each kfunc under the real kernel, record a
distribution per kfunc, and evaluate at simulation time. Three levels, in
increasing cost and fidelity — (a) **point estimate per kfunc** (what we have,
but measured rather than guessed); (b) **distribution per kfunc**, which
matters if any helper is heavy-tailed — map operations under contention
plausibly are; (c) **context-dependent**, cost conditioned on map size, CPU, or
contention. My recommendation is (a) then selectively (b): a distribution is
only worth its complexity where the tail is scheduler-visible. Validation is
the pointwise-per-percentile comparison the calibration crate already
implements for wake latency, which also reports both sides' coefficient of
variation and so can detect "the sim emits a constant where the live run
varies" — exactly the failure a point estimate introduces.

**Syscall cost.** Ren et al., "An analysis of performance evolution of Linux's
core operations", SOSP'19 (DOI `10.1145/3341301.3359640`) is the right primary
source, and its headline caution transfers: core operation costs changed
substantially across kernel versions, largely from security mitigations. **A
syscall cost model must be pinned to a kernel version and re-measured**, not
treated as a constant of nature. For us this is a *workload* model, not a
scheduler model, and it is only worth building if we simulate syscall-heavy
tasks. It is not on the critical path.

---

## §4. Prioritised, opinionated ranking

Ordered by evidence-per-unit-effort, not by realism.

**0. DONE — the §0 diagnostic has been run, and it refuted the defect it was
looking for.** Jitter reaches the workload; the metric is capacity-bound. See
§0. The residual value of the exercise is the reusable rule: test a candidate
calibration metric for capacity-boundedness before trusting it. No further
work here.

**1. Fix the live-side estimator to exclude steal.** Fact 2 says our reference
measurement includes hypervisor overhead. Every fidelity target computed
against it is biased by an unknown amount. This is measurement hygiene, not
modelling, and it is the highest-value item because *everything else is
calibrated against it*.

**2. Make DVFS consequential, or make it loudly unmodelled.** Today
`perf_lvl` is stored and returned with no effect. Either scale work by it or
have the setter record "requested, not modelled" so a test cannot assert on it
vacuously. Small change; removes a whole class of false-green.

**3. Replace the four kfunc constants with measured point estimates.** The RBC
mechanism already proves we can measure per-execution cost. The kfunc tiers are
the last purely-guessed numbers in the hot path, and they are dimensioned
exactly like the thing we know how to measure.

**4. Make IRQ ambient, not opt-in.** Give scenarios a default background
interrupt load, derived from a measured signature. This is the one place where
Hoefler's method applies almost unchanged: capture a real noise trace and
replay it, rather than sampling a synthetic distribution. It is also the most
likely source of *asymmetry* between two otherwise-identical cgroups, which
per §0 is the only kind of effect that could explain the VM spread.

**5. NUMA topology** — already tracked as `goal-simulated-numa-domains`, and
per the layered SEV analysis the cheapest useful slice is multi-node topology
without distance cost.

**Deliberately NOT recommended:**

- **Hypervisor/steal modelling.** Apparatus, not system. Item 1 removes it
  instead.
- **Cache/TLB/branch-misprediction models.** Enormous effort; scheduler-visible
  only through aggregates the migration penalty already approximates. The
  owner's standing constraint — the simulator owes elapsed time and scheduler
  state, not memory-system behaviour — settles this.
- **Syscall cost.** A workload model. Revisit if we simulate syscall-heavy
  tasks.
- **Tuning any noise parameter against the per-cgroup CPU-time spread.** §0.
  That statistic is structurally insensitive to symmetric noise; matching it by
  turning up variance would require absurd per-slice CV and would corrupt every
  latency distribution to fix one number.

**The general principle**, which is the same one the UB audit arrived at from a
different direction: *measured, with its floor stated, beats plausible.* Every
constant in the current model is plausible. The RBC counter is measured. The
difference is that we can say what the RBC number would have to be wrong about.
