# One scenario, two backends: `sched_basic_proportional` on a live guest and on the simulator

**For human review.** Written 2026-08-12.

This walks through a single test definition executing on two different
backends — a real VM guest and the scx-sim simulator — and being compared
under a rejection rule fixed before the run. Every command below was executed
to produce the output shown. Where output came from somewhere other than my
own run, it says so on the spot.

---

## The commit

```
sched-test  39fd51e  (integration)
            "Merge PR #85: first calibration + wprof trace comparison +
             lowering provenance"
ktstr       85c72e1  (the scenario definition side)
guest       Linux 6.14.11, source-built
```

Pinned deliberately. `integration` moved roughly twenty times on 2026-08-12;
"on integration" would not be reproducible by next week.

**Re-verified at `39fd51e` on 2026-08-13.** This document was first written
against `ba8d028`, a commit on the then-unmerged `feat/scxsim-calibration-first-run`.
PR #85 has since landed, so the calibration harness is on `integration` and the
pin is now an ordinary integration commit rather than a feature-branch tip.
Both commands below were re-run at `39fd51e` and **every number in this
document changed** — see "What re-verification changed" before citing any
figure from an older copy of this page.

What is in it, for this demo:

| Path | Role |
|---|---|
| `crates/scxsim-workload-ir/` | ktstr scenario → IR → scx-sim `Scenario` |
| `crates/scxsim-calibration/` | the comparison and its rejection rule |
| `crates/scxsim-calibration/vm_runs/sched_basic_proportional-6.14.11-85c72e1.ktstr.json` | the recorded guest run (854 lines) |

**Note this now, because it changes how you read everything below: the VM half
is a RECORDED artifact checked into the repository, not a VM booted on each
run.** The guest genuinely ran — real kernel, real `scx-ktstr` loaded — but it
ran once, was captured to JSON, and the comparison replays against that
capture. The simulator half executes live every time.

## The scenario

One definition, in ktstr, at `tests/ktstr_sched_tests.rs`, as it exists at the
pinned `85c72e1`:

```rust
#[ktstr_scenario(scheduler = KTSTR_SCHED, llcs = 1, cores = 2, threads = 1,
                 sustained_samples = 15, watchdog_timeout_s = 15)]
fn sched_basic_proportional() -> ScenarioDef {
    ScenarioDef::with_defs(vec![CgroupDef::named("cg_0"), CgroupDef::named("cg_1")])
}
```

Two cgroups, two spinning tasks, two CPUs, twelve seconds. Equal weights, so
each cgroup should get about half the machine. Deliberately boring: the point
is not the workload, it is that the *same* definition drives both backends.

**`#[ktstr_scenario]` is what makes the two-backend claim possible, and it is
worth being precise about why.** The older `#[ktstr_test]` attribute takes a
body of arbitrary Rust ending in `execute_steps(ctx, steps)`. The shape of most
such bodies is declarative, but nothing in the type system says so, and
**nothing can recover the step list without booting a guest and running the
test** — which is exactly what a second backend needs to do.

`#[ktstr_scenario]` lifts that step list into a value: a `ScenarioDef` that can
be built, inspected and printed on the host, with no VM and no `&Ctx`. That is
what the simulator ingests. The restrictions on the function follow from that
one requirement — no parameters (in particular no `&Ctx`, which is what makes
the body host-buildable), no `async`, no generics, no host-side `post_vm`
callbacks.

It is **not** a fork of `#[ktstr_test]`. The scenario attribute emits the
author's function renamed, a canonical `fn() -> ScenarioDef` builder, and a
synthesized `fn(ctx: &Ctx) -> Result<AssertResult>` that it hands to
`ktstr_test_impl` unchanged — so the VM path is literally the same code, and a
test in `ktstr_test_impl`'s own output (`delegates_to_ktstr_test_verbatim`)
pins that token for token. `ScenarioDef::run` dispatches to
`execute_steps_with`, so there is no second execution engine either.

So the two attributes are not "VM one" and "sim one". `#[ktstr_scenario]` is a
*restriction* of `#[ktstr_test]` that additionally exposes the workload as
data. A scenario written with it still runs on the VM exactly as before; a test
written with plain `#[ktstr_test]` **cannot** reach scx-sim, because its
workload only exists as executed side effects inside a running guest.

## Prerequisites that actually bite

- **`cargo-nextest`.** ktstr's own suite needs **>= 0.9.143**; 0.9.100 cannot
  parse ktstr's `.config/nextest.toml`. **Correction from my run:** the
  scx-sim side is fine on older nextest — I ran everything below on
  `cargo-nextest 0.9.100` against scx-sim's own `.config/nextest.toml` with no
  trouble. The version floor is a ktstr-side requirement, not a
  simulator-side one.
