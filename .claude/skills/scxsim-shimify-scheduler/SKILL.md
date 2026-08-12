---
name: scxsim-shimify-scheduler
description: Bring an arbitrary sched_ext BPF scheduler up inside scx-sim (scxsim) — compile its real BPF as userspace C behind a wrapper, wire the Rust loader, prove behaviour with non-vacuous tests, and report honestly what is and is not covered.
---

# Shim-ifying a scheduler into scxsim

You are bringing a sched_ext scheduler (`scx/scheds/rust/scx_<name>/`) up
inside scxsim so its **real BPF logic executes** under simulation.

This skill was distilled immediately after `scx_layered` was brought up end to
end, with six wrappers in the tree (`simple`, `lavd`, `mitosis`, `cosmos`,
`tickless`, `layered`) to separate what is general from what was
scheduler-specific. Everything in the trap catalogue cost someone real time.

---

## 0. The rule that governs everything

**The No-Stub Rule (`scx-sim/CLAUDE.md`): scxsim runs 100% of the real BPF
logic. scxsim models the KERNEL; the scheduler models the SCHEDULER.**

> **Uncovered is fine. Faked is an emergency.**

A code path the simulation never reaches is honest — write it down and move
on. A code path replaced by something that returns plausible values is a
silent lie that will be cited as evidence about production behaviour.

Forbidden, in ascending order of how convincing they look:

1. **No-op shims** — `sim_*` wrappers returning success without doing the work.
2. **Elided libraries** — quietly dropping a translation unit so it compiles.
3. **Interface-only re-implementations** — Rust/C that mirrors the interface
   and *approximates* the semantics. This is the dangerous one: it looks
   correct, matches on easy cases, and diverges exactly where bugs live.
4. **Silent fallbacks** to a simplified path.

The test to apply to every line you are about to write:

> In production, does the **kernel** own this state/decision, or does the
> **BPF scheduler**? Kernel → legitimate scxsim engine code. Scheduler →
> delete it and let the scheduler's own code run.

If a BPF feature genuinely cannot run yet, that is a **scxsim substrate
task**: file a bead, mark `DANGER TODO(<issue>)`, and treat the scheduler as
not-yet-supported for that feature. It is not a licence to stub.

### Two real category-1 violations found in this tree — recognise the shapes

**(a) `scx_bpf_dump_bstr` was a no-op.** It discarded every scheduler's
`ops.dump` output. Consequence: a dump that *faulted* and a dump that did
*nothing* were indistinguishable, and every wrapper's `bpf_snprintf` was
unverified. Nobody noticed because "the dump test passes" was true.
*Shape to recognise:* a substrate function whose body is `{}` and whose
callers' behaviour is therefore unobservable. **Fix:** make it capture or
compute for real, then assert on the output.

**(b) The cosmos PMU shim.** `schedulers/cosmos/wrapper.c` hand-writes
`scx_pmu_*` bodies that fabricate counter values from elapsed `scx_bpf_now()`
deltas — *and* `cosmos_setup()` sets `perf_config = 1`, forcing the
PMU-enabled branch, so cosmos's real **no-PMU** path never runs under
simulation. That is two violations stacked: invented data, plus a
configuration that guarantees the honest path is never exercised.
*Shape to recognise:* you are writing a function body that returns a number
you made up, and separately turning on the feature that consumes it.
**Fix, and the pattern to copy:** compile the *real* library in
(`scx/lib/pmu.bpf.c`) and supply only the genuine hardware primitive beneath
it (`bpf_perf_event_read_value`), returning an honest `-ENOENT` when the
simulated machine has no such counter. Then the feature is simply *off*,
which is a real production configuration. See `schedulers/layered/wrapper.c`.

**Before writing any function body, ask: am I about to invent a value?** If
yes, stop. Either compile the real thing in, or return the honest "not
available" answer the kernel would return, or file a substrate bead.

---

## 1. Worktree protocol

> Work in your **own** git worktree. Never write in `sched-test1` (integration
> only). No host-specific absolute paths in committed files. Do **not** commit
> an `scx` submodule pin bump. Do not push without checking — feature branch
> names need review.

