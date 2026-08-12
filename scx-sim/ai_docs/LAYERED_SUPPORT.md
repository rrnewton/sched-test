# scx_layered support in scxsim

Status: **supported** at Tier 2 (multi-layer on real LLC/SMT topology) and
Tier 3 (the opt-in userspace CPU-reallocation control loop). Every Tier-2
criterion is covered by a test that fails when the behaviour breaks — see
*Tier-2 audit* at the end. The Tier-3 capability statement, including what
the loop approximates and what it refuses outright, is
`LAYERED_TIER3_HANDOFF.md` §9.

The control loop measures the real BPF usage counters, runs upstream's real
`alloc.rs` AND `layer_core_growth.rs` (both compiled verbatim from the scx
submodule), and executes the real BPF refresh programs. It runs on multi-LLC,
multi-node and SMT topologies.

Of `LayerGrowthAlgo`'s 16 variants, 4 are refused at enable time rather than
approximated: `CpuSetSpread`, `CpuSetSpreadReverse` and `CpuSetSpreadRandom`
always (they need cgroup-cpuset topology scxsim does not model), and
`StickyDynamic` when there is more than one LLC (it needs production's
runtime LLC-trading loop). The other 12 execute upstream's real ordering
code. Two of those 12 are degenerate rather than wrong: `BigLittle` and
`LittleBig` run, but the wrapper publishes a uniform machine
(`has_little_cores = false`, every `cpuc->is_big = false`), so there is no
asymmetry for them to sort on. The control loop also refuses to resize a
layer with an explicit cpuset, and requires a `util_range` on every non-open
layer.

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

In production, scx_layered is driven by ~9.9k lines of Rust across
`main.rs`, `alloc.rs` and `layer_core_growth.rs` (14.4k for the whole
`src/*.rs` tree) that compute topology
tables, layer specifications and a continuously re-evaluated CPU
allocation, and publish them into BPF rodata/bss/maps.
`schedulers/layered/wrapper.c` and `safe/layered_control.rs` play the
userspace role. Every scheduling decision is still made by the real BPF.
The engine's generic userspace-control event supplies the periodic cadence.

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
`safe/layered.rs`. `::layered_enable_control_loop` opts into periodic
reallocation and rejects unsupported topology/growth configurations.

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

- **The allocator is upstream's real `alloc.rs`.** It is compiled verbatim as
  `layered_alloc_upstream`; its ~80 upstream tests run in scxsim.

- **Core-growth ordering is upstream's real `layer_core_growth.rs`**, compiled
  verbatim via the `scx_layered_growth` crate. Note the asymmetry with
  `alloc.rs`: that crate supplies the module's dependencies by shimming the
  whole `scx_utils` namespace, so upstream's own 43 tests in that file do NOT
  run here and structurally cannot (see the comment in
  `crates/scx_layered_growth/Cargo.toml`). What backs it instead is
  differential behaviour testing —
  `upstream_linear_and_reverse_choose_different_freed_cores` distinguishes two
  real algorithms on a simulated topology, and a sabotage collapsing per-layer
  core ordering fails it. Growth modes that would need substrate we do not
  have are rejected rather than approximated; see the list at the top.

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
| `p->scx.runnable_at` | Was left at 0 for the whole run, making every task look delayed by the entire uptime to any scheduler that reads it. Now stamped in JIFFIES on the transition to runnable, and — as in the kernel — NOT cleared when the task starts running: `set_next_task_scx()` only raises `SCX_TASK_RESET_RUNNABLE_AT`, so the field keeps the past stamp and `get_delay_sec()` reports a real wakeup-to-run latency. |
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

1. **The default is a static allocation, and it is an approximation.**
   Without `layered_enable_control_loop()` the allocation is computed once
   before `ops.init`: every auto-allocated layer gets a contiguous
   weight-proportional slice, and open layers additionally absorb whatever is
   left unassigned. Upstream instead sizes non-open layers from measured
   utilization and hands open layers the genuine remainder
   (`main.rs::refresh_cpumasks()`), so an idle confined layer leaves a large
   free pool where ours leaves none. A static allocation has no utilization
   to read; weight is the substitute. The property that IS preserved, because
   it changes `pick_idle_cpu()`'s search scope, is that an open layer never
   holds a CPU another layer was allocated
   (`an_open_layer_gets_only_the_cpus_no_confined_layer_holds`).
   Enabling the control loop replaces this with real per-layer usage.
