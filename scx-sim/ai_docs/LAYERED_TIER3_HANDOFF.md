# scx_layered Tier 3 — handoff

**For:** the next agent (codex 5.6) picking this up cold.
**From:** tg `layered-tier3-drive`, 2026-08-12.
**Status:** SUPERSEDED as a handoff — Tier 3 was completed by the original
agent after the codex handoff was aborted. Kept as the Tier-3 design record
and capability statement (§9 is the authoritative capability statement).
Sections describing "what remains" are historical unless marked otherwise.
**Branch:** `feat/layered-support`, landed into `integration` via PR #64.
**scx submodule:** pinned at the committed gitlink `59c30bae`. Do **not**
commit a pin bump.

Read `ai_docs/LAYERED_SUPPORT.md` first — it covers Tier 1/2, the wrapper
architecture, and the documented divergences. This document covers only
Tier 3 and the conventions you must not break.

---

## 1. Where things stand

Tier 2 is done and audited (see the *Tier-2 audit* table in
`LAYERED_SUPPORT.md`). Tier 3 now has its first behaviour-changing increment:
periodic live reallocation for a source-guarded one-LLC/no-SMT `Linear` subset.

| Commit | Contents |
|---|---|
| `c9786db` | substrate: `ops.yield`/`set_weight`/`disable`, jiffies, 2 kfuncs |
| `a50f61f` | scx_layered as the 6th scheduler |
| `7e489e6` | Tier-2 completion: cgroup matching, antistall, dump, tp_btf |
| `19cc392` | antistall + dump tests made non-vacuous |
| `7f6061c` | LLC topology proven to reach DSQ selection |
| `3c68cfd` | Tier-2 audit recorded; SMT gap filed |
| `2b23577` | **Tier 3 step 1:** scx_layered's real allocator compiled in |
| `8a1416a` | **Tier 3 step 2:** periodic control, measured usage, flat Linear reallocation, real BPF refresh |
| `9b60292` | one-LLC support fence lint cleanup |

Suite: **1129 tests pass, 14 skipped.** `./validate.sh` passes after the
rebase, including fmt, clippy with warnings denied, nextest, doc tests, stress
smoke tests, and `mypy --strict`. It reports only its standard optional skips:
e9-instrumented schedulers, e9 stress mode, and the absent release-only ASLR
binary.

---

### How far into Tier 3 this actually is

Be blunt with yourself about this, because the branch looks further along
than it is.

Full Tier 3 = the userspace CPU-reallocation control loop: ~2500 lines of
`alloc.rs` + ~1300 lines of `layer_core_growth.rs`, plus the periodic
`BPF_PROG_RUN` refresh cadence that applies their output.

| Piece | State |
|---|---|
| `alloc.rs` (~2500 lines) — the water-fill allocator | **linked and running**, with its own 80 tests |
| `layer_core_growth.rs` (~1300 lines) — growth algorithms / core ordering | **linked and running**, compiled verbatim via the `scx_layered_growth` crate. 12 of 16 `LayerGrowthAlgo` variants execute upstream's real ordering; the 3 `CpuSetSpread*` and multi-LLC `StickyDynamic` are refused. Unlike `alloc.rs`, upstream's own 43 tests in this file do NOT run — the crate shims the whole `scx_utils` namespace, so they cannot compile. |
| Periodic control hook in the engine | **done.** Generic optional scheduler userspace event; no events for schedulers that do not opt in. |
| Utilisation measurement feeding the loop | **done for owned/open CPU time.** Reads cumulative real `cpu_ctx.layer_usages` and applies production's 100ms EWMA shape. |
| Target/demand glue | **done for the narrow subset.** util band, cpus range, shrink dampening, single-node demand into real `unified_alloc`. Peak util, membw and pinned-util priority remain. |
| Driving BPF reallocation during a run | **done.** Changed masks are serialized, then real `refresh_layer_cpumasks` and `refresh_node_ctx` execute. |

The first observable Tier-3 behaviour now exists, but this is not full Tier
3. Describe it as "flat Linear Tier-3 control landed; topology-aware growth
and advanced sizing remain." The largest remaining policy boundary is full
`layer_core_growth.rs` integration; NUMA behavioural modelling is explicitly
outside this Tier-3 task.