Confirm before starting:

```bash
git status --porcelain | wc -l     # expect 0
git submodule status               # expect no leading '+' or '-' on scx
```

A `+` on `scx` means the submodule working tree differs from the committed
gitlink — resolve that first, or you will build against something the branch
does not record. (This has bitten before: a pin bump and its matching wrapper
fix were committed separately, so the recorded pin did not build.)

---

## 2. Phase 0 — the compile spike. Do this FIRST.

**This is the highest-value hour of the whole job.** It converts an unknown
into a bounded gap list, and it routinely refutes the plan. For `scx_layered`
the plan assumed a large porting effort; the spike showed the BPF compiled as
userspace C on the first attempt with the existing include set, no source
patching. The estimate went from 8–15 days to ~1 day.

Do not skip it. Do not start writing a wrapper before you have run it.

```bash
cd scx-sim
SCHED=<name>                      # e.g. cake
SRC=../scx/scheds/rust/scx_$SCHED/src/bpf
ls $SRC                           # note EVERY .c — they are separate TUs

mkdir -p /tmp/spike && cat > /tmp/spike/w.c <<'EOF'
#include "sim_wrapper.h"
#include "sim_task.h"
#include "intf.h"
/* add every other .bpf.c the scheduler has, then main.bpf.c LAST */
#include "main.bpf.c"
EOF

ROOT=$(git rev-parse --show-toplevel)
BPF_INC=$(ls -d target/debug/build/libbpf-sys-*/out/include | head -1)
clang -fPIC -DSCX_BPF_UNITTEST -g -O2 -Wno-unused-parameter \
  -Wno-unknown-attributes -Wno-implicit-function-declaration -Dconst= \
  -DSCX_CGROUP_BW_NEW_API=1 -DSCXSIM_PHASE2_REAL_CGROUP_BW=1 \
  -I$ROOT/scx-sim/csrc -I$ROOT/lib/scxtest -I$ROOT/scheds/include \
  -I$ROOT/scheds/include/lib -I$ROOT/scheds/vmlinux \
  -I$ROOT/scheds/vmlinux/arch/x86 -I$ROOT/scheds/include/bpf-compat \
  -I$BPF_INC -I$SRC -c -o /tmp/spike/w.o /tmp/spike/w.c 2>&1 | head -40
```

Then the gap list:

```bash
nm -u /tmp/spike/w.o | sed 's/^ *U //' | sort -u
```

Classify each undefined symbol against the existing providers:
`crates/scx_simulator/src/unsafe_impl/kfuncs.rs` (Rust `#[no_mangle]`),
`scx-sim/csrc/sim_*.c`, `lib/scxtest/*.c`. Whatever is left is your real work.

Record the spike result before proceeding — the count is your estimate.
(layered: 7 genuinely missing symbols, not the 14 the plan predicted.)

> ### ⚠ THE UNDEFINED-SYMBOL AUDIT IS NOT SUFFICIENT. READ THIS.
>
> `bpf_helper_defs.h` declares many helpers as **static function pointers
> initialised to the raw BPF helper NUMBER**:
> `static long (*bpf_strncmp)(...) = (void *) 182;`
>
> They **link cleanly** and never appear in `nm -u`. A link-time audit will
> tell you the scheduler is complete. Then the first call jumps to address
> 182 and SIGSEGVs.
>
> Verified against this tree: of the risky set, **only
> `bpf_get_prandom_u32` is already handled globally**
> (`lib/scxtest/overrides.h`). Every one of these is **NOT** covered
> anywhere and each wrapper must override it itself before it is called:
>
> | Helper | Reached from |
> |---|---|
> | `bpf_printk` | any `dbg()`/`trace()` macro — often gated on a `debug` rodata, so it detonates only when a test raises it |
> | `bpf_jiffies64` | delay/watchdog accounting |
> | `bpf_strncmp` | name matching |
> | `bpf_probe_read_str` | reading task/cgroup names |
> | `bpf_snprintf` | dump/debug header formatting |
> | `bpf_map_delete_elem` | any map eviction (`scx_test_map.h` defines lookup/update/task_storage_get but **not** delete) |
> | `bpf_get_current_pid_tgid` | probe/tracepoint paths |
> | `bpf_task_storage_delete` | task-storage eviction |
>
> Copy the override block from `schedulers/layered/wrapper.c`. Note the
> `bpf_printk` case specifically: it is usually behind `if (debug)`, so the
> scheduler runs fine until someone turns debugging on.
>
> **Find them by grep, not by linker:**
> ```bash
> grep -oE 'bpf_[a-z_0-9]+\(' $SRC/*.c | sed 's/.*://;s/($//' | sort -u
> ```
> Cross-check every hit against `sim_wrapper.h`, `lib/scxtest/overrides.h`,
> `scx_test_map.h` and your wrapper's own `#define`s. Anything unaccounted
> for must be macro-overridden in your `wrapper.c` before it is ever called.

