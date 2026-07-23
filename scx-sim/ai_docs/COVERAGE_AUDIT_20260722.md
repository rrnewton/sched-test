# Scheduler Code-Coverage Audit — simple / lavd / cosmos

**Date:** 2026-07-22
**Commit:** integration tip `e0ced50` (gitdepth 154), scx submodule `59c30bae`
**Method:** `./coverage.sh --lcov --keep-profraw` (clang source-based coverage),
then per-function `llvm-cov export` / `llvm-cov report` against the retained
`coverage-out/merged.profdata` and the `schedulers_cov/libscx_*.so` objects.
**Tests run:** full `cargo test --all` suite, `--test-threads=1`.

This audit supersedes the stale CSV baseline (last recorded 2026-03-04 @
`3e4f132`, before cosmos was supported). Fresh totals were appended to
`data/coverage.csv` by the tool.

---

## 0. Meta-finding: cosmos coverage is UNDER-REPORTED by the tooling

`coverage.sh`'s default source filter contains
`-ignore-filename-regex=cosmos_main_patched\.c`. Because cosmos's wrapper
compiles the *patched* copy (`schedulers/cosmos/cosmos_main_patched.c`, a
`sed`-generated div-by-zero guard on `main.bpf.c`), **the entire cosmos
scheduler body is excluded from the default report and from the recorded
CSV.** What the CSV labels "cosmos" (76.4% lines) is actually only the shared
`cgroup_bw.bpf.c` + `ravg.bpf.c` libraries.

Measured directly (filter lifted), cosmos's *own* scheduler code is:

| metric   | cosmos_main_patched.c |
|----------|-----------------------|
| Lines    | **59.24%** (256/628 missed) |
| Functions| 83.33% (9/54 missed)  |
| Branches | **33.48%** (300/451 missed) |
| Regions  | 49.79% (355/707 missed) |

**Action item (tooling):** either (a) rewrite the ignore-regex so the patched
file is attributed back to cosmos and reported/recorded, or (b) change the
cosmos build so the patch is applied without renaming the translation unit
(e.g. patch in place / `#line`-preserve), so `main.bpf.c` coverage is visible.
Until fixed, cosmos coverage numbers in `data/coverage.csv` are misleading.

---

## 1. Per-scheduler summary (measured at e0ced50)

| scheduler | scheduler-body lines | scheduler-body branches | dedicated tests | notes |
|-----------|----------------------|-------------------------|-----------------|-------|
| simple    | 92.16% (4 missed)    | 73.33% (4 missed)       | 8 generic + 4   | only `fifo_sched=true` mode uncovered |
| lavd      | ~77% (varies by TU)  | 62.98% overall          | 190 dedicated   | `lock.bpf.c` (futex) at 3.7% is the hole |
| cosmos    | 59.24% (256 missed)  | 33.48% (300 missed)     | 8 generic + 3   | idle-scan + heterogeneous + NUMA-restrict paths uncovered |

(Shared `scx/lib/cgroup_bw.bpf.c` = 75.9% lines / 55.6% branch and
`ravg.bpf.c` = 83.1% / 73.1% are linked into every scheduler and covered
mostly via the lavd cgroup-bw test suite.)

---

## 2. SIMPLE — gap is entirely FIFO mode

`schedulers/simple/scx_simple.bpf.c` — 92.16% lines, only 4 uncovered lines,
all in the untested `fifo_sched == true` path plus the percpu-stats path:

| line | code | why uncovered |
|------|------|---------------|
| 82   | `scx_bpf_dsq_insert(p, SHARED_DSQ, SCX_SLICE_DFL, enq_flags)` | `simple_enqueue` FIFO branch — no test sets `fifo_sched=true` |
| 106  | `return;` in `simple_running` | FIFO early-return branch |
| 121  | `return;` in `simple_stopping` | FIFO early-return branch |
| 60   | `(*cnt_p)++;` in `stat_inc` | percpu `stats` map lookup returns NULL under sim (increment never taken) |

**Tests needed for simple:**
1. A `fifo_sched=true` variant of the generic suite (single-CPU + multi-task),
   asserting FIFO ordering — covers lines 82/106/121 and the 4 missed branches.