## 2. The Tier-3 design, and the one decision that matters

Tier 3 = model the userspace control loop that continuously re-allocates CPUs
between layers.

**The decision already made: run scx_layered's REAL allocator, do not
re-implement it.** CPU allocation between layers is scx_layered's *policy*,
not glue. A re-implementation would be a fake approximation — same interface,
plausible numbers, silently divergent exactly where the allocator is
interesting. `scx-sim/CLAUDE.md` forbids that.

This is already done (`2b23577`):

- `safe/layered_alloc_upstream` **is** `scx_layered/src/alloc.rs`, compiled
  verbatim via `#[path]` from `safe/mod.rs`.
  - `#[path]`, not `include!`, because the upstream file opens with `//!`
    inner doc comments, legal only at the top of a module.
  - Declared from `mod.rs` so the path resolves relative to `safe/` and not
    to a `layered_alloc/` subdirectory that does not exist.
- Its ~80 upstream unit tests now run in our suite. That is the load-bearing
  evidence that it is genuinely the real allocator and genuinely executing.
- `safe/layered_alloc.rs` holds the single vendored helper,
  `largest_remainder` (it lives in upstream's `lib.rs` next to the BPF
  skeleton, which scxsim cannot build), plus the provenance docs.
- `tests/layered_alloc.rs` re-reads upstream's `lib.rs` at test time and
  asserts the vendored copy is still token-identical.

**Do not hand-edit the vendored helper.** If the drift guard fails after a
submodule bump, re-copy upstream's body programmatically. The guard caught a
hand-transcription error (`for &i` → `for &idx`) on its very first run.

### What remains

Production's `main.rs::refresh_cpumasks()` (line ~3739) has this shape:

```
  measure per-layer utilisation
      -> calc_raw_demands()            (main.rs:3202) -> Vec<LayerDemand>
      -> unified_alloc()               (alloc.rs:366) -> Vec<LayerAlloc>
      -> grow/shrink each layer's cpumask per node
      -> update_bpf_layer_cpumask()    sets layer->cpus/nr_cpus/refresh_cpus
      -> BPF_PROG_RUN refresh_layer_cpumasks
      -> refresh_node_ctx() per node
```

The narrow implementation now follows that full chain. The generic engine
event runs every configured period; `layered_probe_layer_usage` reads the
real counters; `LayeredControl` computes targets and calls real
`unified_alloc`; `layered_apply_layer_cpumasks` publishes only changed masks
and runs the real syscall programs.

The next increments, in priority order:

1. ~~Integrate upstream `layer_core_growth.rs`~~ **DONE** — linked verbatim
   via the `scx_layered_growth` crate; Reverse/Topo/RoundRobin/Sticky and
   topology-aware selection all execute upstream's own code. What remains is
   getting upstream's 43 in-file tests to run, which needs the crate to stop
   shimming `scx_utils`.
2. Add pinned-util demand priority. On one node it matters when demand
   exceeds capacity, even though there is no placement choice between nodes.
3. Add optional peak-util and memory-bandwidth sizing only when their real
   inputs exist. Do not synthesize PMU values.
4. Consider SMT allocation units after the real growth module is linked.

NUMA behaviour is not on this list. It belongs to the separate S692395
substrate goal (`xnuma_gate`, `xnuma_gate_charge`, `xnuma_bucket_refill`).

### Behavioural proof now landed

The identical busy/idle workload stays at `(2, 2)` with control disabled and
reaches `(3, 1)` with control enabled. The test also compares the serialized
mask against the real BPF kptr cpumask. Sabotaging only periodic
`refresh_layer_cpumasks` leaves serialized `(3, 1)` but BPF `(2, 2)`, and the
test fails. This is stronger than checking `layer->nr_cpus`, which would pass
even if BPF never consumed the update.

---

## 3. Non-obvious things that will cost you a day each

These are the traps already paid for. None are guessable from the code.