---

## 3. Phase 1 — the per-scheduler decision table

These four differ **per scheduler**. Getting one wrong costs a day. Decide
each deliberately and write the reasoning in a comment.

### 3.1 `bpf_ksym_exists()` — do NOT force it reflexively

`compat.bpf.h` gates modern-vs-legacy kfunc paths on weak-symbol presence.
The tree contains all three answers, and all three are correct *for their
scheduler*:

| Scheduler | Choice | Why |
|---|---|---|
| mitosis | forced `0` | avoids `scx_bpf_select_cpu_and`, which it does not need |
| cosmos | forced `1` | *wants* `scx_bpf_select_cpu_and` |
| **layered** | **not forced** | needs both answers simultaneously |

layered is the instructive case: `scx_bpf_cpu_curr` and
`scx_bpf_reenqueue_local___v2___compat` **are** exported by scxsim (so the
modern paths must run), while `scx_bpf_task_set_slice___new` is **not** (so
that one must fall back to the direct `p->scx.*` write scxsim supports).
Forcing `1` makes the second group jump through a NULL weak symbol; forcing
`0` makes the first group take a dead fallback (`scx_bpf_reenqueue_local___v1`
is NULL → SIGSEGV).

**Default: leave it alone** and let the real weak-symbol test decide. Only
force it if you can name the specific symbol you are steering and have
checked the other consumers. Enumerate them:

```bash
grep -n 'bpf_ksym_exists' ../scheds/include/scx/compat.bpf.h
grep -n '__COMPAT_\|scx_bpf_' $SRC/*.c | sort -u | head -40
```

### 3.2 `cleanup.bpf.h` RAII — native or neutralised?

`__free(...)`, `no_free_ptr()`, `scoped_guard()`, `DEFINE_GUARD` are
`__attribute__((cleanup))`, which **works natively in userspace C**.

* **layered:** works natively. Nothing neutralised. Destructors really run.
* **mitosis:** neutralises `__free`, `bpf_cgroup_acquire/release`,
  `bpf_kptr_xchg`, RCU locks, and hand-rolls `bpf_iter_css_*`.

Prefer native — it executes the scheduler's real resource handling. Only
neutralise a specific macro when its destructor calls something scxsim cannot
provide, and say which one in the comment. Blanket-neutralising because
mitosis did is how you silently disable a scheduler's cleanup paths.

### 3.3 Source patching — assume NOT needed

* **cosmos:** needs a `sed`'d `main.bpf.c` (BPF division-by-zero returns 0;
  native C raises SIGFPE) — generated by a rule in `config.mk`.
* **layered:** zero patching.

Patching is a last resort and a documented divergence. Try unpatched first.
If you do patch, generate it from the pristine source in `config.mk` (never
edit the submodule) and comment exactly which BPF-vs-C semantic forced it.

### 3.4 `bpf_for_each(scx_dsq, ...)`

The iterator uses a `cleanup()` destructor, so macro rewrites are not enough
— it needs **concrete symbols**. Copy the three-function block
(`bpf_iter_scx_dsq_new/next/destroy`) from `schedulers/layered/wrapper.c`.

---

## 4. Phase 2 — the wrapper

`schedulers/<name>/wrapper.c` + `config.mk`. The Makefile auto-discovers any
subdirectory containing `wrapper.c`; there is no enum to extend and
`--scheduler` is a free-form string.

