# First wprof trace comparison: live guest vs simulator

**Date:** 2026-08-12
**Scenario:** `sched_basic_proportional` — 2 spinners, 2 cgroups, 2 CPUs, 12 s
**Live:** ktstr guest, source-built 6.14.11, `scx-ktstr`, wprof Perfetto `.pb`
**Sim:** `scx-sim` `simple`, same scenario via `ktstr ops -> IR -> Scenario`

This is exploratory: data to decide from, not a verdict. Tolerances and the
four-outcome verdict live in `scxsim-calibration`; nothing here is scored.

## Reproduce

```sh
# live half (needs the wprof cargo feature; see the ktstr-side commit)
cargo ktstr test --features wprof --test ktstr_sched_tests sched_basic_proportional_wprof
# simulated half
cargo run -p scxsim-calibration --features sim --example dump_sim_perfetto -- /tmp/sim.pb
# compare
scripts/wprof_trace_compare.py --compare <guest>.wprof.pb /tmp/sim.pb \
    --cpus 2 --workload-names init --sim-workload-names 'spin#0'
```

## Headline

```text
  quantity                              live             sim   note
  trace span                         14.997s         12.000s   live spans boot+hold+teardown
  workload on-CPU                  24042.9ms       23988.0ms   COMPARABLE — differ 0.23%
  workload slices                        710           48067   COMPARABLE — sim has 68x more
  mean slice                        33.863ms         0.499ms   COMPARABLE — the headline divergence

  what the workload filter REMOVED from the live side
  (the simulator has no counterpart for any of it: NotMeasured, not zero)
    kthread                          4.080ms   0.0136% of capacity
    irq                             22.002ms   0.0734% of capacity
    workqueue                       21.584ms   0.0720% of capacity
    harness                          7.772ms   0.0259% of capacity
    tracer                           3.321ms   0.0111%  (observer effect, excluded)
    TOTAL unmodelled                55.439ms   0.1848% of capacity
    idle                            5437.7ms   live idles during boot/teardown; sim never idles
```

## The accounting is cross-validated

Workload on-CPU from the **trace** is 24042.9 ms. Total CPU time from the
**stats sidecar** of the same run — a completely independent path, the worker
threads' own `CLOCK_THREAD_CPUTIME` rather than wprof's sched tracing — is
24012.9 ms. **0.12% apart.** Two unrelated measurements of the same quantity
agreeing to that precision is what licenses believing the rest of the table.

The simulator's 23988.0 ms sits 0.23% from the live trace figure.

## Finding 1 — the owner's kthread hypothesis, tested and answered

**Kernel threads are NOT the significant unmodelled overhead.** They consume
**4.080 ms** of a 29.99 s CPU capacity — **0.0136%**. IRQ (22.0 ms) and
workqueue (21.6 ms) each consume roughly **five times more** than kthreads do.

Total unmodelled kernel work is **55.4 ms, 0.185% of capacity**. Real,
measurable, and small. Per-entity, the whole population is:

| entity | class | self ms | n |
|---|---|---|---|
| `WQ:events` | workqueue | 21.197 | 48 |
| `SOFTIRQ:timer` | irq | 18.546 | 1257 |
| `scheduler` | harness | 5.667 | 144 |
| `wprof_rb000` | tracer | 3.223 | 144 |
| `rcu_preempt` | kthread | 2.819 | 310 |
| `SOFTIRQ:rcu` | irq | 2.493 | 735 |
| `trace-pipe` | harness | 1.658 | 72 |
| `HARDIRQ` | irq | 0.963 | 11 |
| `kworker/1:1` | kthread | 0.509 | 42 |
| `kcompactd0` | kthread | 0.283 | 29 |
| `kworker/0:1` | kthread | 0.219 | 27 |
| remainder (`rcu_tasks_trace`, `WQ:*`, `hvc0-poll`, `ksoftirqd/*`) | mixed | <0.5 | — |

