---
name: scxsim-shimify-scheduler
description: Bring an arbitrary sched_ext BPF scheduler up inside scx-sim (scxsim) — compile its real BPF as userspace C behind a wrapper, wire the Rust loader, prove behaviour with non-vacuous tests, and report honestly what is and is not covered.
---

# Shim-ifying a scheduler into scxsim

You are bringing a sched_ext scheduler (`scx/scheds/rust/scx_<name>/`) up
inside scxsim so its **real BPF logic executes** under simulation.

This skill was distilled immediately after `scx_layered` was brought up end to
end. **Five** wrappers are in the tree — `cosmos`, `lavd`, `mitosis`,
`simple`, `tickless` — auto-discovered by the Makefile from `*/wrapper.c`:

```make
SCHEDS := $(patsubst %/wrapper.c,%,$(wildcard */wrapper.c))
```

so a new directory containing a `wrapper.c` is all it takes to be built.
(`layered` is a sixth, but it lives on an unlanded branch; where this document
cites it, that is where to look.) Comparing across them is what separates the
general from the scheduler-specific. Everything in the trap catalogue cost
someone real time.

This skill is the reconciliation of two independently written versions — see
the closing note for what came from where.

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

## 2. The include-order mechanic — read this before reasoning about any compat branch

This single fact resolves most "why is this compat branch behaving like that"
questions, it is not obvious from reading any one file, and getting it wrong
produced a **false P0** in Aug 2026.

```
wrapper.c
  #include "sim_wrapper.h"
      └── #include <scx/common.bpf.h>
              └── common.bpf.h:1143  #include "compat.bpf.h"   <-- parsed HERE
  #undef  bpf_ksym_exists          <-- wrapper's own overrides run AFTER
  #define bpf_ksym_exists(sym) ...
  #include "<scheduler>.bpf.c"     <-- re-includes common.bpf.h; header
                                       guard already set, so it is SKIPPED
```

`compat.bpf.h` is fully parsed inside `sim_wrapper.h`, **before** any override
in `wrapper.c`. Therefore:

| compat construct | when `bpf_ksym_exists` binds | affected by a wrapper override? |
|---|---|---|
| `static inline` **function** (`__COMPAT_scx_bpf_cpu_curr`, `__COMPAT_scx_bpf_dsq_peek`, `scx_bpf_dsq_insert`, `scx_bpf_task_set_slice`, `scx_bpf_task_set_dsq_vtime`, `__COMPAT_scx_bpf_reenqueue_local_from_anywhere`, `__COMPAT_has_generic_reenq`, `scx_bpf_dsq_reenq`) | at parse time, inside `sim_wrapper.h` | **NO — immune** |
| **macro** (`__COMPAT_scx_bpf_cpu_node`, `__COMPAT_scx_bpf_*_node`, `__COMPAT_HAS_scx_bpf_select_cpu_and`, `scx_bpf_dsq_move*`, `__COMPAT_bpf_cpumask_populate`, …) | at the call site in the `.bpf.c` | **YES** |

Consequence: a wrapper-level `#define bpf_ksym_exists(sym) (0)` can look like
it disables everything while actually changing nothing, because the constructs
that scheduler happens to use are all inline functions. Empirically, `cosmos`
forces the macro to `1` and its built `.so` still carries **43 `GLOB_DAT`
relocations** — i.e. dozens of genuine runtime tests survived the override
untouched.

**Do not reason about this from the `#define`. Verify from the binary (§3.1).**

---

## 3. Phase 0 — the compile spike. Do this FIRST.

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

### 3.1 Verify what actually compiled — relocations, not `#define`s

Reading the source that was *supposed* to produce a behaviour is not evidence.
Read the binary.

```sh
objdump -R <sched>.so | grep GLOB_DAT     # genuine runtime test survived
objdump -R <sched>.so | grep JUMP_SLOT    # symbol is actually called
objdump -d <sched>.so                     # read the branch itself
```

Interpretation:

- **`GLOB_DAT` on the symbol** → the compiler emitted a real NULL test; the
  genuine capability check is in the binary.
