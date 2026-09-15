# rt-app++: the workload language, and what scx_layered can match on

**Status:** current as of the change that added these keys. Cite symbols, not
line numbers — see the convention note at the end.

`scx_layered` classifies tasks almost entirely by attributes of their
*identity*: the thread name, the process name, the cgroup path, the
credentials, the parent. A simulator that cannot control those strings cannot
run a real layer configuration, however faithfully the BPF itself executes.

scx-sim's workload front end is rt-app JSON (`safe::rtapp::load_rtapp`). Stock
rt-app was designed to describe *timing*, not *identity*, so several of the
attributes layered matches on have no rt-app spelling at all. **rt-app++** is
that language plus the identity keys, added one at a time and only where the
simulator can honestly drive the attribute end to end.

---

## The extension keys

All are **task-level**. Putting one inside a `phases` object is an error
(`RtAppError::TaskLevelKeyInPhase`) rather than a silent no-op: identity belongs
to a task for its whole life, and a spec author who writes one in a phase
believes something is happening that is not.

| key | value | reaches | layered match kind |
|---|---|---|---|
| `thread_of` | task name | `task_struct->group_leader` | `PcommPrefix` |
| `parent` | task name | `task_struct->real_parent` | `PpidEquals` |
| `uid` | integer | `real_cred->{uid,euid}` | `UserIdEquals` |
| `gid` | integer | `real_cred->{gid,egid}` | `GroupIdEquals` |
| `kthread` | boolean | `task_struct->flags \|= PF_KTHREAD` | `IsKthread` |

The single list they are all registered in is `rtapp::RTAPP_PP_KEYS`. Three
places must agree about it — the parser (which must not classify them as
events), the phase-level rejection, and the exporter — so it is one constant
rather than three lists.

### `thread_of` — thread groups

```json
{
  "tasks": {
    "frontendd":    { "run": 4000, "sleep": 4000, "loop": -1 },
    "iothreadpool": { "instance": 8, "run": 1000, "sleep": 9000, "loop": -1,
                      "thread_of": "frontendd" }
  }
}
```

Every instance of `iothreadpool` becomes a **thread** of the process led by the
*first instance* of `frontendd`. Their own `comm` stays `iothreadpool-<i>`;
their `pcomm` is `frontendd`. That distinction is the whole point — production
configurations use `pcomm` precisely to catch a worker pool by the service it
belongs to rather than by the pool's own name.

**Naming your own task** is the compact spelling of "my instances are one
process":

```json
"workerpool": { "instance": 8, "thread_of": "workerpool", "run": 1000 }
```

This is not a special case. The rule is always "the process led by the first
instance of the named task"; here the named task is this one, so instance 0 is
the leader and instances 1..7 are its threads.

Two things come with being a thread rather than a process, and both are
applied, because a "thread" that has neither is not a state any real thread is
in:

- **the address space.** Threads of one process share an `MmId`, so
  wake-affine scheduling sees them as related.
- **the cgroup.** Under cgroup v2's default *domain* mode every thread of a
  process is in the process's cgroup. A thread that declares no `taskgroup`
  inherits its leader's. A thread that declares a **different** one is asking
  for *threaded* mode, which scx-sim does not model, and is refused by name
  (`RtAppError::ThreadInForeignCgroup`) rather than run as if it were the
  ordinary case.

**Chains are refused.** `a -> b -> c` is `RtAppError::NestedThreadGroup`. The
kernel has no nested thread groups; `group_leader` always points at a leader.
Flattening a chain silently would put a plausible but wrong string in front of
`MATCH_PCOMM_PREFIX`.

### `parent`, `uid`, `gid`, `kthread`

```json
"child":   { "run": 1000, "parent": "supervisor" },
"svc":     { "run": 1000, "uid": 4711, "gid": 1000 },
"kworker": { "run": 1000, "kthread": true }
```

A reference (`thread_of`, `parent`) that names a task which does not exist is
`RtAppError::UnresolvedTaskRef`, for the same reason a bad `resume` target is
an error: a typo would otherwise leave the relation unset, and the whole point
of the relation is a match kind that then quietly stops firing.