1. **`bpf_helper_defs.h` helpers link cleanly and SIGSEGV when called.**
   `bpf_strncmp`, `bpf_snprintf`, `bpf_probe_read_str`, `bpf_jiffies64`,
   `bpf_map_delete_elem` are declared as *static function pointers
   initialised to the raw helper number*. They never appear as undefined
   symbols — a link-time audit says the scheduler is complete — and then the
   first call jumps to address 182. Any new helper the scheduler reaches
   must be macro-overridden in `wrapper.c`.

2. **`bpf_ksym_exists()` must NOT be forced either way.** mitosis forces it
   to 0, cosmos to 1; layered needs neither. Left as the real weak-symbol
   test, `scx_bpf_cpu_curr` and `scx_bpf_reenqueue_local___v2___compat`
   resolve (modern paths run) while `scx_bpf_task_set_slice___new` does not
   (falls back to the direct `p->scx.*` write scxsim supports). Forcing
   either value breaks one of the two groups.

3. **Static arrays back the BPF maps, deliberately.** `scx_test_map` grows
   its value storage with `reallocarray()`, so any pointer the scheduler
   holds across an insert dangles — and layered holds `task_ctx *` /
   `cpu_ctx *` across nested lookups constantly. BPF array maps really are
   preallocated, so static arrays are the *more* faithful model. Only the
   genuinely sparse HASH maps use the registry.

4. **`struct layer` is ~10 MB, so `layers[]` is ~165 MB of BSS.**
   Never `memset` the whole array — `layered_reset_layers_internal()` clears
   only the scalar tail past `matches`, and each `struct layer_match` is
   zeroed individually when configured. A full memset touches every page.

5. **A catch-all layer needs `nr_match_ors` set explicitly.** An OR group
   with zero AND rules matches everything, but has no match call to grow the
   count, so `layered_set_layer_nr_match_ors()` publishes it. Miss this and
   every task fails `maybe_refresh_layer()` with "didn't match any layer".

6. **`LayeredProbes` holds raw fn pointers into the `.so`.** Keep the
   `Simulator` alive (`let sim = Simulator::new(sched); let t = sim.run(..)`)
   or dropping it `dlclose`s the library and the probes dangle — SIGSEGV
   *after* the run reports success.

7. **`ops.yield`'s return value is discarded for a plain `sched_yield()`.**
   `yield_task_scx()` only zeroes the slice when there is *no* `ops.yield`.
   `Scheduler::task_yield` returns `Option<bool>`; `None` means "absent,
   apply the fallback". layered always returns `false`, so keying the
   fallback on the return would zero its slice behind its back.

8. **`scx_bpf_dump_bstr` used to be a no-op, and `ops.dump` was therefore
   untestable.** It discarded every scheduler's dump output, so a dump that
   faulted and a dump that did nothing looked identical, and the wrapper's
   own `bpf_snprintf` was entirely unverified. It now formats the BPF `bstr`
   ABI (`%[-+ #0][width][l|ll]{d,i,u,x,s,c}`) into a per-run thread-local
   buffer, cleared by the engine at run start and readable via
   `kfuncs::dump_buffer_take()`. This is substrate — it makes `ops.dump`
   testable for *every* scheduler, not just layered. Its "no surviving
   conversion spec" assertion in `tests/layered.rs` is what caught a missing
   `+` flag in the formatter.

9. **The antistall negative-control pattern — copy it.** Proving a watchdog
   fires is easy to fake. The pattern that works: run the SAME workload
   twice, once with the threshold set so the mechanism must engage and once
   with it set so it must not, and assert the counter is non-zero in the
   first and exactly zero in the second. Here that is
   `--antistall-sec 0` -> `GSTAT_ANTISTALL` = 589 versus
   `--antistall-sec 3600` -> 0. The second arm is the whole test: without it
   you are asserting on a counter that might increment unconditionally. Use
   the same shape for the control loop (loop enabled vs disabled).

10. **The engine has no NUMA concept at all.** `nr_numa_nodes` is a
   harness-supplied grouping over LLCs with no distance cost. Tier 3's
   per-node allocation will therefore exercise `unified_alloc`'s multi-node
   paths without any simulated consequence to the placement. Say so; do not
   imply NUMA fidelity.

---

## 4. Standards that carry forward — non-negotiable

- **No-Stub Rule.** scxsim models the kernel; the scheduler models the
  scheduler. Never stub, no-op, elide a library, or re-implement
  scheduler-owned logic in Rust/C. If something cannot run, that is a
  substrate task — file it, do not fake it.