- **`JUMP_SLOT` but no `GLOB_DAT`** → the test was folded to a constant.
  Someone overrode it, or the symbol is locally defined so the compiler proved
  it non-NULL (check for a `W`/`T` definition in the same `.so`).

A correctly-compiled genuine test looks like this:

```
<__COMPAT_scx_bpf_cpu_curr>:
  cmpq $0x0,0xad58(%rip)        # <scx_bpf_cpu_curr>   <-- runtime NULL test
  jne  4b0 <scx_bpf_cpu_curr@plt>                      <-- modern path
  call 320 <scx_bpf_cpu_rq@plt>                        <-- legacy fallback
```

Both branches present, plus a `cmpq` against the GOT slot, = genuine.

> **Gotcha that has bitten twice, and it applies to the spike above:**
> `nm -D --undefined-only` (and `nm -u`) **hide symbols that are defined
> locally** in the object. Grepping only undefined symbols makes a fallback
> look absent when it is sitting right there. Use plain `nm -D`, **plus**
> `objdump -R`, **plus** the disassembly. Never conclude from one of the three.

**Regression-check a shim change** — an inert change diffs to nothing:

```sh
for so in <out>/schedulers/*.so; do
  objdump -R "$so" | awk '/GLOB_DAT|JUMP_SLOT/{print $2, $3}' | sort \
    > /tmp/$(basename $so).before
done
# ... make your change, rebuild ...
diff /tmp/libscx_<name>.so.before /tmp/libscx_<name>.so.after
```
>
> **Find them by grep, not by linker:**
> ```bash
> grep -oE 'bpf_[a-z_0-9]+\(' $SRC/*.c | sed 's/.*://;s/($//' | sort -u
> ```
> Cross-check every hit against `sim_wrapper.h`, `lib/scxtest/overrides.h`,
> `scx_test_map.h` and your wrapper's own `#define`s. Anything unaccounted
> for must be macro-overridden in your `wrapper.c` before it is ever called.

---

## 4. Phase 1 — the per-scheduler decision table

These four differ **per scheduler**. Getting one wrong costs a day. Decide
each deliberately and write the reasoning in a comment.

### 4.1 Kernel capability: never fake `bpf_ksym_exists` — provide the symbol

`bpf_ksym_exists(sym)` is libbpf's `!!sym` on a `__weak` symbol. `__weak`
survives into this native build (`lib/scxtest/scx_test.h:6` maps it to
`__attribute__((weak))`), so it is a **genuine runtime NULL check** against
what the process actually provides:

- symbols the simulator binary exports — `#[no_mangle]` in `kfuncs.rs`,
  resolved into the `.so` at `dlopen` time via `-rdynamic`; plus
- symbols the scheduler's own `wrapper.c` defines.

**That set IS the capability table.** It is ground truth by construction and
cannot drift the way a hand-maintained list would. Inspect it:

```sh
nm -D --defined-only target/debug/scxsim | awk '{print $3}' | sort -u
```

#### The rule

> To make a modern path run, **provide the symbol** — do not fake the test.

Export the kfunc from `kfuncs.rs`, or define it in that scheduler's
`wrapper.c`. The genuine test then answers TRUE by itself, and *keeps*
answering correctly as upstream moves.

`cosmos/wrapper.c` is the worked example: it needs
`__COMPAT_scx_bpf_cpu_node()` to take the modern branch, so it **defines
`scx_bpf_cpu_node()`** against its own `cpu_node_map`. The real test finds the
symbol and returns true — no lie required.

#### Why forcing the constant is wrong

`bpf_ksym_exists` is consulted for ~31 different symbols. A blanket `#define`
answers for **all of them at once** to fix the one that motivated it:

- Forcing `0` sends every macro-form capability down its legacy branch,
  including ones the simulator fully supports.
- Forcing `1` asserts that symbols the simulator does **not** export do exist.
  Each such call then jumps through a NULL weak `__ksym` and SIGSEGVs the
  moment its guard condition goes true.

Both are fake values standing in for a real capability test — the No-Stub
Rule's core prohibition. And an unexplained forced constant is
indistinguishable from a stub to the next reader.