`irq_gen*` entries are a subtle case of the same thing. They occupy pid slots
and then become `IrqEvent`s rather than tasks, so a reference to one resolves
and names a pid the engine will never build. That is caught at parse time, not
as a panic inside `Engine::new`.

The same hole existed for `resume`, and is now closed the same way.
`validate_wake_targets` rejects a `Phase::Wake` naming a pid no task carries
(`RtAppError::UnresolvedWakeTarget`), which is what a *pid-accounting* slip
produces rather than a typo. There was one: `load_rtapp` makes two passes over
`tasks`, and for an `irq_gen*` entry with `instance > 1` the first consumed a
pid per instance while the second consumed exactly one. Measured — with
`"instance": 3`, a later `"resume": "later"` lowered to `Wake(Pid(4))` while the
task named `later` had been given `Pid(2)`, a wake delivered to nothing. Fixed
at source; the guard is the belt to that brace.

### The native builder

Everything above is reachable from `Scenario::builder()` too. `TaskDef` carries
`thread_group_leader`, `uid`, `gid`, `parent_pid` and `task_flags` directly, and
`ScenarioBuilder::add_thread_of(name, nice, behavior, leader)` is the ergonomic
form — it applies the same mm and cgroup sharing the rt-app path does.

---

## What stock rt-app actually does, measured

Three facts about the real binary, because the rest of this document only makes
sense against them. Measured on `~/bin/rt-app` built from
`checkouts/rt-app@9eedd75`, by running a two-task / three-thread spec and
reading `/proc/<pid>/task/*/comm` and `/proc/<pid>/task/*/status`:

```
TID      COMM              Tgid
401867   rt-app            401867      <- the main thread
401869   frontendd-0       401867
401870   iothreadpool-1    401867
401871   iothreadpool-2    401867
```

**1. Stock rt-app has exactly one thread group.** Every task is a
`pthread_create`d thread of one process (`src/rt-app.c`), named with
`pthread_setname_np`. The group leader is the rt-app main thread and its comm
is `rt-app`. rt-app-rs does the same with `thread::Builder::name`.

So under stock rt-app, **`pcomm` is `rt-app` for every task in every spec ever
written**, and no production `pcomm` rule can fire on a real rt-app run. That
is why `thread_of` had to be a language *extension* and not a mapping: there is
nothing in rt-app to map a real thread group from.

**2. The instance index is global, and single-instance tasks get one too.**
`thread_data_set_unique_name()` is `snprintf("%s-%d", name, tdata->ind)` with
`tdata->ind` assigned from a global thread counter in
`rt-app_parse_config.c`. Hence `frontendd-0` for a task declared
`"instance": 1`, and `iothreadpool-1` for the *first* instance of the second
task.

This parser instead uses the bare name at `instance == 1` and a per-task index
otherwise. Prefix rules are unaffected; an exact rule, or a prefix reaching past
the base name, diverges. Pinned by
`rtapp::tests::known_gap_instance_naming_diverges_from_stock_rtapp`.

**3. `iorun` is not what its name suggests.** Kept here as a pointer because it
is the same class of trap: see
`ai_docs/RTAPP_STRESSNG_IO_EXPRESSIVENESS_20260814.md` in the parent harness.
Treat every apparent rt-app capability with that suspicion.

---

## Export refuses; it does not drop

`scenario_to_rtapp_json` (`bin/scxsim/real_run.rs`) is the reverse direction: a
`Scenario` emitted as an rt-app spec that can run on real hardware. Every
rt-app++ attribute is **refused** there, by name, with the reason:

```
task "iothreadpool-0" carries a thread-group membership (thread_of), which
stock rt-app has no syntax for: rt-app runs every task as a pthread of one
process whose comm is `rt-app`, so the exported spec would give every task the
same pcomm rather than this one. Refusing to emit a spec that would run as a
different workload; ...
```