- **"A passing test that would pass anyway proves nothing."** Every
  behavioural claim needs a negative control or a sabotage check. Worked
  examples in this branch:
  - tp_btf: disable the delivery call, confirm the 4 tests fail.
  - layer matching: make `layered_add_layer_match` install no rules, confirm
    9 tests fail.
  - antistall: `--antistall-sec 0` fires (589), `3600` does not (0), same
    workload.
  - LLC: two-LLC run splits DSQs, flat-topology control does not.
  - dump: assert on emitted text; "no surviving conversion spec" caught a
    real formatter bug.
- **Report the near-misses.** Two tests in this branch were found
  overclaiming *by their author* — a topology test that compared the wrapper
  against a re-derivation of its own input, and a dump assertion that checked
  two hardcoded specs and would have missed the bug it was written for. Both
  are recorded in `LAYERED_SUPPORT.md`. Do the same.
- **Watch the test count, not just the colour.** A scripted edit silently
  deleted four tests here; "all tests pass" stayed true throughout. The only
  signal was the count dropping. Diff the test-name list against `HEAD` after
  any bulk edit.

---

## 5. Constraints

- Work only in `worktrees/layered`. Never write in `sched-test1`.
- Do **not** commit an scx submodule pin.
- No host-specific absolute paths in committed files.
- ~~Do not push~~ — superseded: under the owner's standing policy every agent
  owns its branch through to a landed PR.
- Do not self-close the tg task.
- File beads for out-of-scope findings. Relevant ones from this work:
  `sim-lqyu9` (userspace control-loop substrate — implemented by step 2),
  `sim-juru9` (full real `layer_core_growth` integration),
  `sim-u4the` (SMT placement effect untested), `sim-pf571` (cosmos still
  hand-writes `scx_pmu_*`), `sim-35uta` (thread groups).

---

## 6. Orientation: files you will touch

| Path | Role |
|---|---|
| `schedulers/layered/wrapper.c` | plays scx_layered's userspace; publishes topology + layers, exports probes. ~2000 lines. |
| `crates/scx_simulator/src/safe/layered.rs` | `LayerSpec`/`LayerMatch`/`LayerKind` — the layer-config API. |
| `crates/scx_simulator/src/safe/layered_alloc.rs` | vendored `largest_remainder` + provenance. |
| `crates/scx_simulator/src/safe/mod.rs` | declares `layered_alloc_upstream` via `#[path]`. |
| `crates/scx_simulator/src/unsafe_impl/ffi.rs` | `DynamicScheduler::layered*` constructors and config entry points. |
| `crates/scx_simulator/src/unsafe_impl/probes.rs` | `LayeredProbes` — read-only scheduler state. |
| `crates/scx_simulator/src/safe/engine.rs` | event loop; where a periodic hook goes. |
| `crates/scx_simulator/tests/layered.rs` | 35 Tier-1/2/3 tests. |
| `crates/scx_simulator/tests/layered_alloc.rs` | allocator drift guard + entry-point tests. |

Build: `cargo build --workspace`. Test: `cargo nextest run --workspace`.
Full gate: `./validate.sh`.

---

## 7. Things that did NOT work — do not rediscover these

The most expensive knowledge to re-derive. Each cost real time.

1. **`include!`ing `alloc.rs` fails.** The upstream file opens with `//!`
   inner doc comments, legal only at the top of a module, so `include!`
   produces a wall of `error[E0753]: expected outer doc comment`. Use
   `#[path = "..."] pub mod ...;` — and declare it from a `mod.rs`, because
   `#[path]` on a module declared inside `foo.rs` resolves relative to
   `foo/`, a directory that does not exist here.

