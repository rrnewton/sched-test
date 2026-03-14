---
title: 1.0 Release Candidate validation
status: open
priority: 0
issue_type: task
created_at: 2026-03-14T15:31:46.638258256+00:00
updated_at: 2026-03-14T16:50:55.529682889+00:00
---

# Description


## 1.0 RC Validation Tracker

Branch: `simulator.v4` (tip: a9de150)

### Completed for RC

- [x] Safety boundary refactor: safe/ + unsafe_impl/ module split (sim-3ef7bc) — CLOSED
- [x] SimArc conversion: Arc<Mutex<SimState>> replacing raw static pointers
- [x] SIM_STATE raw pointer path fully eliminated (5a8417c)
- [x] engine.rs: zero unsafe blocks, moved to safe/ (15b43cb)
- [x] 3 RAII wrapper modules: scheduler_wrapper, task_wrapper, cgroup_wrapper — moved to unsafe_impl/
- [x] task.rs split: safe types in safe/task.rs, SimTask FFI in unsafe_impl/sim_task.rs
- [x] task.rs facade removed — direct re-exports in lib.rs (a9de150)
- [x] All clippy and compiler warnings fixed (ec80fa5)
- [x] SIM_ARC installed in concurrent worker threads (aa7746c) — sim-b5e2fe CLOSED
- [x] Re-entrant mutex deadlock fix in sim_rbc_pause/resume (552ce15)
- [x] Integration test deadlocks fixed (0c81ae3)
- [x] SchedulerWrapper used for dispatch — last unsafe in engine.rs eliminated (7a66a30)
- [x] SendPtr internals hidden behind safe factory methods (7a66a30)
- [x] Benchmark --perf and --filter-mode support added (03b1d1b)
- [x] Performance verified: ~0% overhead from safety refactor (perf: <0.1% in mutex ops)
- [x] e9patch install script and CI integration (3ad73a2)
- [x] Stale .d file build fix (3fc7a3f)
- [x] Global events migrated to per-CPU dispatch
- [x] Native concurrency backend (sim-a730ac)
- [x] Determinism bugs fixed (500+ failures from stress.py, sim-e0791)
- [x] LAVD scheduler fully passing (6 test suites)
- [x] Nightly upstream sync workflow
- [x] Safety audit with // SAFETY: comments on all unsafe blocks (786e4e1) — sim-3ef7bc CLOSED
- [x] Clippy warnings fixed (sim-1df9af CLOSED)
- [x] cargo fmt clean (sim-f86e4c CLOSED)
- [x] validate.sh hang workaround (sim-27b2ba CLOSED)

### In progress

- [ ] SimCgroupHandle refactor: replace *mut c_void in CgroupInfo with RAII handle (agent running on sched-test2)
- [ ] Safe subset lint: #![forbid(unsafe_code)] in safe/mod.rs + validate.sh check (agent running on sched-test3)
- [ ] cgroup.rs: move to safe/ after CgroupHandle refactor completes

### src/ root status

After all current work lands, src/ will contain only:
- `lib.rs` — crate root with re-exports
- `safe/` — 17 modules, zero unsafe, enforced by #![forbid(unsafe_code)]
- `unsafe_impl/` — 14 modules, all unsafe consolidated with SAFETY docs

### Open P1 blockers — must triage for RC

- [ ] sim-09a30: Kfunc cost table for deterministic overhead accounting
- [ ] sim-010a1: Mitosis coverage tracking — remaining gaps
- [ ] sim-d80a1: Mitosis vtime starvation on 1-CPU scenarios
- [ ] sim-79e55: test_cpu_affinity fails — pinned task on wrong CPU
- [ ] sim-d46dd: Hardware breakpoint replay for deterministic preemption

### Open P2 — triage: RC or post-RC

- [ ] sim-880544: engine.rs exceeds 4700 lines (guideline: 1500 max)
- [ ] sim-e5076: Re-enable COSMOS scheduler after API update
- [ ] sim-ed0d6: Re-enable Mitosis scheduler after kptr/RAII API update
- [ ] sim-80ce04: Refactor PreemptionBackend
- [ ] sim-943dc0: BPF instruction limit enforcement
- [ ] sim-980447: Tickless scheduler stalls under cooperative interleaving
- [ ] sim-c3fd09: SIGSTKFLT crash during preemptive recording with mitosis
- [ ] sim-d9988: Full BPF arena support
- [ ] sim-6b003: Real-vs-simulated trace comparison — 6 realism gaps
- [ ] sim-c5dbf: Bpftrace kprobes lack PIDs
- [ ] sim-ac6b5a: test_preemptive_custom_timeslice hangs (pre-existing)
- [ ] sim-466a13: test_preemptive_pmu_determinism hangs (pre-existing)

### Open P3 — post-RC

- [ ] sim-4c1e5: rt-app workload parser unsupported features
- [ ] sim-d848e: OpsContext by construction
- [ ] RAII migration for CgroupInfo/SimTask (eliminate remaining unsafe Send/Sync)

### RC validation results (last run)

| Check | Status | Notes |
|-------|--------|-------|
| cargo build | PASS | 0 errors, 0 warnings |
| cargo clippy -D warnings | PASS | Clean after ec80fa5 |
| cargo fmt --check | PASS | Clean |
| cargo test --lib | PASS | 186 tests |
| cargo nextest (full) | 533/536 | 2 PMU hangs (pre-existing), 1 fixed |
| cargo test --doc | PASS | 3 passed, 2 ignored |
| scxsim run (simple) | PASS | Sequential + interleave + preemptive |
| scxsim run (lavd) | PASS | Sequential + interleave + preemptive |
| scxsim run (mitosis) | PASS | Sequential + interleave + preemptive |
| scxsim run (cosmos) | PASS | Sequential + interleave |
| stress.py smoke | PASS | |
| Performance regression | NONE | ~0% overhead (perf confirmed) |
| safe/ has zero unsafe | PASS | Verified by grep (forbid attr in progress) |

### Closed issues this cycle

- sim-3ef7bc: Safety boundary refactor epic — COMPLETE
- sim-b5e2fe: Worker thread SIM_ARC SIGABRT — FIXED
- sim-1df9af: 25 clippy errors in engine.rs — FIXED
- sim-f86e4c: cargo fmt failure — FIXED
- sim-27b2ba: validate.sh blocked by nextest hang — FIXED

### RC Stress Test Matrix

stress.py must run a comprehensive randomized test covering this parameter space:

| Dimension | Current Values | Notes |
|-----------|---------------|-------|
| Scheduler | simple, lavd, cosmos, tickless, mitosis | 5 schedulers |
| Workload | two_runners, dsq_contention, lavd_dsq_stress, simple_wake | 4 workload files |
| CPU count | 1, 2, 4, 8 | Edge cases at 1 and 8 |
| Interleave mode | off, cooperative, preemptive, e9patch | 4 modes |
| Seed | random u32 | For determinism replay |
| Sim duration | 4s default | Virtual time |

**Current gaps — dimensions NOT yet randomized:**
- Task count (currently fixed per workload file)
- Noise config (context switch overhead, IRQ injection)
- Overhead config (sched_overhead_rbc_ns)
- Cgroup hierarchy depth
- CPU hotplug events
- Task migration patterns
- Watchdog timeout (currently 2s fixed)

**RC validation target:**
```
python3 bug_finding/stress.py --duration 30 --determinism
```
30 minutes of randomized testing with determinism checking. Zero new findings = PASS.
