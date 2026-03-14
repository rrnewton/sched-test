---
title: 1.0 Release Candidate validation
status: open
priority: 0
issue_type: task
created_at: 2026-03-14T15:31:46.638258256+00:00
updated_at: 2026-03-14T15:31:46.638258256+00:00
---

# Description

## 1.0 RC Validation Tracker

Track all validation work needed before the 1.0 release.

### Completed for RC

- [x] Safety boundary refactor: safe/ + unsafe_impl/ module split (sim-3ef7bc)
- [x] SimArc conversion: Arc<Mutex<SimState>> replacing raw static pointers
- [x] All clippy and compiler warnings fixed (ec80fa5)
- [x] SIM_ARC installed in concurrent worker threads (aa7746c)
- [x] Re-entrant mutex deadlock fix in sim_rbc_pause/resume (552ce15)
- [x] Integration test deadlocks fixed (0c81ae3)
- [x] e9patch install script and CI integration (3ad73a2)
- [x] Stale .d file build fix (3fc7a3f)
- [x] Global events migrated to per-CPU dispatch
- [x] Native concurrency backend (sim-a730ac)
- [x] Determinism bugs fixed (500+ failures from stress.py, sim-e0791)
- [x] LAVD scheduler fully passing (6 test suites)
- [x] Nightly upstream sync workflow

### Open P1 blockers — must triage for RC

- [ ] sim-09a30: Kfunc cost table for deterministic overhead accounting
- [ ] sim-010a1: Mitosis coverage tracking — remaining gaps
- [ ] sim-d80a1: Mitosis vtime starvation on 1-CPU scenarios
- [ ] sim-79e55: test_cpu_affinity fails — pinned task on wrong CPU
- [ ] sim-d46dd: Hardware breakpoint replay for deterministic preemption

### Open P2 — triage: RC or post-RC

- [ ] sim-e5076: Re-enable COSMOS scheduler after API update
- [ ] sim-ed0d6: Re-enable Mitosis scheduler after kptr/RAII API update
- [ ] sim-80ce04: Refactor PreemptionBackend
- [ ] sim-943dc0: BPF instruction limit enforcement
- [ ] sim-980447: Tickless scheduler stalls under cooperative interleaving
- [ ] sim-c3fd09: SIGSTKFLT crash during preemptive recording with mitosis
- [ ] sim-d9988: Full BPF arena support
- [ ] sim-6b003: Real-vs-simulated trace comparison — 6 realism gaps
- [ ] sim-c5dbf: Bpftrace kprobes lack PIDs

### Open P3 — post-RC

- [ ] sim-4c1e5: rt-app workload parser unsupported features
- [ ] sim-d848e: OpsContext by construction

### RC validation checklist

- [ ] All unit tests pass
- [ ] All integration tests pass
- [ ] LAVD scheduler tests pass
- [ ] Stress tests pass (stress.py --determinism)
- [ ] Coverage report generated
- [ ] Clippy clean (zero warnings)
- [ ] No compiler warnings
- [ ] Benchmark regression check
- [ ] Triage all P1 blockers: fix or defer