This follows the precedent already in that file, which refuses to relabel a
calibrated `Phase::SystemCpu` as user `run`. The alternative — emitting the
spec without the attribute — hands someone a file that runs a measurably
different workload under the same name.

The refusal is not a temporary limitation of the exporter: **no backend can run
a thread-group spec on real hardware today**, because both rt-app and rt-app-rs
put every task in one process. Making one that can means teaching rt-app-rs —
which we own — to launch a spec as multiple processes. Tracked as `sim-cl2fb`;
until then, any comparison of a thread-group workload between scx-sim and
hardware is unavailable, and a report claiming one is wrong.

The exporter also **now emits `taskgroup` and `delay`**, which it previously
dropped. The cgroup drop was the serious one: a scenario built from a
`taskgroup` spec exported to a spec with no cgroups at all, so every cgroup rule
that fired in simulation missed on the real run — silently, because rt-app is
perfectly happy to run tasks in the root cgroup. (rt-app's `add_cgroups()`
calls `cgroup_mkdir()` on every taskgroup in the spec, so the emitted spec is
self-sufficient — it creates its own cgroups rather than requiring them to
pre-exist.)

Two asymmetries to know about, so neither reads as an oversight:

- **The ingest does not read `delay`.** It is in `TASK_PHASE_KEYS`, so the
  parser skips it as a non-event and every task gets `start_time_ns == 0`.
  Exporting it is still right — a scenario with staggered starts now staggers
  on hardware — but the round trip loses the stagger in the other direction.
  `sim-x7evq`; not fixed here because parsing it would change the start time of
  every existing spec that carries one.
- **`mm_id` is still dropped, deliberately.** rt-app puts every task in one
  process, so an explicit address-space grouping is not reproducible as a
  *distinction*; refusing it would reject every COSMOS wake-affine scenario for
  a property rt-app satisfies by construction.

---

## The match-kind ledger

What a layer configuration can match on, and whether the workload language can
drive it. "substrate" is whether the simulated `task_struct` carries the field
at all; "language" is whether a spec can set it.

| match kind | reads | substrate | language | state |
|---|---|---|---|---|
| `CommPrefix` | `p->comm` | yes | task key | works |
| `PcommPrefix` | `p->group_leader->comm` | yes | `thread_of` | **works (new)** |
| `CgroupPrefix` / `Suffix` / `Contains` | `format_cgrp_path()` | yes | `taskgroup` | works |
| `NiceAbove` / `Below` / `Equals` | `p->static_prio` | yes | `priority` | works |
| `UserIdEquals` | `real_cred->euid.val` | yes | `uid` | **works (new)** |
| `GroupIdEquals` | `real_cred->egid.val` | yes | `gid` | **works (new)** |
| `PpidEquals` | `p->real_parent->pid` | yes | `parent` | **works (new)**, with a caveat |
| `IsKthread` | `p->flags & PF_KTHREAD` | yes | `kthread` | **works (new)**, with an upstream caveat |
| `PidEquals` | `p->pid` | yes | assigned, not chosen | partial |
| `NumaNode` | affinity ⊆ node cpus | yes | `cpus` | works |
| `TgidEquals` | `p->tgid` | **no** | — | blocked, see below |
| `IsGroupLeader` | `(p->tgid == p->pid) == want` | **no** | — | blocked, see below |
| `CgroupRegex` | userspace regex bitmap | no | — | out of scope |
| `UsedGpuTid` / `UsedGpuPid` | GPU-ownership map | no | — | out of scope |
| `HintEquals`, `AvgRuntime`, `SystemCpuUtilBelow`, `DsqInsertBelow` | userspace EWMAs | no | — | out of scope |
| `NsPidEquals`, `NsEquals` | pid namespace chain | no | — | out of scope |

The four "out of scope" rows are Gate 2 of
`LAYERED_SUPPORT_RECON_AND_REAL_CONFIGS_20260910.md`: the FFI returns `-ENOTSUP`
and the variants are absent from `LayerMatch`, so they cannot even be
expressed. That refusal is correct and is not a language problem.

