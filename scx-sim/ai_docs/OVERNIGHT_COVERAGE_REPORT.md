# Scheduler Function-Level Test-Coverage Report

**Generated:** 2026-07-23T04:56:23Z (by `scripts/gen_scheduler_fn_coverage.sh`)
**Commit:** integration `c08945a` (gitdepth 199), scx submodule `59c30ba`
**Profile:** `coverage-out/merged.profdata` from `./coverage.sh --keep-profraw`
(full `cargo test --all -- --test-threads=1` suite, clang source-based coverage).

## What "covered" means here

A scheduler C function counts as **tested** iff its clang coverage **execution
count is > 0** after the whole test suite runs — i.e. some test actually drives
the kernel/BPF substrate into that function. This is **function granularity**:
a covered function may still contain uncovered *branches/lines*. For the
line/branch breakdown and the specific uncovered code paths, see the companion
[`COVERAGE_AUDIT_20260722.md`](COVERAGE_AUDIT_20260722.md).

Scope is each scheduler's **own** BPF body only:
- simple → `schedulers/simple/scx_simple.bpf.c`
- lavd → `scx/scheds/rust/scx_lavd/src/bpf/*.bpf.c`
- cosmos → `schedulers/cosmos/cosmos_main_patched.c` (the sed-generated patched
  `main.bpf.c`; `coverage.sh`'s default filter hides it — this report lifts
  that exclusion, so cosmos is no longer under-reported)

Shared libraries linked into every scheduler (`scx/lib/cgroup_bw.bpf.c`,
`ravg.bpf.c`) and the scxsim wrappers are **out of scope** here (they are not
scheduler decision logic).

## Per-scheduler summary

| scheduler | total functions | tested (count>0) | untested | function coverage |
|-----------|-----------------|------------------|----------|-------------------|
| simple | 9 | 9 | 0 | 100.0% |
| lavd | 198 | 171 | 27 | 86.4% |
| cosmos | 54 | 51 | 3 | 94.4% |
| **all three** | **261** | **231** | **30** | **88.5%** |

## simple — 9/9 functions tested

### Per-source-file breakdown

| source file | total | tested | untested |
|-------------|-------|--------|----------|
| scx_simple.bpf.c | 9 | 9 | 0 |

_All 9 functions are exercised by at least one test._

## lavd — 171/198 functions tested

### Per-source-file breakdown

| source file | total | tested | untested |
|-------------|-------|--------|----------|
| balance.bpf.c | 9 | 9 | 0 |
| idle.bpf.c | 19 | 16 | 3 |
| introspec.bpf.c | 3 | 3 | 0 |
| lat_cri.bpf.c | 12 | 12 | 0 |
| lavd.bpf.h | 8 | 8 | 0 |
| lock.bpf.c | 17 | 1 | 16 |
| main.bpf.c | 51 | 48 | 3 |
| power.bpf.c | 19 | 17 | 2 |
| power.bpf.h | 2 | 1 | 1 |
| preempt.bpf.c | 12 | 12 | 0 |
| sys_stat.bpf.c | 8 | 8 | 0 |
| util.bpf.c | 36 | 34 | 2 |
| util.bpf.h | 2 | 2 | 0 |

### Untested functions (27) — no test drives these

| function | source file |
|----------|-------------|
| `wrapper.c:cpumask_any_distribute` | idle.bpf.c |
| `wrapper.c:migrate_to_neighbor` | idle.bpf.c |
| `wrapper.c:pick_random_cpu` | idle.bpf.c |
| `rtp_sys_enter_futex` | lock.bpf.c |
| `rtp_sys_exit_futex` | lock.bpf.c |
| `rtp_sys_exit_futex_wait` | lock.bpf.c |
| `rtp_sys_exit_futex_waitv` | lock.bpf.c |
| `rtp_sys_exit_futex_wake` | lock.bpf.c |
| `wrapper.c:____fexit___futex_wait` | lock.bpf.c |
| `wrapper.c:____fexit_futex_lock_pi` | lock.bpf.c |
| `wrapper.c:____fexit_futex_unlock_pi` | lock.bpf.c |
| `wrapper.c:____fexit_futex_wait_multiple` | lock.bpf.c |
| `wrapper.c:____fexit_futex_wait_requeue_pi` | lock.bpf.c |
| `wrapper.c:____fexit_futex_wake` | lock.bpf.c |
| `wrapper.c:____fexit_futex_wake_op` | lock.bpf.c |
| `wrapper.c:__dec_futex_boost` | lock.bpf.c |
| `wrapper.c:__inc_futex_boost` | lock.bpf.c |
| `wrapper.c:dec_futex_boost` | lock.bpf.c |
| `wrapper.c:inc_futex_boost` | lock.bpf.c |
| `wrapper.c:____cond_hook_sys_enter_execve` | main.bpf.c |
| `wrapper.c:____cond_hook_sys_enter_execveat` | main.bpf.c |
| `wrapper.c:set_aggressive_migration` | main.bpf.c |
| `get_cpuperf_cap` | power.bpf.c |
| `set_power_profile` | power.bpf.c |
| `wrapper.c:conv_wall_to_invr` | power.bpf.h |
| `get_nice_prio` | util.bpf.c |
| `set_cpu_flag` | util.bpf.c |

## cosmos — 51/54 functions tested

### Per-source-file breakdown

| source file | total | tested | untested |
|-------------|-------|--------|----------|
| cosmos_main_patched.c | 54 | 51 | 3 |

### Untested functions (3) — no test drives these

| function | source file |
|----------|-------------|
| `wrapper.c:can_use_node` | cosmos_main_patched.c |
| `wrapper.c:cpus_share_cache` | cosmos_main_patched.c |
| `wrapper.c:is_cpu_faster` | cosmos_main_patched.c |

## Reproduce

```bash
cd scx-sim
./coverage.sh --keep-profraw          # instrumented build + full test suite + merge
./scripts/gen_scheduler_fn_coverage.sh > ai_docs/OVERNIGHT_COVERAGE_REPORT.md
```

## Caveats

- **Function vs line/branch.** 100% function coverage does NOT mean 100%
  line/branch coverage. E.g. `simple` executes all its functions but still has
  uncovered `fifo_sched=true` branches (see the companion audit). Function
  coverage is a floor: an untested function is a hard gap; a tested function may
  still hide untested paths.
- **Untested ≠ dead code.** Some untested functions are unreachable under the
  current scxsim substrate (e.g. `SEC("syscall")` init hooks not invoked by the
  harness, or futex raw-tracepoints the substrate does not yet deliver). Those
  are substrate gaps to file, not merely missing tests — the companion audit
  and the linked minibeads issues distinguish them.
- Counts come straight from `llvm-cov export`; re-running regenerates every
  number. Do not hand-edit the tables above.
