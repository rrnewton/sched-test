# Mitosis Scheduler — Function-Level Test-Coverage Report

**Generated:** 2026-07-23T21:10:56Z
**Commit:** integration `7d2d7f2`, scx submodule `59c30ba`
**Profile:** `coverage-out/merged.profdata` from `./coverage.sh --keep-profraw`
(full `cargo test --all -- --test-threads=1` suite, clang source-based coverage).

This report fills the gap left by `OVERNIGHT_COVERAGE_REPORT.md` (PR #57),
which measured simple / lavd / cosmos but **not** mitosis. Methodology is
identical: a function counts as **tested** iff its clang `llvm-cov` execution
**count > 0** after the whole suite runs. This is function granularity — a
covered function may still hide uncovered branches/lines.

**Scope:** the mitosis BPF body only — `scx/scheds/rust/scx_mitosis/src/bpf/`
(`mitosis.bpf.c` + `*.bpf.h`). Shared libs (`scx/lib/*`) and the scxsim
wrapper are out of scope.

## Headline

| metric | value |
|--------|-------|
| total functions | 93 |
| tested (count>0) | 54 |
| untested (dark) | 39 |
| **function coverage** | **54/93 = 58.1%** |

## Per-source-file breakdown

| source file | total | tested | untested |
|-------------|-------|--------|----------|
| cell_cpumask.bpf.h | 9 | 3 | 6 |
| dsq.bpf.h | 6 | 6 | 0 |
| llc_aware.bpf.h | 22 | 0 | 22 |
| mitosis.bpf.c | 47 | 40 | 7 |
| mitosis.bpf.h | 5 | 5 | 0 |
| slice_shrinking.bpf.h | 4 | 0 | 4 |

## Tested functions (count > 0)

The mitosis decision core is well-exercised: `select_cpu`, `enqueue`,
`dispatch`, `running`, `stopping`, `init_task`, `cgroup_init/exit/move`,
`init`, `exit`, `dump`, plus the DSQ helpers, cell/task lookups, cpumask
refresh, and per-cell stats.

| exec count | function | source file |
|-----------:|----------|-------------|
| 79036 | `wrapper.c:lookup_cpu_ctx` | mitosis.bpf.c |
| 75976 | `wrapper.c:dsq_is_invalid` | dsq.bpf.h |
| 61028 | `wrapper.c:lookup_cell` | mitosis.bpf.h |
| 59230 | `wrapper.c:lookup_task_ctx` | mitosis.bpf.c |
| 55914 | `wrapper.c:get_cell_llc_dsq_id` | dsq.bpf.h |
| 37884 | `wrapper.c:dsq_peek` | mitosis.bpf.h |
| 37092 | `wrapper.c:lookup_cell_cpumask_wrapper` | cell_cpumask.bpf.h |
| 35840 | `wrapper.c:init_cpumask_slot` | cell_cpumask.bpf.h |
| 20746 | `wrapper.c:maybe_refresh_cell` | mitosis.bpf.c |
| 20062 | `wrapper.c:get_cpu_dsq_id` | dsq.bpf.h |
| 19138 | `wrapper.c:cstat_add` | mitosis.bpf.h |
| 19138 | `wrapper.c:cstat_inc` | mitosis.bpf.h |
| 18942 | `mitosis_dispatch` | mitosis.bpf.c |
| 18834 | `mitosis_running` | mitosis.bpf.c |
| 18538 | `mitosis_stopping` | mitosis.bpf.c |
| 18538 | `wrapper.c:advance_cell_llc_vtime` | mitosis.bpf.c |
| 18538 | `wrapper.c:update_task_runtime_ewma` | mitosis.bpf.h |
| 14370 | `mitosis_select_cpu` | mitosis.bpf.c |
| 14080 | `wrapper.c:pick_idle_cpu` | mitosis.bpf.c |
| 14080 | `wrapper.c:pick_idle_cpu_from` | mitosis.bpf.c |
| 14080 | `wrapper.c:try_pick_idle_cpu` | mitosis.bpf.c |
| 6376 | `mitosis_enqueue` | mitosis.bpf.c |
| 1252 | `wrapper.c:lookup_cell_cpumask` | cell_cpumask.bpf.h |
| 1146 | `wrapper.c:get_cpu_from_dsq` | dsq.bpf.h |
| 1146 | `wrapper.c:is_cpu_dsq` | dsq.bpf.h |
| 1146 | `wrapper.c:is_user_dsq` | dsq.bpf.h |
| 1116 | `wrapper.c:lookup_cgrp_ctx_fallible` | mitosis.bpf.c |
| 1112 | `wrapper.c:update_task_cpumask` | mitosis.bpf.c |
| 706 | `dump_cpumask_word` | mitosis.bpf.c |
| 696 | `wrapper.c:dump_cpumask` | mitosis.bpf.c |
| 558 | `wrapper.c:update_task_cell` | mitosis.bpf.c |
| 556 | `mitosis_dump_task` | mitosis.bpf.c |
| 556 | `mitosis_init_task` | mitosis.bpf.c |
| 556 | `mitosis_set_cpumask` | mitosis.bpf.c |
| 556 | `wrapper.c:init_task_impl` | mitosis.bpf.c |
| 556 | `wrapper.c:record_init_task` | mitosis.bpf.c |
| 540 | `wrapper.c:task_cgroup` | mitosis.bpf.c |
| 538 | `wrapper.c:cgrp_is_dying` | mitosis.bpf.c |
| 538 | `wrapper.c:init_cgrp_ctx_with_ancestors` | mitosis.bpf.c |
| 174 | `mitosis_cgroup_init` | mitosis.bpf.c |
| 168 | `mitosis_cgroup_exit` | mitosis.bpf.c |
| 160 | `wrapper.c:init_cgrp_ctx` | mitosis.bpf.c |
| 160 | `wrapper.c:record_cgroup_init` | mitosis.bpf.c |
| 140 | `mitosis_dump` | mitosis.bpf.c |
| 140 | `mitosis_exit` | mitosis.bpf.c |
| 140 | `mitosis_init` | mitosis.bpf.c |
| 140 | `validate_flags` | mitosis.bpf.c |
| 140 | `validate_userspace_data` | mitosis.bpf.c |
| 140 | `wrapper.c:dump_cell_cpumask` | mitosis.bpf.c |
| 20 | `wrapper.c:lookup_cgrp_ancestor` | mitosis.bpf.c |
| 20 | `wrapper.c:lookup_cgrp_ctx` | mitosis.bpf.c |
| 8 | `wrapper.c:record_cgroup_exit` | mitosis.bpf.c |
| 4 | `mitosis_cgroup_move` | mitosis.bpf.c |
| 2 | `wrapper.c:refresh_task_cell` | mitosis.bpf.c |

## Untested (dark) functions — categorized with reasons

The 39 dark functions split into **32 substrate-blocked** (hard gaps needing
simulator infrastructure) and **7 config-gated** (reachable today with a knob
+ workload — genuine test opportunities, filed as `mb sim-31d4c7`).

### A. Substrate-blocked (32)

**A1 — LLC-awareness subsystem: entire `llc_aware.bpf.h` (22).** Gated by
`enable_llc_awareness` (default `false`, `mitosis.bpf.h:42`) AND requires the
`cpu_to_llc[]` / `llc_to_cpus[]` BPF arrays to be populated by userspace — the
simulator never populates them (grep confirms no writer in `wrapper.c` or the
engine), so flipping the knob alone would misbehave. With the knob off all
cells use `FAKE_FLAT_CELL_LLC` and the whole file is bypassed. (Pre-existing
`TODO(sim-llc)` in `mitosis.rs:3314-3321`.)

**A2 — Userspace cell-config apply path (7):** `apply_cell_config`
(`mitosis.bpf.c`, a `SEC("syscall")` program) plus its cell-cpumask apply
helpers in `cell_cpumask.bpf.h`. `apply_cell_config` is the *only* cell-control
path since upstream removed the BPF cell allocator (scx `0f579b78`/`b62f1bae`),
and the simulator has no hook that invokes it (there is no `Scheduler`-trait
method for a `SEC("syscall")` program). This is the root cause of mitosis cells
not being modeled — tracked by `mb sim-010a1` / `mb sim-c923d6` / `mb sim-89948f`.

**A3 — Kernel probe hooks (3):** `____fentry_cpuset_write_resmask` (an fentry
hook) and `____tp_cgroup_mkdir` / `____tp_cgroup_rmdir` (raw tracepoints). The
simulator models cgroup lifecycle via engine events + `ops.cgroup_init/exit`,
and does not deliver kernel fentry/raw-tracepoint probes to the scheduler.

### B. Config-gated, coverable-but-untested (7) — `mb sim-31d4c7`

**B1 — Slice-shrinking: entire `slice_shrinking.bpf.h` (4).** Gated by
`enable_slice_shrinking` (default `false`, `slice_shrinking.bpf.h:71`) AND a
partially-pinned task (`!tctx->all_cell_cpus_allowed`, see `mitosis.bpf.c:877`).
No test sets the knob. Coverable by a test that sets `enable_slice_shrinking`
via `get_symbol` + runs a task whose `allowed_cpus` is a strict subset.

**B2 — Multi-CPU-pinned DSQ path (3):** `select_pinned_cpu`,
`enqueue_pinned_cpu`, `update_pinned_dsq`. `select_pinned_cpu` fires only when a
task is *multi-CPU* pinned (`!all_cell_cpus_allowed` && `cpumask_weight > 1`,
`mitosis.bpf.c:670-679`) AND `dynamic_affinity_cpu_selection=true` (default
`false`, `mitosis.bpf.c:51`). Existing pinned tests use single-CPU pins (weight
== 1, which takes the `get_cpu_from_dsq` branch) and leave the knob off.
Coverable by a task pinned to a 2+ CPU subset with `dynamic_affinity_cpu_selection`
enabled.

**A1 llc_aware.bpf.h (22):**

- `wrapper.c:account_cell_llc_enqueue`
- `wrapper.c:cell_llc_drain_disable`
- `wrapper.c:cell_llc_drain_enable`
- `wrapper.c:cell_llc_has_cpus`
- `wrapper.c:cell_llc_nr_queued_dec`
- `wrapper.c:cell_llc_nr_queued_inc`
- `wrapper.c:cell_mask_intersects_llc`
- `wrapper.c:choose_task_llc`
- `wrapper.c:init_task_llc`
- `wrapper.c:invalidate_task_llc_cpumask`
- `wrapper.c:kick_cell_idle_cpu`
- `wrapper.c:kick_cell_idle_cpu_locked`
- `wrapper.c:llc_from_cpu`
- `wrapper.c:llc_is_valid`
- `wrapper.c:lookup_llc_cpumask`
- `wrapper.c:maybe_update_task_llc`
- `wrapper.c:refresh_cell_llc_draining`
- `wrapper.c:refresh_task_llc_cpumask`
- `wrapper.c:set_task_llc`
- `wrapper.c:try_draining_work`
- `wrapper.c:try_stealing_work`
- `wrapper.c:update_task_llc_assignment`

**A2 cell-config apply path (7):**

- `apply_cell_config` (mitosis.bpf.c)
- `wrapper.c:build_cpumask_from_data` (cell_cpumask.bpf.h)
- `wrapper.c:cell_cpumask_data_test_cpu` (cell_cpumask.bpf.h)
- `wrapper.c:get_tmp_cpumask` (cell_cpumask.bpf.h)
- `wrapper.c:lookup_cell_borrowable_cpumask` (cell_cpumask.bpf.h)
- `wrapper.c:publish_prepared_cpumask` (cell_cpumask.bpf.h)
- `wrapper.c:set_cpumask_from_data` (cell_cpumask.bpf.h)

**A3 kernel probe hooks (3):**

- `wrapper.c:____fentry_cpuset_write_resmask`
- `wrapper.c:____tp_cgroup_mkdir`
- `wrapper.c:____tp_cgroup_rmdir`

**B1 slice_shrinking.bpf.h (4):**

- `wrapper.c:slice_shrink_apply`
- `wrapper.c:slice_shrink_limit`
- `wrapper.c:slice_shrink_on_enqueue`
- `wrapper.c:slice_shrink_on_running`

**B2 multi-CPU-pinned DSQ path (3):**

- `wrapper.c:enqueue_pinned_cpu`
- `wrapper.c:select_pinned_cpu`
- `wrapper.c:update_pinned_dsq`

## Reproduce

```bash
cd scx-sim
./coverage.sh --keep-profraw          # instrumented build + full suite + merge
SO=$(find target -path "*/schedulers_cov/libscx_mitosis.so" | head -1)
llvm-cov export "$SO" -instr-profile=coverage-out/merged.profdata -format=text \
  | jq --arg re 'scx_mitosis/src/bpf/' '
      [ .data[0].functions[] | select(.filenames[0]|test($re))
        | {name:.name, file:(.filenames[0]|sub(".*/";"")), count:.count} ]
      | group_by(.name+"@"+.file)
      | map({name:.[0].name, file:.[0].file, count:(map(.count)|max)})'
```

## Ceiling

The 32 substrate-blocked functions cap achievable coverage at **61/93 = 65.6%**
without simulator infrastructure work (invoking `apply_cell_config`, populating
LLC arrays, delivering fentry/tracepoint probes). Closing the 7 config-gated
functions (`mb sim-31d4c7`) would raise coverage from 58.1% to that ceiling.
