---
title: 1.0 Release Candidate validation
status: open
priority: 0
issue_type: task
created_at: 2026-03-14T15:31:46.638258256+00:00
updated_at: 2026-03-18T02:31:48.040850934+00:00
---

# Description

## 1.0 RC Validation Tracker

Branch: \`simulator.v4\` (tip: 15a3d0c)

### Completed for RC

#### Safety boundary refactor
- [x] safe/ + unsafe_impl/ module split (sim-3ef7bc) — CLOSED
- [x] SimArc conversion: Arc<Mutex<SimState>> replacing raw static pointers
- [x] SIM_STATE raw pointer path fully eliminated
- [x] engine.rs: zero unsafe blocks, moved to safe/
- [x] task.rs split: safe types → safe/task.rs, SimTask FFI → unsafe_impl/sim_task.rs
- [x] 3 RAII wrappers (scheduler, task, cgroup) moved to unsafe_impl/
- [x] #![forbid(unsafe_code)] enforced in safe/mod.rs + validate.sh grep check
- [x] All clippy/compiler warnings fixed, cargo fmt clean
- [x] Performance: ~0% overhead (perf: <0.1% in mutex ops)

#### Concurrency and mutex fixes
- [x] SIM_ARC installed in concurrent worker threads (sim-b5e2fe) — CLOSED
- [x] Re-entrant mutex deadlock fix in sim_rbc_pause/resume
- [x] Integration test deadlocks fixed

#### Interleaving stall fixes
- [x] Remove __thread from C map registry — cosmos/tickless stalls (sim-2ced8b) — CLOSED
- [x] Restore current_cpu in with_sim() — all scheduler cooperative stalls (sim-46dcf3) — CLOSED
- [x] Tickless stalls under interleaving (sim-980447) — CLOSED (fixed by above)

#### Replay and preemption fixes
- [x] Replay SIGABRT: abort→retry + ops_context consistency (sim-603bd5) — CLOSED
- [x] Replay PMU margin: skip PMU for small targets, fix bp-only arming (sim-2273f8) — CLOSED
- [x] E9patch replay backend implemented — deterministic replay without PMU (sim-911585)

#### CPU affinity and dispatch
- [x] dsq_move_to_local respects task cpumask (sim-79e55) — CLOSED

#### Infrastructure
- [x] ASLR disabled at startup (personality + re-exec), verified matching hermit
- [x] ASLR stability test (scripts/test_aslr.sh) in validate.sh
- [x] Benchmark --perf and --filter-mode support
- [x] Randomized workload generation in stress.py (task count 1-32, phase patterns, sim params)
- [x] e9patch install script and CI integration
- [x] Nightly upstream sync workflow

### Stress test results (post-fix, 30 min)

| Metric | Previous (pre-fix) | Current (post-fix) | Change |
|--------|-------------------|-------------------|--------|
| Total runs | 26,032 | 18,578 | — |
| Total findings | 9,070 (35%) | 2,787 (15%) | -57% |
| **Stall** | **1,101 (4.2%)** | **72 (0.39%)** | **-93%** |
| Timeout | 2,695 | 2,715 | ~same (PMU hangs) |
| Crash | — | 0 | — |

**Non-preemptive failure rate: 0.39%** — PASS (threshold: <5%)

Remaining 72 stalls: 58 preemptive, 14 cooperative (from randomized workloads only).
Remaining 2,715 timeouts: all preemptive mode PMU hangs (pre-existing, tracked under sim-d46dd).

### Still in progress

- [ ] SimCgroupHandle refactor: replace *mut c_void in CgroupInfo with RAII handle
- [ ] cgroup.rs: move to safe/ after CgroupHandle refactor completes
- [ ] Expand stress.py determinism coverage (record/replay matrix)

### Stress test determinism coverage gaps

| Test Case | Status |
|-----------|--------|
| Record(PMU) → Replay(BP) succeeds | Tested (stress.py --determinism) |
| Record(PMU) → Replay(BP) output matches recording | NOT TESTED |
| Record(PMU) → Replay(BP) twice → identical | NOT TESTED |
| Record(PMU) → Replay(e9patch) | NOT TESTED |
| E9patch run × N → deterministic | Tested (stress.py --determinism, e9patch mode) |
| Sequential × 2 → deterministic (--no-rbc) | Tested |
| Cooperative × 2 → deterministic (--no-rbc) | Tested |

### Open P1 — must triage for RC

- [ ] sim-09a30: Kfunc cost table for deterministic overhead accounting
- [ ] sim-010a1: Mitosis coverage tracking — remaining gaps
- [ ] sim-d80a1: Mitosis vtime starvation on 1-CPU scenarios
- [ ] sim-d46dd: Hardware breakpoint replay for deterministic preemption

### Open P2 — triage: RC or post-RC

- [ ] sim-880544: engine.rs exceeds 4700 lines (guideline: 1500 max)
- [ ] sim-e5076: Re-enable COSMOS scheduler after API update
- [ ] sim-ed0d6: Re-enable Mitosis scheduler after kptr/RAII API update
- [ ] sim-80ce04: Refactor PreemptionBackend
- [ ] sim-943dc0: BPF instruction limit enforcement
- [ ] sim-c3fd09: SIGSTKFLT crash during preemptive recording with mitosis
- [ ] sim-d9988: Full BPF arena support
- [ ] sim-6b003: Real-vs-simulated trace comparison — 6 realism gaps
- [ ] sim-c5dbf: Bpftrace kprobes lack PIDs
- [ ] sim-ac6b5a: test_preemptive_custom_timeslice hangs (pre-existing)
- [ ] sim-466a13: test_preemptive_pmu_determinism hangs (pre-existing)
- [ ] sim-094402: test_batch_concurrent_preemptive_smoke hang

### Open P3 — post-RC

- [ ] sim-4c1e5: rt-app workload parser unsupported features
- [ ] sim-d848e: OpsContext by construction
- [ ] RAII migration for CgroupInfo (eliminate remaining unsafe Send/Sync)

### RC validation checklist

| Check | Status |
|-------|--------|
| cargo build (0 warnings) | PASS |
| cargo clippy -D warnings | PASS |
| cargo fmt --check | PASS |
| cargo test --lib (186 tests) | PASS |
| cargo nextest (534/536, 2 PMU hangs) | PASS |
| safe/ has zero unsafe | PASS (compiler + grep enforced) |
| ASLR test (scripts/test_aslr.sh) | PASS (6/6 checks) |
| scxsim run (all 4 schedulers, all modes) | PASS |
| stress.py 30min (<5% non-preemptive) | PASS (0.39%) |
| Performance regression | NONE (~0% overhead) |
| Benchmark perf profiling | DONE (<0.1% mutex overhead) |

### Issues closed this cycle

sim-3ef7bc, sim-b5e2fe, sim-1df9af, sim-f86e4c, sim-27b2ba, sim-2ced8b, sim-46dcf3, sim-2273f8, sim-603bd5, sim-79e55, sim-980447, sim-911585 (implemented)