2. (Optional / substrate) If the percpu `stats` map is meant to be readable
   under sim, back it so `stat_inc` line 60 executes; otherwise this is a
   known sim-substrate no-op, not a scheduler gap.

---

## 3. LAVD — 190 tests, but one whole subsystem is dark

Per-TU line coverage (scheduler body only):

| file | lines | funcs missed | branch | status |
|------|-------|--------------|--------|--------|
| balance.bpf.c   | 75.4% | 0/9   | 65.4% | ok |
| idle.bpf.c      | 69.3% | 3/19  | 55.6% | migration paths uncovered |
| introspec.bpf.c | 43.3% | 0/3   | 44.1% | introspection/debug-dump paths thin |
| lat_cri.bpf.c   | 92.4% | 0/12  | 83.3% | good |
| lock.bpf.c      | **3.7%** | **16/17** | **1.4%** | **futex subsystem essentially untested** |
| main.bpf.c      | 80.0% | 3/51  | 66.2% | ok |
| power.bpf.c     | 80.9% | 2/19  | 64.7% | profile-switch path uncovered |
| power.bpf.h     | 12.1% | 1/2   | 50.0% | `conv_wall_to_invr` inline uncovered |
| preempt.bpf.c   | 95.3% | 0/12  | 81.0% | good |
| sys_stat.bpf.c  | 93.3% | 0/8   | 75.7% | good |
| util.bpf.c      | 91.5% | 2/36  | 87.5% | good |

### 3a. `lock.bpf.c` — futex priority-boost (16/17 functions NEVER executed)

The futex tracepoint/boost machinery is completely unexercised:

- `rtp_sys_enter_futex`, `rtp_sys_exit_futex`, `rtp_sys_exit_futex_wait`,
  `rtp_sys_exit_futex_waitv`, `rtp_sys_exit_futex_wake` (raw-tracepoint entry/exit)
- `inc_futex_boost` / `__inc_futex_boost`, `dec_futex_boost` / `__dec_futex_boost`
- fexit hooks: `____fexit___futex_wait`, `____fexit_futex_lock_pi`,
  `____fexit_futex_unlock_pi`, `____fexit_futex_wait_multiple`,
  `____fexit_futex_wait_requeue_pi`, `____fexit_futex_wake`,
  `____fexit_futex_wake_op`

**Root cause:** no test drives futex syscalls, and the scxsim substrate may not
deliver the futex raw-tracepoints/fexit hooks that these depend on. This is
likely a *substrate* gap (need to model futex enter/exit tracepoints), not just
a missing test. Recommend: file a scxsim-infra issue to deliver futex
tracepoints, then add a lavd test with a futex-contended workload asserting the
boost is applied/decayed. Until then, lavd's lock-boost logic is running 0% under
sim (relevant to any deadlock/priority-inheritance investigation).

### 3b. `idle.bpf.c` — load-balancing migration paths uncovered

- `migrate_to_neighbor` (63 regions) — cross-domain migration
- `pick_random_cpu` (26 regions) — random-CPU fallback
- `cpumask_any_distribute` (21 regions) — distributed cpumask pick

**Test needed:** an overloaded/imbalanced multi-domain scenario that forces
LAVD's balance path to migrate to a neighbor domain / fall back to random pick.

### 3c. `power.bpf.c` / `power.bpf.h` — dynamic power-profile switching

- `set_power_profile`, `get_cpuperf_cap` (27 regions), `conv_wall_to_invr`

**Test needed:** run with `--perf`/power-profile transitions
(powersave↔performance) so `set_power_profile` and cpuperf-cap scaling execute.

### 3d. `main.bpf.c` — misc

- `set_aggressive_migration` (32 regions), execve cond-hooks
  (`____cond_hook_sys_enter_execve{,at}`) — need an execve-emitting workload and
  the aggressive-migration knob.

---

## 4. COSMOS — largest true gap (59% lines, 33% branches on its own body)

9 of 54 cosmos functions never execute. Ranked by size (regions):

