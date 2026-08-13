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