Remember §2: because the inline-function compat constructs bind before any
wrapper override, a blanket force frequently does not even achieve the thing
it was written for. It is simultaneously too broad and ineffective.

#### What the existing wrappers do, and how to read it

| Scheduler | Choice | Read it as |
|---|---|---|
| mitosis | forces `0` | legacy, historical; only reaches macro-form constructs |
| cosmos | forces `1` | historical — but note cosmos *also* does the right thing by defining `scx_bpf_cpu_node()`, which is what actually makes its modern branch work |
| layered | **not forced** | the pattern to copy |

layered is the instructive case for why a blanket force cannot be right:
`scx_bpf_cpu_curr` and `scx_bpf_reenqueue_local___v2___compat` **are** exported
by scxsim (so the modern paths must run), while `scx_bpf_task_set_slice___new`
is **not** (so that one must fall back to the direct `p->scx.*` write). Forcing
`1` makes the second group jump through a NULL weak symbol; forcing `0` makes
the first group take a dead fallback (`scx_bpf_reenqueue_local___v1` is NULL →
SIGSEGV). No single constant is correct for both groups — which is the general
case, not a layered quirk.

**Default: leave it alone.** If you genuinely need an override, make it **per
symbol**, never blanket, and put the reason in the code next to it, stating
what breaks without it. A reader must be able to tell your override from a stub
without running `git log`. Then confirm from the binary (§3.1) that you changed
what you thought you changed.

### 4.2 `cleanup.bpf.h` RAII — native or neutralised?

`__free(...)`, `no_free_ptr()`, `scoped_guard()`, `DEFINE_GUARD` are
`__attribute__((cleanup))`, which **works natively in userspace C**.

* **layered:** works natively. Nothing neutralised. Destructors really run.
* **mitosis:** neutralises `__free`, `bpf_cgroup_acquire/release`,
  `bpf_kptr_xchg`, RCU locks, and hand-rolls `bpf_iter_css_*`.

Prefer native — it executes the scheduler's real resource handling. Only
neutralise a specific macro when its destructor calls something scxsim cannot
provide, and say which one in the comment. Blanket-neutralising because
mitosis did is how you silently disable a scheduler's cleanup paths.

### 4.3 Source patching — assume NOT needed

* **cosmos:** needs a `sed`'d `main.bpf.c` (BPF division-by-zero returns 0;
  native C raises SIGFPE) — generated by a rule in `config.mk`.
* **layered:** zero patching.

Patching is a last resort and a documented divergence. Try unpatched first.
If you do patch, generate it from the pristine source in `config.mk` (never
edit the submodule) and comment exactly which BPF-vs-C semantic forced it.

### 4.4 `bpf_for_each(scx_dsq, ...)`

The iterator uses a `cleanup()` destructor, so macro rewrites are not enough
— it needs **concrete symbols**. Copy the three-function block
(`bpf_iter_scx_dsq_new/next/destroy`) from `schedulers/layered/wrapper.c`.

---

## 5. Phase 2 — the wrapper

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
4. **helper macro overrides** (§3 warning box)
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

### Config globals: prefer DISABLING to ENABLING

Set from a `<sched>_setup()` the engine calls before `<sched>_init()`.

> Prefer *disabling* what the engine cannot model (`no_freq_scaling = true`)
> over *enabling* a feature it cannot really support.

Enabling one means the scheduler's genuine "feature off" path never runs, and
whatever the substrate feeds it is a fabricated input. That is precisely how
the cosmos PMU violation in §0 came about: `perf_config = 1` forced the
PMU-enabled branch, so the honest no-PMU path became unreachable.
`lavd/wrapper.c` is the model to copy — it *declares* its limits
(`is_smt_active = false`, `nr_llcs = 1`) instead of hiding them.

### Keep `sim_wrapper.h` authoritative

Anything every scheduler needs belongs there, not copy-pasted per wrapper.
Three wrappers answering the same question three different ways is what made
the Aug 2026 capability confusion possible in the first place.

### Add read-only probes