This also **corroborates the off-CPU finding from the calibration** from a
second direction. That run measured 0.358%/0.433% off-CPU per cgroup — about
95 ms of worker time unaccounted for across both. Traced kernel work explains
55.4 ms of it, ~58%; the balance is run delay and scheduling gaps that are not
attributable to any traced slice. It is emphatically **not** hypervisor steal:
there is no steal field in the sidecar at all, and `host_dilation` (1.000632,
0.063%) is ~6x too small.

## Finding 2 — the real divergence is slice granularity, 68x

The live scheduler gave each spinner **33.863 ms per slice**; the simulator
gave **0.499 ms**. Same total CPU time, 68x the number of context switches.

**Part of this is ours, not the simulator's.** `DEFAULT_SLICE` in the IR
lowering is 500 us, and 12 s / 500 us x 2 tasks = 48000, which is essentially
exactly the 48067 observed. The lowering picked that number; ktstr never
specified one. So this figure is at least as much an artifact of a lowering
default as it is simulator infidelity, and it should not be reported as the
latter. The honest next step is to make the slice a property carried by the
IR rather than a constant chosen inside it.

**This metric was NotMeasured in the stats-based calibration** — ktstr's
sidecar has no per-task context-switch counter, only a VM-wide schedstat
total. The trace channel makes it comparable. That is the concrete argument
for trace comparison earning its keep: it converted a NotMeasured into a
measured 68x.

## Two traps caught before they produced numbers

**1. The default capture misses the workload entirely.** `WprofConfig::default_args`
is `-d 500` — 500 ms — and guest init spawns the tracer at boot. The first
capture was 0.495 s of *boot*: `init`, `swapper`, `rcu_preempt`, `kworker`,
`wprof-capture`, and **not one workload task**. Diffing that against a 12 s
simulation would have run cleanly and reported that the live side has HARDIRQ,
SOFTIRQ, workqueues and kthreads the simulator lacks, and 539 events against
486685. Every one of those statements is true and the conclusion would have
been garbage. Fixed with `wprof_args = "-d 15000 ..."`.

**2. `--kthread --idle` are not additive — they delete the workload.** Their
help text ("Allow kernel tasks", "Allow idle tasks") invites the assumption
that they widen the trace. Measured: adding them dropped **24.0 s of userspace
on-CPU time**, took the trace from 587 KB to 189 KB and 10777 to 3034 packets,
and moved kthread time *down* (4.080 -> 2.745 ms). The configuration without
them is the one that contains the workload. Recorded at the call site in the
ktstr test so nobody repeats it.

A third, in the tooling rather than the data: **Perfetto slices nest.** wprof
puts an IRQ slice inside the slice of the task it interrupted. A first version
of the analyzer kept one open slice per track instead of a stack, silently lost
731 of 3819 ends, and under-reported workload CPU time by 13x — which looked
exactly like a spectacular fidelity gap. Self-time accounting (slice minus
children) is what makes the class totals sum correctly.

## What was comparable, and what was not

**Comparable: 3 quantities** — workload on-CPU time, workload slice count,
mean slice length.

**NotMeasured: everything else**, and not because the live side lacks it —
because the *simulator* does. The simulator emits no kthread, IRQ, workqueue,
idle or harness events at all, so there is nothing to compare 55.4 ms against.
Recording it as `n/a` rather than 0 is the same rule the calibration applies:
a quantity one side does not emit cannot be scored as agreement.

Notably the asymmetry is the reverse of the stats channel. There the live side
was missing things (wake latency unmeasured, no per-task context switches).
Here the live side is richer and the simulator is the sparse one.

## Syscall time inside slices — not visible in this capture

Asked directly. Answer: **no, and for a concrete reason.** The capture ran with
`-e sched`, scheduling events only. Max slice nesting in the live trace is 2,
and the only nesting present is IRQ-inside-task. There are no syscall slices to
find.