- **ktstr's CAS hard-requires `FICLONE`.** The source checkout,
  `KTSTR_CACHE_DIR` and `CARGO_TARGET_DIR` must all be on **one filesystem**.
  Three separate `EXDEV` failures came from splitting them. Check with
  `df --output=source,target <each path>` and confirm one device.

## Running it

Two commands, both from `scx-sim/`. They are feature-gated, so a plain
`cargo test --workspace` silently skips them — `validate.sh` invokes them
explicitly for that reason.

```bash
cargo nextest run -p scxsim-workload-ir  --features ingest \
      --test sched_basic_proportional --no-capture --no-fail-fast
cargo nextest run -p scxsim-calibration  --features sim \
      --test calibrate_sched_basic_proportional --no-capture
```

**`--no-fail-fast` on the first command is required at `39fd51e`, and the
reason is worth reading rather than working around.** That target also contains
`cgroup_cpuset_confinement_is_observable_or_is_not`, a characterization test
that deliberately FAILS when a known gap closes:

```
KNOWN GAP CLOSED? Tasks are now confined to their cgroup's cpuset. That is the
desired behaviour — delete this characterization test and replace it with a
real confinement assertion.
```

It is currently firing, so nextest cancels the run before this walkthrough's
own test executes unless you pass `--no-fail-fast`. That is a tripwire
announcing an improvement, not a regression — but it does mean `bash
validate.sh` at `39fd51e` ends `1270 passed, 1 failed` and aborts before its
later stages. Someone owns replacing that test; until they do, integration's
own gate is red for this one reason.

### Stage 1 — the scenario runs on the simulator

Real output from the first command:

```
Simulation complete:
  Logical time elapsed:   11.9s
  Total tasks:            2
  Max concurrent running: 2
  Total time slices:      1205
  Tasks at end:           2 alive, 2 runnable

Sched_ext structop summary:
     cpu   structops      kfuncs
  ------  ----------  ----------
       0        4830         605
       1        4811         604
  ------  ----------  ----------
   total        9641        1209
```

What you are seeing: the ktstr scenario has been lowered to simulator IR,
ingested as a `Scenario`, and executed. **Logical time**, not wall time — the
simulator advanced a modelled clock 11.9 s in a fraction of a second of real
time. Both tasks are still alive and runnable at the end, which is what
spinners on a `HoldSpec::FULL` step should be.

The structop summary shows 9,641 scheduler callbacks and 1,209 kfunc calls
crossing the boundary into scheduler code — the scheduler's own BPF logic
executing, not a model of it. Roughly even split across the two CPUs, as two
spinners on two CPUs should be.

**Do not read the callback count as a fidelity score, and note that an earlier
version of this document did exactly that.** It reported 150,223 structops and
48,071 kfuncs and called them "the fidelity evidence". Those figures were
inflated by a lowering defect, fixed in `676b42f`: `DEFAULT_SLICE = 500us` was
being applied to `SpinWait`, which is a continuous busy loop with no yield
point, so lowering inserted roughly 48,000 voluntary yields the real workload
does not have and the scheduler's 20 ms slice request never got to bind. Slices
went `48067 -> 1205`, mean slice `0.499ms -> 19.917ms` against the live guest's
`33.863ms`. A bigger callback count meant a worse lowering, not a more faithful
run — which is the opposite of how the number was being read.

Worth carrying, because it is the more general lesson: that defect passed every
check that existed. Quoting the fix, "SpinWait sat in the arm labelled `exact:
pure time, nothing dropped` and `ir.fidelity.is_exact()` returned true. Both the
calibration and the trace comparison assert on that and both passed, while the
lowering had invented the single most consequential parameter in the run."
`sched_basic_proportional` was chosen as the first calibration subject *because*
it lowered exactly.

### Stage 2 — the comparison

Real output from the second command:

```
=== sched_basic_proportional: simulator vs live guest ===
wall 12.000s  sim cpus 2  guest vcpus 2  guest kernel 6.14.11 / 85c72e1
SCHEDULERS DIFFER: guest ran `ktstr_sched`, simulator ran `simple`

