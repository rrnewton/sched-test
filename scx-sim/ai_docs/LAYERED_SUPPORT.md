# scx_layered support in scxsim

Status: **supported** (Tier 2 — multi-layer on real topology), with one
documented asterisk: SMT topology is published and verified correct, but its
effect on placement is not behaviourally tested (mb sim-u4the). Every other
Tier-2 criterion is covered by a test that fails when the behaviour breaks —
see *Tier-2 audit* at the end.
Added by tg `layered-support-implement`, 2026-08-12.

`scx_layered` is the sixth scheduler scxsim runs, after `simple`, `lavd`,
`mitosis`, `cosmos` and `tickless`.

## What runs

The whole of scx_layered's BPF is compiled into `libscx_layered.so` and
executes unmodified:

| Translation unit | Lines | Notes |
|---|---|---|
| `scx_layered/src/bpf/main.bpf.c` | 4769 | all 18 struct_ops, both tp_btf progs, 3 syscall progs |
| `scx_layered/src/bpf/util.bpf.c` | 213 | cgroup path formatting, prefix/suffix/substring matching |
| `scx_layered/src/bpf/timer.bpf.c` | 105 | the antistall timer |
| `scx/lib/pmu.bpf.c` | 305 | the real PMU library (see *No stubs* below) |

No source patching is needed — unlike `cosmos`, which requires a `sed`'d
`main.bpf.c` to guard a division. The BPF compiles as userspace C with the
existing `sim_wrapper.h` include set plus `-Dconst=`, and `cleanup.bpf.h`'s
RAII macros (`__free`, `no_free_ptr`, `scoped_guard`) work natively, so
unlike the mitosis wrapper nothing has to be neutralised.

## Architecture: the wrapper is userspace

In production, scx_layered is driven by ~14.7k lines of Rust
(`main.rs`, `alloc.rs`, `layer_core_growth.rs`) that compute topology
tables, layer specifications and a continuously re-evaluated CPU
allocation, and publish them into BPF rodata/bss/maps.
`schedulers/layered/wrapper.c` plays exactly that role and nothing more.
Every scheduling decision is made by the real BPF.

The wrapper publishes:

- **Topology** — `cpu_llc_id_map`, `llc_numa_id_map`, `numa_cpumasks`,
  `__sibling_cpu`, `all_cpus`, `nr_llcs` / `nr_nodes` / `smt_enabled`, and
  the CPU / LLC / node proximity maps. The CPU proximity map reproduces
  `main.rs::init_cpu_prox_map()`'s `radiate` / `radiate_cpu` distance sort
  exactly (stable insertion sort, same keys), so layered's idle-CPU search
  walks the same order it would on real hardware.
- **Layers** — kind, preempt, exclusive, weight, slice, growth algorithm and
  match rules; the weight-ordered `layer_iteration_order`; the per-kind
  layer counts (`nr_op_layers` etc.); and the per-CPU layer scan orders.
- **Maps** — 15 of them.

The Rust entry points are `DynamicScheduler::layered`,
`::layered_with_topology`, `::layered_layers` and `::layered_set_antistall`,
with layer specs built from `LayerSpec` / `LayerMatch` / `LayerKind` in
`safe/layered.rs`.

### Map backing: static arrays, and why that is the faithful choice

`ARRAY`, `PERCPU_ARRAY` and `TASK_STORAGE` maps get static C arrays; only
the genuinely sparse `HASH` maps go through the generic `scx_test_map`
registry.

This is not a shortcut. `scx_test_map` grows its value storage with
`reallocarray()`, so any pointer a scheduler holds across an insert
dangles — and scx_layered holds `struct task_ctx *` and `struct cpu_ctx *`
across nested lookups constantly. BPF array maps really are preallocated in
the kernel, so a fixed static array is the *more* faithful model. Same
reasoning as the mitosis wrapper.

## No stubs

Per `scx-sim/CLAUDE.md`'s No-Stub Rule:

- **`scx/lib/pmu.bpf.c` is compiled in**, not replaced by five hand-written
  `scx_pmu_*` bodies. (The cosmos wrapper currently hand-writes them — that
  is the "elided library" antipattern the rule forbids; migrating cosmos is
  filed as follow-up work.) Only the hardware primitive underneath,
  `bpf_perf_event_read_value()`, is simulator-supplied, and it honestly
  returns `-ENOENT`: scxsim models CPU time, not microarchitecture, so there
  is no memory-bandwidth counter to report. `membw_event` therefore stays 0
  — the same state as production run without `--membw-event`.
- **Unconfigurable match kinds are rejected, not silently accepted.**
  `layered_add_layer_match()` returns `-ENOTSUP` for the kinds scxsim cannot
  honestly drive, so a test cannot accidentally install a rule that never
  matches. See *Not covered* below.

## Substrate added along the way

All of it is scheduler-agnostic and reusable:

| Addition | Why |
|---|---|
| `ops.yield` / `ops.set_weight` / `ops.disable` | Three struct_ops the engine never delivered. Wired to the kernel's own call sites: `set_weight` right after `ops.enable` (`scx_enable_task()`), `disable` right before `ops.exit_task` (`scx_disable_task()`), `yield` from the on-CPU task after its runtime is accounted (`yield_task_scx()`). |
| `Phase::Yield` | The simulator had no `sched_yield()` primitive, so `ops.yield` would have been dead code. Round-trips through the rt-app bridge. |
| `p->scx.runnable_at` | Was left at 0 for the whole run, making every task look infinitely delayed to any scheduler that reads it. Now stamped in JIFFIES at `scx_runnable()` and cleared at `scx_running()`. |
| `kfuncs::CONFIG_HZ` / `ns_to_jiffies` / `sim_bpf_jiffies64` | Derived from `engine::TICK_INTERVAL_NS`, so `bpf_jiffies64()` and `runnable_at` cannot disagree with the ticks actually delivered. |
| `bpf_cpumask_full`, `bpf_task_acquire` | Two missing kfuncs. |
| `cgrp->kn->name` | Cgroups had no directory-entry name, so any scheduler reconstructing a cgroup path saw an empty string. Now set from `CgroupDef::name`. |
| `tp_btf` delivery | Generic optional `tp_cgroup_attach_task` / `tp_task_rename` hooks, resolved by symbol prefix like `futex_hook`. |
| `scx_bpf_dump_bstr` capture | Was a no-op that discarded every scheduler's `ops.dump` output, making a faulting dump and a no-op dump indistinguishable. Now formats the BPF `bstr` ABI (`%[-+ #0][width][l\|ll]{d,i,u,x,s,c}`) into a per-run buffer readable via `kfuncs::dump_buffer_take()`. Benefits every scheduler's dump path. |
| `Scenario::task_rename` | A `prctl(PR_SET_NAME)` event, so the rename tracepoint has something to deliver. |

### Two traps worth remembering

1. **`bpf_helper_defs.h` helpers link cleanly and SIGSEGV when called.**
   `bpf_strncmp`, `bpf_snprintf`, `bpf_probe_read_str`, `bpf_jiffies64` and
   `bpf_map_delete_elem` are declared as *static function pointers
   initialised to the raw helper number*. They never show up as undefined
   symbols, so a link-time audit says the scheduler is complete — and then
   the first call jumps to address 182. Every one must be macro-overridden.
2. **`bpf_ksym_exists()` must not be forced.** mitosis forces it to 0 and
   cosmos to 1; layered needs neither. Left as the real weak-symbol test,
   `scx_bpf_cpu_curr` and `scx_bpf_reenqueue_local___v2___compat` resolve
   (so the modern paths run) while `scx_bpf_task_set_slice___new` does not
   (so it falls back to the direct `p->scx.*` write scxsim supports).
   Forcing either value breaks one of the two groups.

### An engine bug scx_layered found

The first cut of the `ops.yield` plumbing applied the kernel's
slice-zeroing fallback whenever the callback returned false. That is wrong:
`yield_task_scx()` *discards* `ops.yield`'s return value for a plain
`sched_yield()` and only zeroes the slice when there is no `ops.yield` at
all. `layered_yield()` always returns false, so the engine would have
zeroed its slice behind its back. `Scheduler::task_yield` now returns
`Option<bool>`, where `None` means "no `ops.yield`, apply the fallback".

## Documented divergences

These are real gaps, filed rather than hidden.

1. **Static CPU allocation (the Tier-3 boundary).** Production re-runs
   `refresh_cpumasks()` on a timer, growing and shrinking each layer's CPU
   set from live utilisation. scxsim has no model for a userspace control
   loop, so the allocation is computed once before `ops.init` and held fixed
   for the run. Layer growth/shrink paths are therefore not exercised.
   Layers get every CPU when open, or a contiguous weight-proportional slice
   otherwise, unless a test pins them with `LayerSpec::with_cpus`.
   *Mitosis has the same class of gap with its userspace cell-control path.*
2. **NUMA is a harness-supplied grouping.** The engine models LLCs and SMT
   siblings but has no NUMA concept and no inter-node distance cost, so
   `nr_numa_nodes` groups LLCs purely so layered's cross-node code paths can
   run. Same shape as the existing `cosmos_with_numa` precedent.
3. **Per-CPU layer scan orders are a deterministic rotation** rather than
   production's `fastrand`-shuffled orders, and the LLC proximity maps are
   distance-ordered rather than randomised. `bpf_get_prandom_u32()` is
   deterministically 0 in scxsim, so only `prox_maps[0]` is ever selected
   anyway.
4. **Thread-group cgroup moves are not modelled.** The engine migrates one
   task at a time, so `tp_cgroup_attach_task` is always delivered with
   `threadgroup = false`. That is also the only safe value: the threadgroup
   path walks `leader->signal->thread_head`, and the simulated `task_struct`
   has no `signal`.

## Not covered

Match kinds rejected at the FFI boundary, with what each would need:

| Kind | Blocker |
|---|---|
| `MATCH_NSPID_EQUALS`, `MATCH_NS_EQUALS` | no pid-namespace chain on the simulated `task_struct` |
| `MATCH_USED_GPU_TID`, `MATCH_USED_GPU_PID` | no GPU; the `kprobe/nvidia_*` probes are never delivered |
| `MATCH_CGROUP_REGEX` | needs the userspace regex evaluator that populates `cgroup_match_bitmap` |
| `MATCH_AVG_RUNTIME`, `MATCH_HINT_EQUALS`, `MATCH_SYSTEM_CPU_UTIL_BELOW`, `MATCH_DSQ_INSERT_BELOW` | driven by userspace-computed EWMAs |

`MATCH_SCXCMD_JOIN` is wired on the C side (the `task_rename` tracepoint
parses embedded SCXCMDs) but is not exposed through `LayerMatch`, because
without the userspace join/leave protocol there is nothing to drive it.

## Tests

`crates/scx_simulator/tests/layered.rs` — 25 tests. Every scenario sets
`.detect_bpf_errors()`, so a `scx_bpf_error` (for instance layered's
"didn't match any layer") fails the test rather than passing silently.

- **ABI guard** — all 24 `LayerKind` / `LayerMatch` / `LayerGrowthAlgo`
  discriminants compared against what the compiled `.so` reports, so an
  upstream reordering of `enum layer_match_kind` fails a test instead of
  silently mis-configuring every layer.
- **Default config** — loads, `ops.init` succeeds, one catch-all layer owns
  every CPU, all tasks complete, oversubscription starves nobody.
- **Topology** — scheduler-observed LLC / node / sibling layout compared
  against the engine's; multi-LLC + NUMA + SMT run.
- **Matching** — comm prefix, negation, ANDed rules, OR alternatives,
  cgroup prefix / suffix / contains.
- **Confinement** — a confined layer's tasks never run off its CPUs; a task
  whose affinity excludes its layer's CPUs still makes progress via a
  fallback DSQ.
- **Callbacks** — `ops.yield` reaches layered and is counted in
  `LSTAT_YIELD_IGNORE`; `ops.disable` drops layer membership; `ops.dump`
  runs over a multi-layer, multi-LLC config.
- **Antistall** — the timer fires and re-arms; disabling it stops the
  re-arm after one fire; and, on a workload where a task's affinity excludes
  every CPU of its confined layer, antistall actually *consumes* the delayed
  DSQ (`GSTAT_ANTISTALL` > 0) with `--antistall-sec 0`, while the identical
  workload with a one-hour threshold leaves the counter at zero. The paired
  control is the point — without it the test would pass on a counter that
  increments unconditionally.
- **`ops.dump`** — asserts on the text layered actually emits (every layer
  name, both fallback DSQs, and no unformatted printf spec surviving), not
  merely that the dump does not fault.
- **tp_btf** — cgroup migration and task rename both re-layer the task, in
  both directions. All four were verified non-vacuous by disabling the
  delivery call and confirming they fail.
- **Determinism** — two identical runs produce byte-identical traces.

`layered` was also added to the cross-scheduler suites — `determinism`,
`per_cpu_isolation`, `scheduling_invariants`, `scheduler_comparison` and
`examples_matrix` — so it is held to the same general invariants as the
other five schedulers.

## Tier-2 audit

Asked for explicitly during review: for each Tier-2 criterion, is it
exercised by a test that would FAIL if the behaviour broke, or does it merely
compile and run?

| Criterion | Verdict | Evidence |
|---|---|---|
| Multi-layer | genuinely exercised | 4 matching tests assert specific per-task `layer_id`. Sabotaging `layered_add_layer_match()` to install no rules fails 9 tests. |
| LLC topology | genuinely exercised | `llc_topology_drives_dsq_selection`: tasks pinned into LLC 0 vs LLC 1 land on different DSQs (`0x40000000` / `0x40000001`); flat-topology control arm requires all on one DSQ. |
| NUMA topology | publication only, inherently | the engine has no NUMA concept and no distance cost, so there is no observable consequence to assert on. Documented divergence #2, not a closable test gap. |
| SMT topology | **publication only** | `__sibling_cpu[]` is asserted correct/-1, but no test shows SMT changing a placement decision. mb sim-u4the. |
| comm matching | genuinely exercised | see Multi-layer. |
| cgroup matching | genuinely exercised | `cgroup_prefix_match_routes_tasks_by_cgroup_path`, `cgroup_suffix_and_contains_match` drive the real `format_cgrp_path()` + `match_prefix_suffix()`/`match_substr()`; both fail under the matching sabotage. |
| antistall timer | genuinely exercised | `GSTAT_ANTISTALL` = 589 with `--antistall-sec 0`, 0 with `--antistall-sec 3600` on the identical workload. |
| `ops.dump` | genuinely exercised | asserts on emitted text; the "no surviving conversion spec" assertion caught a real formatter bug (`%+lldms`). |

Two tests were found overclaiming during this audit and corrected:
`topology_seen_by_scheduler_matches_the_engine` compared the wrapper against
a re-derivation of its own input rather than against the engine (renamed and
its comment corrected, with the behavioural test added alongside), and the
dump test's spec check originally looked for two hardcoded specs and would
have missed the bug it was written to catch.
