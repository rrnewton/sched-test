# scx_layered Tier 3 — handoff

**For:** the next agent (codex 5.6) picking this up cold.
**From:** tg `layered-tier3-drive`, 2026-08-12.
**Branch:** `feat/layered-support` in `worktrees/layered`. Not pushed.
**scx submodule:** pinned at the committed gitlink `59c30bae`. Do **not**
commit a pin bump.

Read `ai_docs/LAYERED_SUPPORT.md` first — it covers Tier 1/2, the wrapper
architecture, and the documented divergences. This document covers only
Tier 3 and the conventions you must not break.

---

## 1. Where things stand

Tier 2 is done and audited (see the *Tier-2 audit* table in
`LAYERED_SUPPORT.md`). Tier 3 is one commit in.

| Commit | Contents |
|---|---|
| `e3e2277` | substrate: `ops.yield`/`set_weight`/`disable`, jiffies, 2 kfuncs |
| `633dca9` | scx_layered as the 6th scheduler |
| `1fe2cea` | Tier-2 completion: cgroup matching, antistall, dump, tp_btf |
| `f9d3f12` | antistall + dump tests made non-vacuous |
| `12d4157` | LLC topology proven to reach DSQ selection |
| `e012423` | Tier-2 audit recorded; SMT gap filed |
| `9052aec` | **Tier 3 step 1:** scx_layered's real allocator compiled in |

Suite: **1117 tests pass, 14 skipped.** `cargo fmt --check` and
`cargo clippy --all-targets --workspace -D warnings` clean. Worktree clean.

`validate.sh` exits 1 at the mypy gate on a **pre-existing** bug unrelated to
this work: `scripts/typecheck.sh` resolves `mypy` from `PATH`
(`~/.local/bin/mypy`) but `pip` from `.venv`, so stubs land where mypy cannot
see them. Fixed on main by `f0acf58`, which is not an ancestor of this branch.
`.venv/bin/mypy --strict` passes. Do not "fix" it here — you will conflict on
rebase.

---

## 2. The Tier-3 design, and the one decision that matters

Tier 3 = model the userspace control loop that continuously re-allocates CPUs
between layers.

**The decision already made: run scx_layered's REAL allocator, do not
re-implement it.** CPU allocation between layers is scx_layered's *policy*,
not glue. A re-implementation would be a fake approximation — same interface,
plausible numbers, silently divergent exactly where the allocator is
interesting. `scx-sim/CLAUDE.md` forbids that.

This is already done (`9052aec`):

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

The loop itself. In production it is `main.rs::refresh_cpumasks()`
(line ~3739). Its shape:

```
  measure per-layer utilisation
      -> calc_raw_demands()            (main.rs:3202) -> Vec<LayerDemand>
      -> unified_alloc()               (alloc.rs:366) -> Vec<LayerAlloc>   [DONE: linked]
      -> grow/shrink each layer's cpumask per node
      -> update_bpf_layer_cpumask()    sets layer->cpus/nr_cpus/refresh_cpus
      -> BPF_PROG_RUN refresh_layer_cpumasks
      -> refresh_node_ctx() per node
```

Everything after `unified_alloc` **already exists in our wrapper** — it is
exactly what `layered_init()` does once at startup today
(`schedulers/layered/wrapper.c`: `layered_publish_layer_cpus`,
`refresh_layer_cpumasks(NULL)`, the `refresh_node_ctx` loop). Making it
periodic is largely a matter of extracting that tail into a callable
`layered_reallocate()` and driving it.

So the remaining work is three pieces:

**(a) A periodic userspace hook in the engine** — the substrate filed as
**mb sim-lqyu9**. A `Scenario`-level hook that runs a Rust closure at a
configured period during the simulation with access to the loaded
`DynamicScheduler`. Model it on the existing timed-event plumbing:
`Scenario::task_rename` (added in `1fe2cea`) is the smallest complete
worked example — `TaskRenameEvent` struct, builder method, `EventKind`
variant, seeding loop in `run_internal`, per-CPU-clock match arm, and a
handler. Copy that shape.

Keep the boundary honest: the closure plays **userspace**. It may read
scheduler state and write scheduler *config*. It must not make scheduling
decisions — those stay in the BPF.

**(b) Utilisation measurement.** `calc_raw_demands` needs per-layer CPU
usage. layered already maintains it in `cpu_ctx.layer_usages[layer]
[NR_LAYER_USAGES]`, summed across CPUs. Add a probe next to the existing
`layered_probe_layer_stat` (`wrapper.c`) — the pattern is already there.
This is real measured data from the scheduler's own counters, which is what
makes the loop faithful rather than synthetic.

**(c) The glue between (b) and `unified_alloc`.** `calc_raw_demands` itself
(main.rs:3202) is ~65 lines and depends on main.rs state, so it will need
reproducing rather than linking. **Flag this honestly in the commit** — it is
the one genuinely re-implemented piece, and it should be kept as thin as
possible, with the policy left in `unified_alloc`.

### Suggested first Tier-3 increment

Do not attempt the whole loop at once. The smallest thing that is genuinely
Tier 3 and provable:

> Two layers, one busy and one idle, on a fixed CPU count. After N control
> iterations, the busy layer's `nr_cpus` must have **grown** and the idle
> layer's **shrunk**, relative to the static allocation.

Prove it with a negative control: the identical workload with the control
loop disabled must leave both layers at their initial allocation. Without
that arm the test proves nothing (see §4).

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

8. **The engine has no NUMA concept at all.** `nr_numa_nodes` is a
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
- **Do not push** — the feature branch name needs review first.
- Do not self-close the tg task.
- File beads for out-of-scope findings. Open ones from this work:
  `sim-lqyu9` (userspace control-loop substrate — the enabler for (a)),
  `sim-u4the` (SMT placement effect untested), `sim-pf571` (cosmos still
  hand-writes `scx_pmu_*`), `sim-35uta` (thread groups).

---

## 6. Orientation: files you will touch

| Path | Role |
|---|---|
| `schedulers/layered/wrapper.c` | plays scx_layered's userspace; publishes topology + layers, exports probes. ~1500 lines. |
| `crates/scx_simulator/src/safe/layered.rs` | `LayerSpec`/`LayerMatch`/`LayerKind` — the layer-config API. |
| `crates/scx_simulator/src/safe/layered_alloc.rs` | vendored `largest_remainder` + provenance. |
| `crates/scx_simulator/src/safe/mod.rs` | declares `layered_alloc_upstream` via `#[path]`. |
| `crates/scx_simulator/src/unsafe_impl/ffi.rs` | `DynamicScheduler::layered*` constructors and config entry points. |
| `crates/scx_simulator/src/unsafe_impl/probes.rs` | `LayeredProbes` — read-only scheduler state. |
| `crates/scx_simulator/src/safe/engine.rs` | event loop; where a periodic hook goes. |
| `crates/scx_simulator/tests/layered.rs` | 27 Tier-1/2 tests. |
| `crates/scx_simulator/tests/layered_alloc.rs` | allocator drift guard + entry-point tests. |

Build: `cargo build --workspace`. Test: `cargo nextest run --workspace`.
Full gate: `./validate.sh` (see the mypy caveat in §1).