`config.mk` minimum:

```make
EXTRA_CFLAGS_<name> := -Dconst=          # BPF "const volatile" globals must be writable
<NAME>_BPF_DIR := $(ROOT_DIR)/scheds/rust/scx_<name>/src/bpf
EXTRA_INCLUDES_<name> := -I$(<NAME>_BPF_DIR) -I<name>
```

`wrapper.c` structure, in this order — the order matters, because macros must
be in effect before the scheduler source is included:

1. `#include "sim_wrapper.h"`, `"sim_task.h"`
2. externs (libc, `sim_*` entry points)
3. `__kconfig` globals the scheduler declares (`CONFIG_HZ`, …)
4. **helper macro overrides** (§2 warning box)
5. map-routing forward declarations + `#undef`/`#define`
6. timer routing
7. concrete `bpf_iter_scx_dsq_*`
8. `#include "intf.h"`, then every `.bpf.c`, `main.bpf.c` last
9. static map storage, routing implementations, probes, `<name>_setup()`

### The wrapper is *userspace*, and only userspace

Most schedulers are userspace-driven. Your wrapper plays the role that
scheduler's Rust `main.rs` plays: publish topology, publish config, then get
out of the way. It must never make a scheduling decision.

### Map backing: static arrays, deliberately

`scx_test_map` grows its value storage with `reallocarray()`, so **any pointer
the scheduler holds across an insert dangles** — and schedulers hold
`task_ctx *` / `cpu_ctx *` across nested lookups constantly. BPF `ARRAY`,
`PERCPU_ARRAY` and `TASK_STORAGE` maps *are* preallocated in the kernel, so a
fixed static array is the **more faithful** model, not a shortcut. Route only
genuinely sparse `HASH` maps through the registry. Say this in a comment or a
reviewer will read it as laziness.

### `ops.init` shim

If the scheduler's userspace does work immediately after attach (publishing
cpumasks, per-node contexts), rename the BPF init and wrap it:

```c
#define <name>_init <name>_bpf_init
#include "main.bpf.c"
#undef <name>_init

int <name>_init(void) {
    /* pre-attach: publish config */
    int ret = <name>_bpf_init();
    if (ret) return ret;
    /* post-attach: what main.rs does right after attaching */
    return 0;
}
```

### Add read-only probes

Export `<name>_probe_*` functions reading real scheduler state
(`task_ctx.layer_id`, per-CPU stat arrays, published topology). Tests must
assert on what the scheduler *decided*, never on a re-derivation. Wrap them in
Rust in `unsafe_impl/probes.rs` (`LayeredProbes` is the model).

Also export an **enum ABI probe** returning the scheduler's `intf.h` enum
values, and assert your Rust mirror matches. An upstream reordering then fails
a test instead of silently mis-configuring everything.

---

## 5. Phase 3 — Rust side

* `unsafe_impl/ffi.rs`: `DynamicScheduler::<name>(nr_cpus)` plus
  topology/config constructors. Optional ops resolve via `try_get!`, so a
  missing symbol is `None`, not a panic.
* If the scheduler implements a struct_op the engine does not deliver yet,
  that is **engine substrate**: add the fn-pointer type, `SchedOps` field,
  trait method, `try_get!` entry, and an engine call site **at the kernel's
  own call point**. Get the kernel's ordering right — e.g. `set_weight`
  immediately after `ops.enable` (`scx_enable_task()`), `disable` immediately
  before `ops.exit_task` (`scx_disable_task()`).
* Adding a `TraceKind` means touching `trace.rs`, `perfetto.rs`,
  `perfetto_pb.rs` (two sites) and `structops_jsonl.rs`.
* Pure-data config types go in `safe/` (`#![forbid(unsafe_code)]`).

### Linking a scheduler's Rust userspace logic

If the scheduler's *policy* lives in Rust (allocators, growth algorithms),
re-implementing it is a §0 violation. Check whether the module is
self-contained:

```bash
grep -n '^use ' $SRCDIR/alloc.rs
grep -c 'libbpf\|bpf_skel\|Topology\|std::fs\|unsafe' $SRCDIR/alloc.rs
```