calibration `sched_basic_proportional`  sched-test feat/scxsim-calibration-first-run  ktstr 85c72e1
  occupancy                  sim=0.9990         vm=1.0005         gap=+0.2%    n=1    +/-5.0%        agree
  cpu_time          [cg_0]   sim=11.990s        vm=12.011s        gap=+0.2%    n=1    +/-10.0%       agree
  off_cpu_time      [cg_0]   sim=0.0008         vm=0.0036         gap=+77.5%   n=1    +/-10.0%       DISAGREE
  cpu_time          [cg_1]   sim=11.985s        vm=12.002s        gap=+0.1%    n=1    +/-10.0%       agree
  off_cpu_time      [cg_1]   sim=0.0012         vm=0.0043         gap=+71.6%   n=1    +/-10.0%       DISAGREE
  migrations                 sim=0              vm=16             gap=+100.0%  n=1    +/-25.0% or +/-2 DISAGREE
  context_switches           sim=1205           vm=-              gap=-        n=1    +/-15.0% or +/-5 not-measured
  wake_latency      [p99]    sim=5.701us        vm=-              gap=-        n=2    +/-20.0%       not-measured
  negative control: live occupancy scaled by 3x — the pre-registered tolerance must reject this -> DISAGREE
  outcome: GAP (DISAGREE)

supporting detail not in the table:
  sim context switches 1205 (live side has no per-task counterpart)
  sim wake-latency samples n=2
  sim migrations 0 (kernel-exact) vs guest 16 (userspace-sampled)
```

Read the verdict column, not the gap column. **The overall outcome is GAP
(DISAGREE).** This run does not pass, and the section below says why that is
the honest result rather than a failure of the demo.

Note the four distinct verdicts. `not-measured` is not a quiet pass: the guest
did not record context switches or wake latency, so those rows are excluded
rather than scored. `n=1` on every row is why nothing here is a distributional
claim.

## The shared oracle, and why agreement means anything

This is the part worth understanding, because without it the whole exercise
reads as one side checking the other.

**Per-cgroup CPU time is computed independently on each side, from different
raw material, by different code.**

- On the guest: ktstr sums each worker thread's CPU time from the kernel's own
  per-task accounting, then unions per cgroup. From the recorded artifact
  (`stats.phases.per_cgroup`, read from the checked-in JSON):
  `cg_0 = 12010668525 ns`, `cg_1 = 12002274341 ns` — a spread of **0.0699%**.
- On the simulator: scx-sim sums modelled on-CPU time slices per task from its
  own trace, then unions per cgroup. Re-verified run at `39fd51e`: **11.990 s**
  and **11.985 s**, a spread of **0.042%**.

Neither side is told the other's answer. There is no shared intermediate that
both read. The estimator is *defined* identically — "sum of CPU time for tasks
in this cgroup" — but *computed* twice, from kernel accounting on one side and
from simulated dispatch on the other.

That is what makes agreement evidence. If the simulator derived its number
from the VM capture, matching would be arithmetic, not corroboration. Because
the two numbers are produced by independent mechanisms, agreement to 0.1% is a
real claim: the simulator's dispatch model puts the same amount of CPU in the
same places as a real kernel running a real scheduler did.

And the converse is what gives the failures below their force. A comparison
that cannot disagree is not a measurement. This one can, and did.

## What this does NOT show

Four things, stated here rather than left for the reader to notice.

**1. The overall verdict is GAP (DISAGREE), not agreement.** `off_cpu_time`
differs by roughly fourfold — sim `0.0008` and `0.0012` against vm `0.0036` and
`0.0043`, a 72–78% gap against a **pre-registered ±10%** tolerance. The tolerance was
fixed before the run, with a written rationale, so this is a rejection by a
rule that existed in advance, not a number judged after the fact. It is under
investigation. A demo that showed only the two `cpu_time` rows would be
precisely the kind of green worth removing.

**2. The two backends are not running the same scheduler.** The harness says
so itself: `SCHEDULERS DIFFER: guest ran ktstr_sched, simulator ran simple`.
The guest loaded `scx-ktstr`; the simulator ran its `simple` scheduler. So the
`cpu_time` agreement demonstrates that *two equal spinners get equal CPU under
both*, which is a weaker claim than *this scheduler behaves identically in
both*. Until the simulator runs `ktstr_sched` itself, that stronger claim is
not on the table.

**3. `migrations` also disagrees, and is not the same measurement on both
sides.** sim 0 versus vm 16, a 100% gap. The harness footnotes why: sim
migrations are kernel-exact, guest migrations are userspace-sampled. Two
different estimators wearing one name — arguably it should not be scored at
all until they are reconciled.

**4. n=1.** One guest recording, one simulator run. Nothing here is a
distributional result, and the harness marks `wake_latency` `not-measured`
rather than reporting a p99 from two samples.

The negative control did behave correctly: live occupancy scaled by 3× was
rejected by the same tolerances (`-> DISAGREE`). Without that, the rows that
agreed would be uncitable, because nothing would establish the bounds could
have rejected anything.

## What re-verification changed

Re-run at `39fd51e` on 2026-08-13, after PR #85 landed. **Every figure moved.**
The qualitative story did not: same verdict per row, same overall
`GAP (DISAGREE)`, negative control still rejects.

| | `ba8d028` (as first published) | `39fd51e` (re-verified) |
|---|---|---|
| time slices / `context_switches` | 48067 | **1205** |
| structops | 150223 | **9641** |
| kfuncs | 48071 | **1209** |
| `occupancy` sim | 0.9995 | **0.9990** |
| `cpu_time` sim cg_0 / cg_1 | 11.994s / 11.994s | **11.990s / 11.985s** |
| `off_cpu_time` sim cg_0 / cg_1 | 0.0005 / 0.0005 | **0.0008 / 0.0012** |
| `off_cpu_time` gap | +85.4% / +88.0% | **+77.5% / +71.6%** |
| per-cgroup sim spread | 0.0002% | **0.042%** |

The slice-derived rows moved because of the `676b42f` lowering fix described
under Stage 1: the old numbers were inflated by roughly 48,000 invented yields.
The old figures are wrong, not merely stale, and should not be cited.

Two things did **not** change and are worth noting because they are the load-
bearing claims: the recorded guest figures (`cg_0 = 12010668525 ns`,
`cg_1 = 12002274341 ns`, spread 0.0699%), which come from a checked-in artifact
and cannot move; and the per-row verdicts, which is what makes the comparison a
rejection rule rather than a description.

## What I ran versus what I did not

Being explicit, since this is the kind of document people cite.

- **I ran both commands above** at `39fd51e`, on `cargo-nextest 0.9.100`, and
  every block of output in Stages 1 and 2 is copied from that run, untrimmed.
  The first command required `--no-fail-fast` for the reason given under
  "Running it". I also ran the full `bash validate.sh` at that commit: it ends
  `1270 passed, 1 failed`, the one failure being the cpuset characterization
  tripwire, and it aborts there before its later stages.
- **I did not boot the VM.** The guest half is the recorded artifact in
  `vm_runs/`. Its per-cgroup figures above were read from that file. Nobody
  re-ran a kernel to produce this document.
- **The IR dump is not shown, because no command in this walkthrough produces
  it.** The brief for this walkthrough asked for a dump reading `1n/1l/2c/1t`
  with duration, seed and the two spinning tasks. That formatter exists —
  `pretty()` in `crates/scxsim-workload-ir/src/pretty.rs`, with exactly that
  format string. It is **exercised by its own three unit tests**
  (`pretty_renders_structure_and_fidelity_together`,
  `pretty_states_exact_fidelity_explicitly`,
  `pretty_renders_wake_edges_and_timeline`, all passing), so it is neither dead
  nor unverified — but **no binary and no integration test calls it**, so
  neither of the two commands above emits a dump. Rather than reconstruct
  plausible output, it is omitted. Wiring `pretty()` into the ingest test would
  make the IR visible and is worth doing; the topology it would print
  (1 node / 1 LLC / 2 cores / 1 thread) is visible in the `#[ktstr_scenario]`
  attribute regardless.