2. **`calc_raw_demands` is reimplemented.** It is the one piece of
   scheduler-owned userspace sizing logic that is ours rather than upstream's,
   and is deliberately kept thin. Pinned-util demand, peak-util sizing
   (`util_peak_half_life_ms` is hard-coded 0) and memory-bandwidth sizing are
   not implemented. *Mitosis has the same class of userspace control gap.*
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

`crates/scx_simulator/tests/layered.rs` — 35 tests, plus 4 in
`tests/layered_alloc.rs` and ~80 upstream `alloc.rs` unit tests. Every scenario sets
`.detect_bpf_errors()`, so a `scx_bpf_error` (for instance layered's
"didn't match any layer") fails the test rather than passing silently.

- **ABI guard** — 26 `LayerKind` / `LayerMatch` / `LayerGrowthAlgo`
  discriminants compared against what the compiled `.so` reports, so an
  upstream reordering of `enum layer_match_kind` fails a test instead of
  silently mis-configuring every layer. That is 26 of the 35 discriminants
  those three enums define; the 9 unguarded ones are the tail of
  `LayerGrowthAlgo` (`CpuSetSpread*`, `RandomTopo`, `StickyDynamic` and the
  `NodeSpread*` group). Those are precisely the values the enable-time
  rejections key off, so a silent upstream renumbering there would misroute a
  rejection — worth closing, filed as follow-up.
- **Default config** — loads, `ops.init` succeeds, one catch-all layer owns
  every CPU, all tasks complete, oversubscription starves nobody.
- **Topology** — `llc_topology_drives_dsq_selection` is behavioural: tasks
  pinned into different LLCs must land on different DSQs, with a flat-topology
  control where they must all share one. Sibling publication is checked for
  well-formedness (involution, no self-pairing) and for -1 when SMT is off;
  it is NOT compared against the engine's own `build_cpus` layout, so an
  engine that paired CPUs differently would not be caught there.
  `control_loop_and_scheduler_agree_on_the_node_partition` pins the userspace
  and scheduler views of the node count together. Multi-LLC + NUMA + SMT run
  end to end.
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
  DSQ (`GSTAT_ANTISTALL` > 0) with `layered_set_antistall(true, 0, ..)`, while the identical
  workload with a one-hour threshold leaves the counter at zero. The paired
  control is the point — without it the test would pass on a counter that
  increments unconditionally.
- **Tier-3 control** — the identical asymmetric workload remains at `(2, 2)`
  with the loop disabled and reaches `(3, 1)` with it enabled. The test reads
  both serialized `layer->cpus` state and the real BPF kptr cpumasks. Removing
  only the periodic BPF refresh makes it fail with BPF `(2, 2)` versus
  serialized `(3, 1)`.
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
| SMT topology | **genuinely exercised** (allocation), publication only (placement) | `smt_core_transfer_moves_whole_cores_only` forces a real core transfer under the control loop and proves whole cores move together; releasing half a core fails it. The SMT-off arm of `smt_siblings_are_published_only_when_smt_is_on` is a real discriminator (-1 short-circuits the exclusive-layer path). Still missing: a test showing SMT changing a *placement* decision. mb sim-u4the. |
| comm matching | genuinely exercised | see Multi-layer. |
| cgroup matching | genuinely exercised | `cgroup_prefix_match_routes_tasks_by_cgroup_path`, `cgroup_suffix_and_contains_match` drive the real `format_cgrp_path()` + `match_prefix_suffix()`/`match_substr()`; both fail under the matching sabotage. |
| antistall timer | genuinely exercised | `GSTAT_ANTISTALL` = 589 with `layered_set_antistall(true, 0, ..)`, 0 with `layered_set_antistall(true, 3600, ..)` on the identical workload. |
| `ops.dump` | genuinely exercised | asserts on emitted text; the "no surviving conversion spec" assertion caught a real formatter bug (`%+lldms`). |

Two tests were found overclaiming during this audit and corrected:
`topology_seen_by_scheduler_matches_the_engine` compared the wrapper against
a re-derivation of its own input rather than against the engine (renamed and
its comment corrected, with the behavioural test added alongside), and the
dump test's spec check originally looked for two hardcoded specs and would
have missed the bug it was written to catch.