| function | regions | code path it guards |
|----------|---------|---------------------|
| `pick_idle_cpu_flat`     | 71 | flat idle-CPU scan — **disabled** because the cosmos wrapper forces `bpf_ksym_exists(scx_bpf_select_cpu_and)=1`, routing to the kfunc path instead |
| `pick_idle_cpu_pref_smt` | 71 | SMT-preferring idle scan — same reason as above |
| `can_use_node`           | 26 | per-node cpumask restriction (NUMA/node-affinity gating) |
| `enable_sibling_cpu`     | 19 | `SEC("syscall")` SMT-domain setup — userspace-init syscall not invoked by sim |
| `is_cpu_faster`          | 19 | heterogeneous/big.LITTLE capacity comparison |
| `cpus_share_cache`       | 16 | LLC-share test used in placement |
| `get_idle_smtmask`       | 16 | SMT idle mask accessor (only reached via the flat-scan paths) |
| `task_dl`                | 11 | task **deadline** computation |
| `test_cpu_idle`         |  5 | idle re-test in scan |

**Interpretation & tests needed:**

1. **Flat/SMT idle-scan paths (`pick_idle_cpu_flat`, `pick_idle_cpu_pref_smt`,
   `get_idle_smtmask`, `test_cpu_idle`).** These are dead under sim because the
   wrapper hard-sets `bpf_ksym_exists=1` so cosmos always takes the
   `scx_bpf_select_cpu_and` kfunc path (see `cosmos/wrapper.c`). To exercise the
   flat-scan fallback, add a fixture/knob that runs cosmos with
   `flat_idle_scan=true` **or** with `bpf_ksym_exists` returning 0 (simulating a
   kernel lacking `scx_bpf_select_cpu_and`). This is the single biggest cosmos
   coverage win (~142 regions).
2. **`is_cpu_faster` / `cpus_share_cache`** — heterogeneous topology. Add a
   scenario with asymmetric CPU capacities (big.LITTLE) and a multi-LLC topology.
3. **`can_use_node`** — add a task with restricted `allowed_cpus` spanning fewer
   than all NUMA nodes so the node-usability gate is hit (the existing
   `test_numa_topology` uses unrestricted tasks).
4. **`task_dl`** — deadline computation is never reached; investigate which
   enqueue path calls it and add a workload that triggers deadline-based
   ordering (likely event-heavy / interactive tasks).
5. **`enable_sibling_cpu`** — a `SEC("syscall")` init hook. Covering it requires
   the sim harness to invoke the syscall prog during cosmos init (substrate
   task), analogous to how `enable_primary_cpu` IS covered.

Cosmos branch coverage (33%) is the weakest of the three schedulers — even
covered functions like `cosmos_select_cpu` (59 regions) and `cosmos_enqueue`
(126 regions) have many untaken branches. A property/fuzz-style test varying
CPU count, SMT, NUMA, nice levels, and wake patterns would lift branch coverage
substantially.

---

## 5. Prioritized recommendations

1. **[tooling] Fix cosmos coverage reporting** (§0) so the CSV stops
   over-reporting cosmos. High value, low effort.
2. **[cosmos] Add a flat-idle-scan test/knob** (§4.1) — unlocks ~142 uncovered
   regions (the two `pick_idle_cpu_*` scan functions).
3. **[lavd] Decide futex-boost fate** (§3a) — either model futex tracepoints in
   the substrate + add a contended-futex test, or explicitly document
   `lock.bpf.c` as unsupported-under-sim. Currently 16 functions run 0%.
4. **[simple] Add `fifo_sched=true` suite** (§2) — trivially closes simple to
   ~100%.
5. **[cosmos] Heterogeneous + NUMA-restricted + deadline tests** (§4.2–4.4).
6. **[lavd] Balance-migration + power-profile-switch tests** (§3b–3c).

## Reproduce

```bash
cd scx-sim
./coverage.sh --lcov --keep-profraw          # full run, records to data/coverage.csv
# per-function detail (cosmos body, normally hidden):
PROF=coverage-out/merged.profdata
BIN=$(head -1 coverage-out/test-binaries.txt)
llvm-cov report "$BIN" -object target/debug/build/*/out/schedulers_cov/libscx_cosmos.so \
  -instr-profile="$PROF" \
  -ignore-filename-regex='lib/scxtest/|csrc/|scheds/include/|libbpf-sys-.*|/wrapper\.c|/intf\.h$|scx/lib/|scheds/rust/'
```