If it only needs crate-local pure helpers, compile it in verbatim:

```rust
// in safe/mod.rs — from a mod.rs, so #[path] resolves relative to safe/
#[path = "../../../../../scx/scheds/rust/scx_<name>/src/alloc.rs"]
pub mod <name>_alloc_upstream;
```

Use `#[path] mod`, **not** `include!` — upstream files open with `//!` inner
doc comments, legal only at the top of a module. Depending on the whole
scheduler crate is a dead end: its `lib.rs` pulls the generated BPF skeleton
(needing bpftool + a clang BPF target at build time) plus libbpf-rs and more.

Bonus: upstream's own unit tests come along and run in your suite.

If you must vendor a helper, **splice it programmatically, never by hand**,
and add a drift guard that re-reads upstream at test time and asserts
token-identity. A hand transcription in this tree silently changed
`for &i` to `for &idx` — semantically identical, so nothing would ever have
failed, but the claim was "verbatim". Anchor the guard's extractor at line
start (or it matches doc-comment mentions) and give it a self-test.

---

## 6. Phase 4 — tests, and the proof standard

> **A passing test that would pass anyway proves nothing.**

Every behavioural claim needs a **negative control or a sabotage check**.
Always set `.detect_bpf_errors()` so a `scx_bpf_error` fails the test.

**Negative control** — run the *same* workload twice, once where the
mechanism must engage and once where it must not:

```
--antistall-sec 0     -> GSTAT_ANTISTALL == 589   (engaged)
--antistall-sec 3600  -> GSTAT_ANTISTALL == 0     (identical workload)
```
The second arm *is* the test. Without it you are asserting on a counter that
might increment unconditionally.

**Sabotage check** — temporarily break the mechanism, confirm the tests fail,
restore, confirm the file is byte-identical. Worked examples: disabling tp_btf
delivery failed 4 tests; making match-rule installation a no-op failed 9.

**Do not test that the scheduler agrees with itself.** A topology test here
compared the wrapper's published LLC map against a re-derivation of the same
constant fed to both sides — it would not have caught an inconsistently
configured `Scenario`. The test that mattered was behavioural: tasks pinned
into different LLCs must land on **different DSQs**, with a flat-topology
control arm.

**Watch the test COUNT, not just the colour.** A scripted edit here silently
deleted four tests; `nextest` stayed green throughout and the only signal was
1031 → 1028. After any bulk edit:

```bash
git show HEAD:<testfile> | grep -o '^fn [a-z_]*' | sort > /tmp/before
grep -o '^fn [a-z_]*' <testfile> | sort > /tmp/after
diff /tmp/before /tmp/after
```

Add the scheduler to the cross-scheduler suites (`determinism`,
`per_cpu_isolation`, `scheduling_invariants`, `scheduler_comparison`,
`examples_matrix`) so it is held to the general invariants.

---

## 7. Phase 5 — report honestly, in tiers

Use the tier framing and **do not inflate**:

* **Tier 1** — compiles, loads, runs a trivial config on flat topology.
  **This is NOT "supported."** Most of the scheduler's logic is dead code
  behind a one-element config.
* **Tier 2** — multi-{layer,cell,domain}, real LLC/SMT topology, the
  scheduler's matching/placement policy, its timers, its dump. **This is the
  bar for "supported."**
* **Tier 3** — the userspace control loop, if the scheduler has one.

For each criterion state whether it is **exercised by a test that would fail
if the behaviour broke**, or whether it merely compiles and runs. Put the
table in the design doc. Distinguish:

* *genuinely exercised* — sabotage/control proven;
* *publication only* — value is published and asserted correct, but no test
  shows it changing a decision;
* *inherently unobservable* — the engine cannot model the consequence (e.g.
  scxsim has **no NUMA concept at all**: no per-CPU node id, no distance, no
  cost to a cross-node placement, so cross-NUMA gating logic cannot be
  exercised at all).

Deliverables: `scx-sim/ai_docs/<NAME>_SUPPORT.md`, the README scheduler table,
and a bead per documented divergence.