2. **Depending on the whole `scx_layered` crate is a dead end.** Rejected by
   inspection, not tried: its `lib.rs` pulls in the generated BPF skeleton
   (`bpf_skel.rs` / `bpf_intf.rs`), needing a full BPF build (bpftool +
   clang BPF target) at scxsim build time, plus libbpf-rs, nvml-wrapper,
   fb_procfs and inotify. Compiling the pure algorithm modules directly is
   the only tractable route. This was originally why `layer_core_growth.rs`
   looked unlinkable — unlike `alloc.rs` it needs `scx_utils::Topology`,
   `CpuPool` and `bpf_intf`, all behind that wall. **Resolved:** the
   `scx_layered_growth` crate supplies those containers itself
   (`extern crate self as scx_utils`) and compiles the upstream file verbatim
   on top. The cost of that trick is that upstream's own tests in the file
   cannot compile against the shim.

3. **Hand-copying upstream code does not survive review.** The vendored
   `largest_remainder` was transcribed by hand and silently differed
   (`for &i` -> `for &idx`). Semantically identical; nothing would ever have
   failed. Splice programmatically, and keep the drift guard.

4. **A drift guard is only as good as its extractor.** The first version
   searched for `pub fn <name>` anywhere and matched the *doc-comment
   mention* in `layered_alloc.rs`, comparing documentation against code.
   Anchor at line start and give the extractor its own self-test — a broken
   extractor makes the guard vacuous.

5. **`--antistall-sec 0` was unreachable at first.** `layered_set_antistall`
   originally had `if (sec) antistall_sec = sec;`, so 0 silently kept the
   production default of 3 and the hot arm could never be reached without
   simulating multiple seconds of per-task delay. It now applies `sec`
   verbatim (production accepts 0 too).

6. **Do not build against scx pin `eba091e` from this branch.** The pin bump
   and the matching `schedulers/mitosis/wrapper.c` fix were separated: at
   `eba091e` upstream had removed `debug_events` / `DEBUG_EVENTS_BUF_SIZE`
   while the committed mitosis wrapper still referenced them, so the build
   dies on `mitosis_wrapper.o` before reaching layered. This branch builds
   against the committed pin `59c30bae`. `validate2` has since committed the
   fix as `db45c2c` and shown it backward-compatible with the old pin, so
   after rebasing onto a base containing it either pin works — but
   `db45c2c` is **not** an ancestor of this branch today.

7. **`rfind` + `s[:idx] + new` truncates files.** A scripted edit that
   spliced at the last match forgot to re-append the tail and silently
   deleted four tests. `cargo nextest` stayed green throughout; the only
   signal was the count dropping 1031 -> 1028. After any bulk scripted edit,
   diff the test-name list against `HEAD`.