---

## What is still wrong, and why it was left

Each of these is pinned by a `known_gap_*` test, so it goes red the day it is
fixed. That is the point — do not delete them, invert them.

### `tgid` is 0 for every task (`sim-6mheb`, blocked on `sim-zwypg`)

`sim_task_set_pid()` carries a `DANGER TODO(sim-6mheb)` and does not set
`p->tgid`. Consequences, all silent:

- `MATCH_IS_GROUP_LEADER` is `(p->tgid == p->pid) == want`, so
  `IsGroupLeader(true)` can never fire and `IsGroupLeader(false)` always does;
- `MATCH_TGID_EQUALS` only ever compares against 0;
- layered's `is_scheduler_task(p)` is `(u32)p->tgid == layered_root_tgid` and
  `layered_root_tgid` is also 0, so **every** simulated task is classified as
  one of scx_layered's own userspace daemon threads and takes its fast path
  instead of the ordinary layer-DSQ path.

**This is deliberately not set here, including for thread-group members.**
Setting `tgid` only for explicitly-grouped tasks would be worse than either
state: adding a `thread_of` to a workload would silently change which dispatch
path layered takes for those tasks. Setting it for everyone unmasks
`sim-zwypg`, whose root cause is recorded in that issue's comments — scx-sim's
`stop_and_reenqueue` calls `ops.enqueue` on a preempted task *before*
`ops.dispatch`, while the kernel's `balance_one()` dispatches first and then
honours a slice the scheduler refreshed from inside `ops.dispatch` by keeping
`prev` running. layered's `keep_running()` uses exactly that idiom.

So thread groups are faithful on the `group_leader` axis (which is what `pcomm`
reads) and still wrong on the `tgid` axis, exactly as they already were for
ungrouped tasks. Fixing `sim-zwypg` closes both.

### `PpidEquals(X)` also matches task X (`sim-22c6c`)

`sim_task_alloc()` sets `p->real_parent = p` — "self-referencing; simulates
init as parent". Correct for pid 1, a simplification for everyone else. A rule
saying "everything under the supervisor" therefore picks up the supervisor too.

The simplification is load-bearing and not casually removable: `Engine::new`
defers `task_pid_to_raw` registration specifically so that a self-referencing
`real_parent` makes `bpf_task_from_pid()` return NULL during `init_task`, which
is what drives LAVD down its correct initialisation path.

### `IsKthread` ignores its own argument (upstream, `sim-olwul`)

layered's arm is `return p->flags & PF_KTHREAD;` — no reference to
`match->is_kthread`, unlike `MATCH_IS_GROUP_LEADER` directly above it. So
`IsKthread(false)` behaves identically to `IsKthread(true)`.

`LayerMatch::IsKthread(bool)` mirrors the upstream schema and therefore
advertises a knob the BPF does not read. This is **deliberately not worked
around on our side** by emitting `Not` for `IsKthread(false)`: that would make
our API mean something the scheduler does not, which is the failure the whole
`layered_rtapp_naming` suite exists to catch. Negation still works through
`LayerMatch::Not`, which sets the separate `exclude` flag.

### Instance naming (`sim-bs93q`)

See "What stock rt-app actually does" above. Not fixed here because changing it
touches pid assignment, `resume`-target resolution and the export path together
— its own change, with its own risk, and nothing above is blocked on it.

---

## Conventions this document follows

- **Cite symbols, not line numbers.** Line numbers rot across every rebase;
  function and field names do not.
- **A capability is not claimed until a test drives it end to end** through the
  real BPF and reads back which layer the task landed in. Every "works (new)"
  row above has one in `tests/layered_rtapp_naming.rs`, and each was verified by
  *sabotage* — reverting the fix and confirming that test, and only that test,
  goes red.
- **No stubs.** Where the language cannot honestly express something, it says
  so and leaves it unexpressed. `thread_of` names a real task with a real
  `comm`; it does not let a spec declare a `pcomm` string out of thin air.