wprof does expose more: `-e sched-extras,numa,tidpid,timer-ticks` and, more
interestingly, `-f scx` — a sched_ext capture feature that would emit events
directly comparable to the simulator's own `SCX_DSQ` vocabulary. Neither has
been tried. `-f scx` is the highest-value next capture.

## The gap list for THIS channel

Different from, and smaller than, the ~12-of-75 structops JSONL gap — that
figure is the bpftrace/JSONL channel and does not carry over. For Perfetto:

- **Simulator emits, wprof does not:** the dispatch-path ops (`EnqueueTask`,
  `SelectTaskRq`, `Balance`, `PickTask`, `SetNextTask`, `PutPrevTask`),
  `DsqInsertVtime`'s vtime, `DsqMoveToLocal`, `DispatchRejected`, and the
  entire `SCXSIM_CGROUP_BW` causal channel (no public tracepoint exists).
- **wprof emits, simulator does not:** softirq subtypes, IPI receive side,
  waker-chain depth, process lifecycle (`EXEC`/`EXIT`/`FREE`/`FORKING`), PMU
  deltas — plus, per the measurement above, every kthread, workqueue and idle
  slice.
- **Shape mismatches:** idle as `swapper/N` vs a `CpuIdle` instant; one ONCPU
  slice vs the simulator's three-event `PickTask`/`SetNextTask`/`TaskScheduled`.

Source: `experiments/wprof_trace_baseline_20260513/REPORT.md` §5–6, confirmed
empirically here.

## Caveat that bounds all of it

**The backends ran different schedulers** — guest `scx-ktstr`, simulator
`simple`, because scx-sim has no scx-ktstr. The 68x slice divergence in
particular is a statement about `simple` plus a lowering default, not about
"the simulator". Until the schedulers match, this is a scenario-level
comparison.

Also relevant: scxsim's observed spread is reportedly ~450x tighter than its
own configured noise model predicts (`run_jitter` at 20% CV apparently not
reaching the workload; `docsreview` investigating). Any low variance on the sim
side here is that known defect, not a finding of this work.

## Where the tooling should live

`scripts/wprof_trace_compare.py` (mypy `--strict` clean, run by `validate.sh`)
does the accounting. It deliberately emits no verdict.

**repromagic (`repm`) was checked first, as asked.** It has real comparison
machinery — `compare.rs` (687 lines), `score.rs` (748), `trace.rs` (925) — but
it does not fit this job: it **cannot read Perfetto protobuf at all** (only
`parse_perfetto_json`, Chrome JSON; the sole `.pb` mention in the crate is a
gitignore line), it compares **metrics CSVs** rather than event streams, and
its trace ingestion feeds **workload synthesis** — trace-in/workload-out, the
opposite direction from trace-in/comparison-out.

They should **not** be merged: repm answers "did my synthesised reproducer
match production", calibration answers "does the simulator match a live guest".
The one thing that should converge is the **verdict layer**. repm scores with
geomean-of-ratios and a fixed >2x divergence flag — no pre-registered
tolerance, no rationale, no `NotMeasured`, no sample floor, no negative
control — so a repm comparison cannot distinguish "agrees" from "the bound was
too loose to reject anything". `scxsim-calibration`'s
`Metric`/`Tolerance`/`Verdict`/`MinSamples`/`NegativeControl` already has that
and carries no simulator dependency by default, so repm could consume it today.

## Next, in priority order

1. **Capture with `-f scx`.** Directly comparable to the simulator's own
   sched_ext vocabulary, and the largest single increase in comparable surface
   available.
2. **Make the slice length IR-carried rather than a lowering constant.** Until
   then the 68x cannot be attributed.
3. **Make the schedulers match** (`scx-sim/schedulers/ktstr/wrapper.c`, or
   point the test at a scheduler both have). Still the top blocker for
   attributing anything.
4. **Feed these quantities into `scxsim-calibration`** as metrics with
   pre-registered tolerances, so the trace channel gets the same rejection rule
   as the stats channel.