## Reproducing

```bash
git clone https://github.com/rrnewton/sched-test.git
cd sched-test && git checkout 39fd51e
git submodule update --init --recursive                     # REQUIRED: see below
cd scx-sim
# confirm one filesystem across checkout, KTSTR_CACHE_DIR, CARGO_TARGET_DIR
cargo nextest run -p scxsim-workload-ir --features ingest \
      --test sched_basic_proportional --no-capture --no-fail-fast
cargo nextest run -p scxsim-calibration --features sim \
      --test calibrate_sched_basic_proportional --no-capture
```

The submodule step is not optional and is not a formality: the simulator's C
substrate includes `<scx/common.bpf.h>` from the `scx` submodule, so without it
**both commands fail at build**, not at run, with

```
fatal error: 'scx/common.bpf.h' file not found
error occurred in cc-rs: command did not execute successfully
```

`39fd51e` is an ordinary commit on `integration` — the merge of PR #85 — so it
stays fetchable from the mirror as an ancestor of `refs/heads/integration`, and
no tag is needed to reach it. Use the SHA rather than `integration`, which moves
constantly.

The older annotated tag `demo/both-backends-20260812` pins the superseded
`ba8d028` and is **not** what this document now describes. It existed because
`ba8d028` sat on the then-unmerged `feat/scxsim-calibration-first-run` and the
mirror carries no `refs/pull/*` refs; PR #85 landing removed that need. The tag
is left in place rather than moved, since retargeting an annotated tag that has
been published to both remotes would silently change what an existing citation
resolves to.

The calibration test is a ratchet: `the_findings_as_first_measured` pins the
current per-metric verdicts, so a change in simulator fidelity fails there
rather than drifting quietly. That includes the two `DISAGREE` rows — closing
the `off_cpu_time` gap will require updating the pin, which is the intended
way to notice that it moved.