8. **rustfmt rewrites string-literal line continuations.** A fixture using
   `"...pub fn \` + newline + indentation was reformatted into literal
   embedded spaces, breaking a matcher. Use `concat!()` for multi-line
   fixtures whose exact bytes matter.

---

## 8. NUMA — related SEV, and explicitly NOT Tier 3

**Do not conflate these.** Separate goals, separate substrate.

The layered NUMA SEV is **S692395**. It lives in the cross-NUMA gating path:

| Function | `main.bpf.c` |
|---|---|
| `xnuma_bucket_refill` | 1137 |
| `xnuma_gate` | 1170 |
| `xnuma_gate_charge` | 1201 |

Those are exactly the three functions the coverage run reported unreachable,
and the reason is structural rather than incidental: **the scxsim engine has
no NUMA concept at all** — no per-CPU node id, no inter-node distance, no
cost to a cross-node placement. `nr_numa_nodes` in the layered wrapper is a
harness-supplied grouping over LLCs that exists only so layered's multi-node
code paths can be entered; it has no simulated consequence.
`xnuma_gate` and friends implement a token-bucket rate limit on cross-node
migration, which cannot be meaningfully exercised until the engine models
nodes as something a task can be placed *badly* relative to.

Reaching those three functions therefore requires **engine NUMA substrate**,
a separate project from the Tier-3 control loop. Tier 3 can be completed
without them, and completing Tier 3 will not reach them. If reproducing
S692395 becomes the goal, file and size it as a NUMA-substrate task — do not
let it be absorbed into "finish Tier 3".

---

## 9. Tier-3 capability statement (completion)

Same shape as the Tier-2 audit: for each claim, is it exercised by a test that
would FAIL if the behaviour broke, or does it merely run?

### What the loop genuinely does

| Capability | Verdict | Proof |
|---|---|---|
| Periodic userspace CPU reallocation on a cadence | **sabotage-proven** | dropping the engine's periodic re-arm fails `userspace_control_reallocates_cpus_from_idle_to_busy_layer` |
| Runs upstream's REAL allocator | **sabotage-proven** | `alloc.rs` compiled verbatim; its ~80 upstream tests run in our suite; a vendored-helper drift guard compares against upstream at test time |
| Runs upstream's REAL core-growth ordering | **sabotage-proven** | collapsing every layer to layer 0's ordering fails `upstream_linear_and_reverse_choose_different_freed_cores` |
| Whole-core allocation under SMT | **sabotage-proven** | releasing half a core fails `smt_core_transfer_moves_whole_cores_only` |
| `growth_denied` correct and attributed to the right layer | **sabotage-proven, non-circularly** | swapping attribution between layers fails the asymmetric spec-oracle while the symmetric enabled-vs-disabled test passes |
| Serialized view and BPF kptr masks agree | exercised | both read and compared in the reallocation and SMT tests |

### What it approximates

- **`calc_raw_demands` is re-implemented**, not linked. It depends on
  `main.rs` state that cannot be compiled here. It is kept deliberately thin;
  the policy it feeds (`unified_alloc`) is upstream's real code.
- **Utilisation input** is an EWMA over `cpu_ctx.layer_usages` with upstream's
  100ms half-life, driven by the simulator's own runtime accounting rather
  than by real hardware counters.

### What it cannot do — loud, not silent

These abort with a clear message rather than degrading:

- **`CpuSetSpread` / `CpuSetSpreadReverse` / `CpuSetSpreadRandom`** — scxsim
  has no cgroup-cpuset substrate.
- **`StickyDynamic` on multiple LLCs** — needs production's runtime
  LLC-trading loop.
- **Resizing an explicitly pinned layer** (`with_cpus`) — refused by design.

Not on this list, because it is NOT a limitation: full `layer_core_growth`
runs. mb sim-juru9 tracked the old flat-Linear specialization and is closed by
this work.

### Known behavioural limitation, faithfully reproduced

With SMT, a layer at 2 cores whose target is 1 core **cannot shrink**:
CPU-space dampening (`4 - ceil(2/2) = 3`) then `div_ceil(au)` rounds back to
2 cores. This is upstream's behaviour, verified against
`main.rs::refresh_cpumasks()`, and is pinned by
`smt_allocation_hits_the_shrink_fixed_point` so a
simulator-side "fix" would fail loudly as a divergence. mb sim-klue5.

### Is `growth_denied` sufficient for the NUMA acceptance criterion?

**No — necessary but not sufficient.** The criterion is that
`xnuma_bucket_refill` and `xnuma_gate_charge` actually EXECUTE. Reading
`main.bpf.c::xnuma_gate()` (line 1170) and `main.rs::xnuma_check_active()`
(line 1872), three things are required and only one exists:

1. **`growth_denied` per (layer, node)** — DONE. It is condition 3 of the 3
   that make a node a migration source in `xnuma_check_active()`, and it is
   now exposed and proven correct.
2. **Userspace must WRITE the resulting rate** into
   `layers[].node[src].xnuma[dst].rate`. We never do — `grep xnuma` across
   `crates/` and `schedulers/layered/wrapper.c` returns nothing. The field is
   zeroed BSS, and `xnuma_gate()` treats `rate == 0` as "deny" and returns
   **before** reaching `xnuma_bucket_refill()`. So today the refill path is
   unreachable no matter what `growth_denied` says.
3. **Engine NUMA substrate**, so `src_nid != dst_nid` can ever hold.
   `xnuma_gate()`'s first guard returns early when they are equal, and
   `xnuma_gate_charge` only fires when a task starts on a different node than
   its previous CPU. This is `f5415f8` on `agent/deps` (`SimCpu.node_id`,
   `Scenario::cpus_per_node`, engine-owned `scx_bpf_cpu_node()`), which is
   **not on this branch**.

So the NUMA work is unblocked on the signal it was waiting for, but two
concrete pieces remain, in this order: land the engine NUMA substrate, then
add the userspace rate-write. Only then can a workload drive cross-node
migration and make the two functions execute. Neither is Tier-3 scope.
