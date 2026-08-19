# How hermit does happens-before

**An account of hermit's existing implementation. No scx-sim design here —
the owner narrowed the ask to "just study how we did it in hermit".**

**Task:** tg `port-hermit-happens-before-to-scxsim`
**Date:** 2026-08-13
**Source read:** the dev-hermit workspace checkout. Everything below is quoted
from that tree; where I paraphrase, I say so.

---

## 1. Where it lives

| Layer | File | Lines |
|---|---|---|
| Model — parse, normalize, validate | `detcore-model/src/happens_before.rs` | 1275 |
| Debug-info resolution | `hermit-cli/src/happens_before.rs` | 464 |
| Enforcement | `detcore/src/scheduler.rs` (`HbRuntime`, from ~line 295) | — |
| Checkpoint issue point | `detcore/src/lib.rs` (~line 1661) | — |
| Resource kind | `detcore/src/resources.rs:297` — `HappensBeforeCheckpoint(u64)` | — |

The module doc states the purpose and the contrast with full replay:

> Happens-before edges: a sparse, authored partial order over dynamic events.
>
> Where `--replay-schedule-from` replays a *complete* total order captured from
> a prior run, a happens-before specification pins down only the *few* events
> that matter for a race and lets the deterministic scheduler fill in the rest.
> An agent (or human) that already knows a target race can therefore construct
> it deterministically instead of blind seed-search.

---

## 2. The exact syntax

### 2.1 JSON

Quoted verbatim from `spec_json()` in the module's own tests — this is the
RFC #1146 file format, and it round-trips:

```json
{
  "version": 1,
  "threads": { "writer": {"label": "writer"}, "reader": {"label": "reader"} },
  "events": {
    "X_342": {"thread": "writer", "func": "free_buffer", "line": 120, "nth": 342},
    "Y_97":  {"thread": "reader", "func": "read_buffer", "nth": 97},
    "lockA":  {"thread": "writer", "syscall": "futex", "phase": "posthook", "nth": 5},
    "storeB": {"thread": "reader", "rip": "0x401f3c", "nth": 1},
    "scA":    {"thread": "writer", "syscalls": 10},
    "rcbB":   {"thread": "reader", "rcbs": 123456}
  },
  "edges": [
    {"before": "X_342", "after": "Y_97", "strength": "hard"},
    {"before": "lockA", "after": "storeB"},
    {"before": "scA", "after": "rcbB", "strength": "soft"}
  ]
}
```

### 2.2 Terse DSL

Quoted verbatim from the grammar comment above `from_dsl`:

```
// One edge per non-empty, non-comment line:
//
//     writer:free_buffer#342  <  reader:read_buffer#97
//     writer:futex@post#5     <  reader:@0x401f3c#1
//     A:rcb=123456            <  B:sc=97
//
// Each side is `thread:anchor[#ordinal]`. The anchor token is one of:
//   * `name`            -> function name (code location)
//   * `@0xADDR`         -> raw RIP
//   * `syscall@phase`   -> a named syscall, optional `@pre`/`@post`/`@polling`
//   * `rcb=M`           -> after M RBCs (owner primary)
//   * `sc=N`            -> after N syscalls (owner primary)
// A trailing `#N` sets the occurrence ordinal (ignored by `rcb=`/`sc=`).
// A `!soft` suffix on the line marks the edge soft; default is hard.
```

### 2.3 The types

```rust
pub enum Position {
    SyscallCount(u64),                                   // after N syscalls
    Rcb(u64),                                            // when the RCB clock hits N
    Syscall { sysno: Sysno, phase: Option<SyscallPhase>, nth: u64 },
    Rip { addr: Option<u64>, nth: u64 },                 // addr resolved later
    Marker { name: String, nth: u64 },                   // "reserved for a future backend"
}

pub struct Anchor {
    pub name: String,
    pub thread: ThreadRef,
    pub position: Position,
    pub location: CodeLocation,
}

pub struct CodeLocation {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

pub enum Strength {
    /// Park the sink thread in a true gate until the source fires. The guarantee
    /// wanted for constructed repros, and the default.
    #[default]
    Hard,
    /// Merely bias scheduling (priority nudge); the sink may still run if it is
    /// the only runnable thread.
    Soft,
}
```

### 2.4 CLI

```
--happens-before <filepath>   Path to a happens-before specification (JSON)
--hb-list-events              Resolve anchors against debug info, print, and exit
```

---

## 3. How a code location is identified — the transferability question

This is the part worth the most attention, so it is answered in two halves:
how a location is *resolved*, and what is *actually matched at runtime*. They
are not the same, and the gap is the finding.

### 3.1 Resolution: real ELF, symbol table, DWARF

`hermit-cli/src/happens_before.rs`, module doc, verbatim:

> The anchor/edge *model* lives in [`detcore_model::happens_before`]; it leaves
> `func`/`file:line` code locations unresolved (a [`Position::Rip`] with
> `addr: None`). This module turns those human-legible locations into concrete
> instruction pointers by reading the target binary's symbol table (for
> function entry addresses) and DWARF line program (for source lines), and
> provides the reverse mapping (address → function/file/line) used by
> introspection / `--list-events`.

Its imports name the mechanism outright: `addr2line::gimli`, and `object::{Object,
ObjectSection, ObjectSymbol, SymbolKind}`. A `FuncExtent { addr, size }` is
built per function symbol.

So a `func`/`file:line` anchor becomes an absolute virtual address in a
specific ELF image, obtained from that image's symbol table and DWARF.

### 3.2 Matching: only syscall counts actually fire

The runtime matcher is `anchors_at_syscall`, quoted in full:

```rust
fn anchors_at_syscall(&self, dettid: DetTid, count: u64) -> Vec<String> {
    self.program
        .anchors
        .values()
        .filter(|a| {
            matches!(a.position, Position::SyscallCount(n) if n == count)
                && self.thread_matches(&a.thread, dettid)
        })
        .map(|a| a.name.clone())
        .collect()
}
```

It matches `Position::SyscallCount` **and nothing else**. Confirmed by the
complement, `unenforced_positions`:

```rust
pub fn unenforced_positions(&self) -> impl Iterator<Item = &Anchor> {
    self.anchors
        .values()
        .filter(|a| !matches!(a.position, Position::SyscallCount(_)))
}
```

and by the warning `HbRuntime::new` emits for each such anchor:

> `[happens-before] anchor {} uses position '{}', which the scheduler does not
> yet enforce (only 'after N syscalls' is enforced); this ordering constraint
> will NOT be applied`

**So the whole RIP / function / file:line / DWARF apparatus resolves, prints
under `--hb-list-events`, and is then ignored by the scheduler.** The only
addressing that gates anything is "after N syscalls on thread T". `Rcb` is
modelled and parsed and also never fires.

### 3.3 Where the count comes from

`detcore/src/lib.rs`, on the syscall path, quoted:

```rust
if guest
    .config()
    .happens_before
    .as_ref()
    .is_some_and(|p| p.has_syscall_count_anchors())
{
    let request = guest.thread_state().mk_request(
        ResourceID::HappensBeforeCheckpoint(new_count),
        Permission::R,
    );
    resource_request(guest, request).await;
}
```

with the comment above it stating the precondition:

> It requires sequentialized threads (enforced by the CLI) so the scheduler
> owns ordering.

Every syscall increments a per-thread counter and, when the program has any
`SyscallCount` anchor, raises a checkpoint resource request. There is no
instrumentation, no breakpoint, and no single-stepping involved in the path
that actually works — the syscall interception hermit already performs is the
only hook.

---

## 4. How the delay is enforced

`Scheduler::hb_checkpoint`, the essential part quoted:

```rust
let reached = hb.anchors_at_syscall(dettid, count);
let blocked = reached.iter().any(|name| hb.anchor_blocked(name));
...
if reached.is_empty() {
    // No anchor addresses this (thread, count); nothing to gate or fire.
    return Ok(());
}

if blocked {
    info!("[scheduler] ... SKIP dettid {} held at happens-before anchor(s) {:?} \
           (syscall count {}) awaiting a BEFORE anchor", dettid, reached, count);
    self.happens_before.as_mut().unwrap().parked.insert(dettid);
    return self.skip_turn_blocked(dettid);
}
```

`anchor_blocked` is documented as:

> True when anchor `name` is the AFTER endpoint of a Hard edge whose BEFORE
> endpoint has not yet fired -- i.e. a thread reaching `name` must be held.

The runtime state is four fields, and their doc comments explain the design:

```rust
struct HbRuntime {
    program: HappensBeforeProgram,
    /// Names of anchors that have fired. Monotonic: an anchor fires at most once,
    /// when its thread is first granted passage past it.
    fired: BTreeSet<String>,
    /// Threads currently parked at an AFTER anchor, out of the run queue, awaiting
    /// their gating BEFORE anchor(s). A `BTreeSet` keeps re-admission order
    /// deterministic.
    parked: BTreeSet<DetTid>,
    /// Threads observed at creation time, in deterministic spawn order ...
    spawn_order: Vec<DetTid>,
    /// Set when a newly fired anchor may have opened a parked thread's gate ...
    wake_pending: bool,
}
```

Two details worth carrying forward:

- **Firing is monotonic and idempotent.** "Only wake parked threads when a new
  anchor actually fired, so an idempotent re-grant causes no churn."
- **Re-admission is deferred.** From the `wake_pending` doc: re-admission
  happens at the next `step3` boundary "because it pushes to the run queue,
  which is illegal while a `tentative_pop` selection is in progress (as it is
  inside `block_for_one_resource`, where anchors fire)." A port that parks
  inside a selection routine will hit the same constraint.

**Mechanism summary:** the gate is *removal from the run queue*, not a sleep,
not a priority tweak. `Strength::Soft` is the priority-tweak variant and is
described as "merely bias scheduling".

---

## 5. What happens when a constraint cannot be satisfied

**Statically — cycles are rejected.** `normalize()` calls `detect_cycle(&anchors,
&edges)?`, a DFS that returns `HappensBeforeError::Cycle(Vec<String>)` listing
the cycle in discovery order, rendered as `"happens-before edges contain a
cycle: ..."`. The other validation errors are `UnsupportedVersion`,
`AmbiguousPosition`, `UnknownSyscall`, `BadRip`, `UnknownEvent`,
`UnknownThread`, and `DslSyntax`.

**At runtime — there is no happens-before-specific unsatisfiability check.**
I looked for one and did not find it. If a BEFORE anchor never fires (its
thread took a different path, or exited, or never reaches that syscall count),
the parked thread simply stays out of the run queue. It then falls into the
scheduler's *general* deadlock machinery — `terminal_deadlock: Option<String>`,
`take_terminal_deadlock()`, `report_terminal_deadlock` — which is not
HB-aware and so will not tell you *which* anchor failed to fire.

That is a real gap for a port: the diagnostic an author most needs ("edge
X→Y never opened because X never fired") is not produced.

---

## 6. Known limitations, stated by the code itself

1. **Only `SyscallCount` is enforced.** Everything else — `Rcb`, `Syscall`,
   `Rip`, `Marker`, and therefore every `func`/`file:line` anchor — parses,
   validates, resolves, prints, and is then skipped with a warning (§3.2).
2. **`Marker` is unimplemented by design** — "A cooperative guest marker.
   Reserved for a future backend."
3. **Requires sequentialized threads**, enforced by the CLI (§3.3).
4. **No runtime unsatisfiability diagnostic** (§5).
5. **THE CLI HELP TEXT IS STALE, and contradicts the code.** `--happens-before`
   is documented as:

   > Scheduler enforcement is not yet wired; combine with `--hb-list-events` to
   > preview how the spec resolves against the binary.

   That is no longer true: `detcore/src/lib.rs` issues the checkpoint and
   `Scheduler::hb_checkpoint` parks threads, and `scheduler.rs:1240` builds the
   runtime with `happens_before: cfg.happens_before.clone().map(HbRuntime::new)`.
   Enforcement *is* wired, for `SyscallCount` only. Anyone reading the `--help`
   would conclude the feature does nothing at all. Flagging rather than fixing:
   it is hermit's repo, not ours.

---

## 7. The one thing to know before designing a port

The addressing that hermit *advertises* — name a function, name a source line —
is the addressing hermit *does not enforce*. The addressing it enforces is a
per-thread syscall counter, which needs no debug info, no breakpoints and no
instrumentation; it rides on the syscall interception the deterministic
hypervisor already does.

So the question "does scx-sim have an equivalent of hermit's instruction
pointers / breakpoints / instrumentation?" turns out not to be the blocking
question, because hermit does not use them for enforcement either. The blocking
question is the narrower one: **what is scx-sim's equivalent of "the countable,
deterministic, per-thread event that the scheduler already intercepts"?**

Answering that is design work, and is deliberately not in this document.

---

# Part 2: what is scx-sim's equivalent of a syscall count?

Follow-up study question, still not design. Hermit's working mechanism is
"block at the Nth occurrence of a countable, engine-sequenced event". The
question is what scx-sim's version of that event is, judged on three criteria,
of which the third is binding:

1. Does the engine already sequence it deterministically?
2. Can the engine block at one?
3. **Can an agent reading LAVD source name it?**

An event that fails (3) is useless even if it passes (1) and (2), because the
whole point is to turn "the race is X then Y then Z" into a constraint.

## The single block point

There is exactly one, and everything else is judged relative to it:

```rust
pub fn maybe_yield() {
    // Try preemptive yield first (no-op if preempt context not installed).
    crate::preempt::maybe_yield_preemptive();

    // Cooperative yield fallback (no-op if interleave context not installed).
    let ctx = INTERLEAVE_CTX.with(|c| c.get());
    ...
    let switched = unsafe { (ctx.yield_fn)(ctx.ring_data, ctx.worker_id) };
```

`interleave.rs:374`. It is called from **20 sites in `kfuncs.rs`** and nowhere
else. So every candidate below is really a question about "which kfunc yield,
described how".

## The engine already names its yield points

This is the finding. The kfunc wrapper does not just yield — it *names* the
site first:

```rust
macro_rules! define_cgroup_bw_yield {
    ($fn_name:ident, $site_name:literal) => {
        #[no_mangle]
        pub extern "C" fn $fn_name() {
            crate::preempt::set_current_kfunc($site_name);
            crate::interleave::maybe_yield();
        }
    };
}

define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_put_aside,  "cgroup_bw_put_aside");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_consume,    "cgroup_bw_consume");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_reenqueue,  "cgroup_bw_reenqueue");
```

18 distinct `set_current_kfunc("...")` names exist, including `dsq_insert`,
`dsq_insert_vtime`, `dsq_move`, `dsq_move_to_local`, `dsq_nr_queued`,
`kick_cpu`, `select_cpu_dfl`, `select_cpu_and`, `task_cpu`, `task_running`,
`scx_clock_task`, plus the 13 `cgroup_bw_*` site names.

The callback context is tracked alongside it:

```rust
pub enum OpsContext {
    None = 0, SelectCpu = 1, Enqueue = 2, Dispatch = 3, Tick = 4,
    Stopping = 5, Running = 6, UpdateIdle = 7, FireTimer = 8,
    CpuOnline = 9, CpuOffline = 10, Runnable = 11, Quiescent = 12,
    Dequeue = 13, Enable = 14, ...
}
```

Both live in thread-locals, set at every yield:

```rust
/// Current kfunc name, set before each `maybe_yield()` call.
static CURRENT_KFUNC_NAME: Cell<&'static str> = const { Cell::new("") };
/// Current ops context, cached from SimulatorState at structop boundary.
static CURRENT_OPS_CONTEXT: Cell<OpsContext> = const { Cell::new(OpsContext::None) };
```

## And it already counts them

`preempt/mod.rs:798-810`:

```rust
static STRUCTOP_CPU_COUNT:         Cell<u64>;
static STRUCTOP_RBC_TOTAL:         Cell<u64>;
static STRUCTOP_KFUNC_COUNT:       Cell<u64>;
static STRUCTOP_INTERLEAVE_COUNT:  Cell<u64>;
/// Per-structop kfunc count (resets at each structop boundary).
static STRUCTOP_KFUNC_COUNT_LOCAL: Cell<u64>;
static IN_STRUCTOP:                Cell<bool>;
static STRUCTOP_GLOBAL_COUNT:      AtomicU64;
```

surfaced together as `StructopInfo { cpu_count, global_count, rbc_total,
kfunc_count, interleave_count, ops_context, kfunc_name, kfunc_count_local }`.

`kfunc_count_local` is the interesting one: *"Per-structop kfunc count (resets
at each structop boundary). Tracks how many kfuncs have executed within the
current structop."*

## Candidate table

| Candidate | Engine-sequenced? | Can block at it? | Nameable from LAVD source? |
|---|---|---|---|
| **Named kfunc site** (`dsq_insert`, `cgroup_bw_put_aside`, …) | yes | **yes** — `maybe_yield()` is called at that exact site | **yes** — it is the function LAVD calls |
| **Ops callback** (`OpsContext::Enqueue`, `Dispatch`, `Tick`, `Running`, `Stopping`) | yes | yes, via any kfunc yield inside it | **yes** — LAVD defines `ops.enqueue`, `ops.dispatch`, … |
| **Nth kfunc within the current callback** (`kfunc_count_local`) | yes, on the preemptive path (below) | yes | **yes**, compositionally — "the 2nd `dsq_insert` in `ops.enqueue`" |
| Global kfunc count (`kfunc_count`, `global_count`) | yes | yes | **no** — exactly the "4,182nd kfunc call" nobody can write |
| RBC (`rbc_total`, `arm_replay_timer(target_rbc)`) | yes, deterministic per the PMU RBC section of `CLAUDE.md` | yes | **no** |
| Interleave count | yes | yes | **no** |
| `TraceKind` events (55, incl. `LavdBailOnCgroupThrottle`, `CbwPutAside`, `TaskScheduled`, `Tick`) | yes | **no** — emitted to a trace sink, not a decision point | **yes**, the most nameable of all |

## The intersection is NOT empty

**The answer: `(OpsContext, kfunc_name, nth-occurrence)`.**

An agent reading LAVD source can write *"the 2nd `dsq_insert` inside
`ops.enqueue`"*, and all three components are already maintained by the engine
at the one point where it can block. That is a **composite name**, not a bare
count — which makes scx-sim *better* positioned than hermit here, because
hermit's only enforced address (`SyscallCount(97)`) is precisely the kind that
fails criterion 3. scx-sim's natural address is the kind hermit models but
never enforces.

## Three honest caveats

1. **The counters only advance on the preemptive path.** `cooperative_yield_impl`
   — which calls `maybe_begin_structop`, `set_current_ops_context` and
   `inc_structop_kfunc` — early-returns when the preempt context is absent:

   ```rust
   fn cooperative_yield_impl(phase: KfuncYieldPhase) {
       let ctx = PREEMPT_CTX.with(|c| c.get());
       let ctx = match ctx { Some(ctx) => ctx, None => return };
   ```

   Consistent with the 14 `// dormant: parallel-dispatch / replay / preemptive
   path, inert in the sequential engine` markers in that module, and with
   `structop_info()`'s only consumers being the `e9patch`, `pmu` and `replay`
   backends. `set_current_kfunc` itself is unconditional; the *counting* is not.
   So in the default sequential engine the *name* is always available and the
   *ordinal* is not.
2. **`TraceKind` events are observation-only.** The most nameable surface — 55
   variants, several named directly after LAVD behaviour, e.g.
   `LavdBailOnCgroupThrottle`, captured "by the `scxsim_cgroup_bw_observe_put_aside`
   hook installed by the wrapper.c `scx_cgroup_bw_put_aside` macro AFTER the lib
   call returns 0" — but they go to a sink, and `TraceEvent` is
   `{time_ns, cpu, kind}` with **no sequence number**. Nothing blocks on them.
3. **Only kfunc boundaries are addressable at all.** LAVD code computing
   between kfunc calls passes through no yield point, so no constraint can name
   a position inside it. Hermit has the same shape of limit at syscall
   boundaries, so this is parity, not a regression.
