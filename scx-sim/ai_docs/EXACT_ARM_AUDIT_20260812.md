# Audit: every lowering arm that reports fidelity EXACT

**Date:** 2026-08-12
**Scope:** all 42 supported arms of `plan_work` in `scxsim-workload-ir/src/lower.rs`
**Trigger:** `SpinWait` fabricated the scheduling quantum while reporting exact,
producing a 68x slice-count divergence that three tests asserted through.

The audit instrument is **not** the fidelity report — it was green throughout
the bug. Each arm was read directly and its output compared field-by-field
against its input, then confirmed empirically by lowering it and printing what
came out.

## Result

**42 arms. 8 claimed exact. 3 were fabricating, 1 was silently dropping, 1 was
under-disclosing, 2 are clean, 1 is unreachable.**

| arm | claimed | actually | verdict |
|---|---|---|---|
| `SpinWait` | exact | invented a 500 us quantum | **fixed earlier** — now a continuous run phase |
| `YieldHeavy` | exact | invents the work between yields | **FABRICATION — fixed** |
| `Mixed` | exact | invents it twice | **FABRICATION — fixed** |
| `Sequence` + `Yield(d)` | exact | drops the declared duration | **SILENT DROP — fixed** |
| `Bursty` | exact | carries both declared durations | **clean** |
| `RtStarvation` | non-exact | `sleep: ZERO` not in source | defensible, documented |
| `PreemptStorm` | non-exact | invents `priority: 50`, undisclosed | **UNDER-DISCLOSURE — fixed** |
| `Custom`/`Schbench`/`Taobench` | — | `unreachable!`, refused earlier | verified refused |

The other 34 arms all record an approximation and do not claim exact.

## The three defects

### 1. `YieldHeavy` and `Mixed` — fabrication, same shape as `SpinWait`

Both lower to `Phase::Run(DEFAULT_SLICE)` around a `Yield`. ktstr declares a
yielding *pattern*; it does not declare how much work sits between yields. The
500 us is the lowering's, and both arms reported `is_exact() == true`.

Empirically, before:

```text
=== YieldHeavy  exact=true
    phases [Run(500000), Yield]
=== Mixed       exact=true
    phases [Run(500000), Yield, Run(500000)]
```

### 2. `SourceWorkPhase::Yield(DurationNs)` — a silently dropped value

`lower_phase` matched `Yield(_) => Phase::Yield`. The declared duration was
discarded with no record, because `Phase::Yield` carries none. Any `Sequence`
containing a `Yield` therefore reported exact with the duration gone:

```text
=== Seq+Yield   exact=true
    input:  Spin(4ms), Yield(9ms)
    output: [Run(4000000), Yield]      <- the 9ms is simply not there
```

This is the **opposite direction** from fabrication — a number thrown away
rather than made up — which is why the fix introduces two separate `Cause`
variants rather than one. Blurring them is how one hides behind the other.

### 3. `PreemptStorm` — under-disclosure, and the subtlest of the three

`PreemptStorm { cfs_workers, rt_burst_iters, rt_sleep_us }` has **no priority
field**, and the arm emits `SchedPolicy::Fifo { priority: 50 }`. 50 is invented.

It did not show up as an exactness bug because the arm was *already* non-exact
from its `iters()` conversion. **That is the lesson: `is_exact() == false` does
not mean "everything is disclosed".** The report listed the iterations
conversion and said nothing about the fabricated priority. A reader checking
the flag, or even skimming the approximation list, would not learn that the RT
priority was invented.

## The fixes

**`Ctx::invented_slice(source, what)`** replaces bare `DEFAULT_SLICE` use in the
arms that were silent. It returns the constant *and* records the approximation,
so the honest path is the only path. `SpinWait`'s fix already removed the worst
case; this generalises it.

**Two new `Cause` variants**, deliberately distinct:

- `UnspecifiedWorkQuantum` — the source did not say, and the lowering supplied
  a number. Now also used for `PreemptStorm`'s priority.
- `UnrepresentableWorkQuantum` — the source said, and the IR cannot carry it.
  Used for the dropped `Yield` duration.

After:

```text
=== YieldHeavy  exact=false   approx: "...the source declares the behaviour but
                                       not how much work per phase; 500.000us
                                       supplied by the lowering"
=== Mixed       exact=false   (same)
=== Seq+Yield   exact=false   approx: "WorkPhase::Yield(9.000ms) — Phase::Yield
                                       carries no duration"
=== Bursty      exact=true    phases [Run(3000000), Sleep(7000000)]   <- unchanged
=== PreemptStorm              approx: "PreemptStorm does not specify an RT
                                       priority; 50 supplied by the lowering"
```