Export `<name>_probe_*` functions reading real scheduler state
(`task_ctx.layer_id`, per-CPU stat arrays, published topology). Tests must
assert on what the scheduler *decided*, never on a re-derivation. Wrap them in
Rust in `unsafe_impl/probes.rs` (`LayeredProbes` is the model).

Also export an **enum ABI probe** returning the scheduler's `intf.h` enum
values, and assert your Rust mirror matches. An upstream reordering then fails
a test instead of silently mis-configuring everything.

---

## 6. Phase 3 — Rust side

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

## 7. Phase 4 — tests, and the proof standard

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

## 8. Phase 5 — report honestly, in tiers

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

## 9. Trap catalogue

1. **`bpf_helper_defs.h` helpers link cleanly and SIGSEGV when called.** §3.
2. **Never fake `bpf_ksym_exists` — provide the symbol instead.** §4.1.
3. **Static arrays back the maps deliberately** — `reallocarray()` dangles
   held pointers. §5.
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

## 10. Portability: you do NOT need a build-time fetch

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

## 11. Commit discipline — this failed twice in one day

Two agents were each found holding substantial, finished-or-nearly-finished
work **uncommitted** in their worktrees: one with ~499 lines of Tier-3
implementation, one with a three-file P0 No-Stub fix. Neither was careless.
Both were waiting for the work to feel "finished", and "finished" kept
receding.

Uncommitted work in a worktree is **the highest loss-risk state in this
setup**. A worktree cleanup or machine loss destroys it with no recovery, and
— worse — a branch-level inventory cannot even see it, so nobody knows it is
at risk. Committed work on a branch survives worktree removal, because refs
and objects live in the shared git dir.

> **RULE: commit early and often on your own branch. WIP messages are fine.
> Commit BEFORE any long-running measurement or build.**
>
> Committing is not a claim that the work is done — *pushing* and *closing*
> are. It costs nothing and has already nearly cost us twice.

**Corollary for the orchestrator, stated here so agents can hold them to it:**
do not leave an agent running for long stretches without asking whether
anything is uncommitted.

---

## 12. Definition of done

- [ ] Compile spike run; gap list recorded before any wrapper was written.
- [ ] Every `bpf_*` helper the scheduler calls is accounted for **by grep**,
      not only by `nm -u`.
- [ ] Each §4 decision made deliberately, with the reasoning in a comment.
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
- [ ] Nothing of value left uncommitted at any pause point (§11).
- [ ] Every claim about a compat branch confirmed from the binary (§3.1), not
      from the `#define` that was supposed to produce it.

---

## Provenance

Reconciled from two independently written versions (the coordinator
commissioned the skill twice; the second author correctly found no existing
file because the first lived on an unlanded branch).

**From the process/traps version** (written by the agent that shim-ified
`scx_layered` end to end): the No-Stub Rule leading §0 with the forbidden
shapes ranked by how convincing they look and both category-1 failures written
as recognisable shapes; the compile spike as the highest-leverage first step;
the `bpf_helper_defs.h` raw-helper-number trap; the per-scheduler decision
table (RAII, source patching, DSQ iterators); the wrapper/Rust/test/reporting
phases; the proof standard (negative control and sabotage); the tier framing;
the 11-entry trap catalogue; the portability finding.

**From the capability version:** §2 in its entirety — the include-order
mechanic, `common.bpf.h:1143`, and the inline-function-vs-macro table that
explains why a wrapper override often changes nothing; §3.1 in its entirety —
the `GLOB_DAT` vs `JUMP_SLOT` relocation method, the disassembly signature of
a genuine test, the `nm --undefined-only` gotcha, and the before/after
relocation diff; the §4.1 rule *provide the symbol, do not fake the test* with
cosmos's `scx_bpf_cpu_node()` as the worked example and the ~31-symbol
blast-radius argument; the config-globals prefer-disabling rule; and
`sim_wrapper.h` authoritative.

Corrected during reconciliation: the claim that six wrappers are in the tree —
there are **five**, with `layered` on an unlanded branch. Verified against
`integration`. The two versions' `bpf_ksym_exists` guidance also disagreed in
emphasis; the capability version's rule is stronger and now leads, with the
per-wrapper table retained as historical reading rather than as a
recommendation.