**Report your own near-misses.** Two tests in this tree were caught
overclaiming *by their author*. That disclosure is what makes the rest of the
report trustworthy.

---

## 8. Trap catalogue

1. **`bpf_helper_defs.h` helpers link cleanly and SIGSEGV when called.** §2.
2. **`bpf_ksym_exists()` must not be forced reflexively.** §3.1.
3. **Static arrays back the maps deliberately** — `reallocarray()` dangles
   held pointers. §4.
4. **A huge `struct` in BSS must never be wholesale `memset`.**
   layered's `layers[]` is ~165 MB; clear only the scalar tail.
5. **Probe structs hold raw fn pointers into the `.so`.** Keep the
   `Simulator` alive (`let sim = Simulator::new(sched); let t = sim.run(..)`)
   or `dlclose` dangles them — SIGSEGV *after* the run reports success.
6. **`ops.yield`'s return is discarded for a plain `sched_yield()`.**
   `yield_task_scx()` zeroes the slice only when there is *no* `ops.yield`.
   Model "absent" and "returned false" as different states.
7. **A catch-all match group needs its count published explicitly** — an
   empty AND-group matches everything but has no rule to grow the counter.
8. **`p->scx.runnable_at` and friends may be unset.** Any field the scheduler
   reads that the engine never writes reads as 0 — for `runnable_at` that
   means every task looks infinitely delayed. Grep the scheduler for
   `p->scx.` and verify the engine populates each field.
9. **Derive time constants from one source.** `CONFIG_HZ` /
   `bpf_jiffies64()` must agree with the engine's tick interval.
10. **`rustfmt` rewrites string-literal line continuations** into embedded
    spaces. Use `concat!()` for fixtures whose exact bytes matter.
11. **`validate.sh` may fail for reasons that are not yours.** Check whether
    the failure predates your branch before "fixing" it into a rebase
    conflict.

---

## 9. Portability: you do NOT need a build-time fetch

Everything required to shim an scx scheduler is already vendored:

| Need | Location |
|---|---|
| scheduler sources | `scx/scheds/rust/scx_<name>/src/bpf/` |
| scx headers | `scx/scheds/include/` |
| `vmlinux.h` | `scx/scheds/vmlinux/` — pre-generated and committed |
| test substrate | `lib/scxtest/` |
| libbpf headers | the `libbpf-sys` cargo dep |

Bringing layered up required **zero** fetching, confirmed by a from-scratch
build in a clean `CARGO_TARGET_DIR`.

For the record, so it is not re-opened: ktstr's `build.rs` (ktstr master) does
contain a hermetic, SHA-256-pinned, CAS-backed build-time fetch — but it
fetches a **BusyBox tarball and wprof**, i.e. VM test infrastructure, not
headers. Its answer to the header question is to **generate `vmlinux.h` from
local kernel BTF** via libbpf's `btf_dump`, not to download anything. Neither
is needed here. If some future asset genuinely must be fetched, read that
`build.rs` first and reuse the pattern rather than reinventing it (its author
notes ~30% of the effort was performance and overcommit handling).

Add the scheduler's source dir to `build.rs`'s `rerun-if-changed` list, or a
submodule bump will silently reuse a stale `.so`.

---

## 10. Definition of done

- [ ] Compile spike run; gap list recorded before any wrapper was written.
- [ ] Every `bpf_*` helper the scheduler calls is accounted for **by grep**,
      not only by `nm -u`.
- [ ] Each §3 decision made deliberately, with the reasoning in a comment.
- [ ] No invented values anywhere; real libraries compiled in, not shimmed.
- [ ] Tests use `.detect_bpf_errors()`; each behavioural claim has a negative
      control or sabotage proof.
- [ ] Test-name list diffed against `HEAD` after bulk edits.
- [ ] `cargo nextest run --workspace` green; `cargo fmt --check` and
      `cargo clippy --all-targets --workspace -- -D warnings` clean.
- [ ] Tier table written, per criterion, distinguishing *exercised* from
      *published only* from *inherently unobservable*.
- [ ] Beads filed for every divergence and substrate gap.
- [ ] Worktree clean; no scx pin committed; nothing depends on uncommitted
      state.