## A fourth test was encoding the bug

`pure_timing_work_types_lower_exactly` asserted that `SpinWait`, `YieldHeavy`,
`Mixed` and `Bursty` all lower exactly. It is the **third** test to have
asserted on the false exactness, after the calibration test and the trace
comparison — and unlike those two it named the arms explicitly, so it was the
one place a reader might have caught it.

It now covers only the two arms that genuinely carry declared values,
`SpinWait` and `Bursty`, with the reason written into its doc comment. Removing
the other two is the fix, not a relaxation.

## Guards, all checking values rather than the flag

- **`no_arm_reports_exact_while_using_the_invented_default_slice`** — the
  systemic tripwire. Loops over all 42 supported arms; for any that reports
  exact, asserts no `Phase::Run` equals `DEFAULT_SLICE`. `DEFAULT_SLICE` is by
  definition not in the input, so its presence in an exact arm is proof of
  fabrication. **This one assertion catches all three known instances and any
  new arm that repeats them.**
- `bursty_is_exact_and_carries_both_declared_durations` — the control. Without
  it, the tripwire could be satisfied by arms that stopped claiming exact for
  the wrong reason.
- `yieldheavy_and_mixed_disclose_the_invented_quantum` — requires the specific
  `Cause`, not merely non-exactness.
- `a_dropped_yield_duration_is_recorded_not_silent` — requires the record to
  name the lost value (`9.000ms`).
- `preemptstorm_discloses_its_fabricated_rt_priority`.

## The second failure shape: scope narrowness, confirmed

The brief asked for the other class too — a value the lowering carries
correctly that something downstream ignores. **Found, and confirmed with a
concrete violation.**

`to_scenario` resolves a cgroup's cpuset and passes it to `CgroupDef`. But a
task's `allowed_cpus` is populated **only from its own affinity**, never from
its cgroup, and the engine builds each task's cpumask solely from
`allowed_cpus`. So a cgroup cpuset reaches the `Scenario` and confines nothing.

Measured on two cgroups with disjoint halves of a 4-CPU box:

```text
task Pid(1) cgroup cg_0 allowed_cpus None
task Pid(2) cgroup cg_1 allowed_cpus None
CONFIRMED: cgroup cpusets do not confine tasks.
           pid 2 allowed {2,3} ran on [0]
```

`cg_1` declared CPUs 2–3 and its task ran on CPU 0.

Note the trap in testing this: two tasks on four CPUs land on different CPUs by
luck often enough that "are the two tasks disjoint from each other" passes while
confinement is entirely absent. The check compares each task against **its own
cgroup's declared set**.

Captured as `cgroup_cpuset_confinement_is_observable_or_is_not`, a
characterization test that fails loudly *when the gap is closed*, telling
whoever fixes it to replace it with a real confinement assertion.

Under "Don't Model the Scheduler — Model the Kernel", cpuset enforcement is the
kernel's job, so this is an engine-level gap rather than a lowering one. Filed,
not fixed here — it is a different subsystem and this change is already large.

## What this does to earlier results

Nothing already reported changes. `sched_basic_proportional` uses only
`SpinWait`, whose fix landed with the 68x investigation and whose corrected
numbers are already published. No calibration or trace comparison has been run
against `YieldHeavy`, `Mixed`, a `Sequence` with a `Yield`, or `PreemptStorm`.

The exposure was forward-looking: `sched_cpuset_split` is one of the five ported
scenarios and was next in line, and it is exactly the scenario the cpuset gap
would have silently invalidated.

## Follow-ups filed, not done

1. **Cgroup cpusets should confine member tasks** (engine + ingestion).
2. **Arms that record an unrelated approximation still do not name the invented
   quantum.** ~11 arms use `DEFAULT_SLICE` while recording only, say,
   `Microarchitectural`. Their reports are non-green but incomplete — the
   `PreemptStorm` shape. `Ctx::invented_slice` exists, so the fix is mechanical;
   kept out of this change to keep it reviewable.
3. **`RtStarvation`'s `sleep: ZERO`.** Not in the source. Defensible — RT
   workers that never sleep are the definition of the starvation scenario — and
   it cannot currently reach the simulator anyway, since a `Fifo` policy is
   refused at ingestion with `PolicyNotRepresentable`. Documented rather than
   recorded, to avoid making the report noisier than it is useful.
