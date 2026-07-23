//! `ops.select_cpu` correctness tests across schedulers.
//!
//! On every wakeup the engine calls the real BPF scheduler's `select_cpu`
//! callback and records the decision as `SelectTaskRq { pid, prev_cpu,
//! selected_cpu }`. The engine only clamps an out-of-range return down to
//! `prev_cpu` (kernel `select_task_rq_scx` semantics) — it does **not** clamp
//! the result to the task's cpumask — so "the selected CPU is valid and honors
//! the task's affinity" is a genuine property of the scheduler under test.
//!
//! This suite verifies, for simple / lavd / cosmos:
//!   1. select_cpu returns a valid CPU (`< nr_cpus`).
//!   2. select_cpu honors a task's cpumask (subset affinity).
//!   3. select_cpu honors single-CPU affinity (returns exactly that CPU).
//!   4. select_cpu keeps the strong majority of wakeups LLC-local (cache-warm).
//!   5. select_cpu honors per-NUMA-node affinity under a NUMA topology.
//!   6. all of the above invariants hold identically across the three schedulers.
//!
//! Complements `idle_cpu_selection.rs` (idle notification / spread / power
//! modes) by asserting the *correctness* of the returned CPU rather than idle
//! handling.

use std::collections::BTreeSet;

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor, so one test can sweep several schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

const SCHEDULERS: [NamedSchedFactory; 3] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("cosmos", DynamicScheduler::cosmos),
];

/// A wake/sleep task so it re-enters `select_cpu` on every wakeup.
fn waky() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(3_000_000), Phase::Sleep(2_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// A `TaskDef` pinned to `cpus` with a wake/sleep behavior.
fn pinned(name: &str, pid: i32, cpus: Vec<CpuId>) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: waky(),
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some(cpus),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// `selected_cpu` values from every `select_cpu` decision for `pid`.
fn selected_for(trace: &Trace, pid: Pid) -> Vec<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::SelectTaskRq {
                pid: p,
                selected_cpu,
                ..
            } if p == pid => Some(selected_cpu.0),
            _ => None,
        })
        .collect()
}

/// All `(prev_cpu, selected_cpu)` pairs from every `select_cpu` decision.
fn all_selects(trace: &Trace) -> Vec<(u32, u32)> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::SelectTaskRq {
                prev_cpu,
                selected_cpu,
                ..
            } => Some((prev_cpu.0, selected_cpu.0)),
            _ => None,
        })
        .collect()
}

/// CPUs a task actually ran on (from `TaskScheduled`).
fn ran_on(trace: &Trace, pid: Pid) -> BTreeSet<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.cpu.0),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (1 + 6) select_cpu returns a valid CPU, for every scheduler.
// ---------------------------------------------------------------------------

/// Every `select_cpu` decision must name a CPU that exists (`< nr_cpus`), and
/// the callback must actually fire. Verified for simple / lavd / cosmos.
#[test]
fn test_select_cpu_returns_valid_cpu() {
    let _lock = common::setup_test();
    let nr = 4;
    for (name, make) in SCHEDULERS {
        let mut b = Scenario::builder().cpus(nr);
        for i in 0..(nr * 2) {
            b = b.add_task(&format!("t{i}"), 0, waky());
        }
        let t = Simulator::new(make(nr)).run(b.duration_ms(120).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let selects = all_selects(&t);
        assert!(
            !selects.is_empty(),
            "{name}: no select_cpu decisions recorded"
        );
        for (prev, sel) in &selects {
            assert!(
                *sel < nr,
                "{name}: select_cpu returned invalid CPU {sel} (prev={prev}, nr_cpus={nr})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (2 + 6) select_cpu honors a task's cpumask (subset affinity).
// ---------------------------------------------------------------------------

/// A task restricted to a strict subset of CPUs must never have `select_cpu`
/// pick a CPU outside that subset — the scheduler is responsible for honoring
/// the cpumask (the engine does not clamp to it). The task must also only run
/// within the subset. Verified for all schedulers.
#[test]
fn test_select_cpu_respects_cpumask() {
    let _lock = common::setup_test();
    let allowed: BTreeSet<u32> = [1, 3].into_iter().collect();
    for (name, make) in SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(4)
            .task(pinned("pin13", 1, vec![CpuId(1), CpuId(3)]))
            // Background load so the scheduler has reason to consider migration.
            .add_task("bg0", 0, waky())
            .add_task("bg1", 0, waky())
            .add_task("bg2", 0, waky())
            .duration_ms(150)
            .build();
        let t = Simulator::new(make(4)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let selects = selected_for(&t, Pid(1));
        assert!(!selects.is_empty(), "{name}: pinned task got no select_cpu");
        for sel in &selects {
            assert!(
                allowed.contains(sel),
                "{name}: select_cpu chose CPU {sel} outside cpumask {allowed:?}"
            );
        }
        let ran = ran_on(&t, Pid(1));
        assert!(
            ran.iter().all(|c| allowed.contains(c)),
            "{name}: pinned task ran outside cpumask: ran on {ran:?}, allowed {allowed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (3 in mask sense + 6) single-CPU affinity → select_cpu returns that CPU.
// ---------------------------------------------------------------------------

/// A task pinned to exactly one CPU must have every `select_cpu` return that
/// exact CPU (there is no other legal choice), and it must run only there.
#[test]
fn test_select_cpu_single_cpu_affinity() {
    let _lock = common::setup_test();
    let target = 3u32;
    for (name, make) in SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(4)
            .task(pinned("solo", 1, vec![CpuId(target)]))
            // Keep the other CPUs busy so a naive scheduler might be tempted
            // to migrate the pinned task — it must not.
            .add_task("bg0", 0, waky())
            .add_task("bg1", 0, waky())
            .add_task("bg2", 0, waky())
            .duration_ms(150)
            .build();
        let t = Simulator::new(make(4)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let selects = selected_for(&t, Pid(1));
        assert!(
            !selects.is_empty(),
            "{name}: single-pinned task got no select_cpu"
        );
        for sel in &selects {
            assert_eq!(
                *sel, target,
                "{name}: single-CPU-pinned select_cpu chose {sel}, must be {target}"
            );
        }
        assert_eq!(
            ran_on(&t, Pid(1)),
            [target].into_iter().collect(),
            "{name}: single-CPU-pinned task ran on the wrong CPU(s)"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) select_cpu under full load: still valid, no crash.
// ---------------------------------------------------------------------------

/// Under heavy oversubscription (16 tasks on 4 CPUs), `select_cpu` must keep
/// returning valid CPUs and the simulation must complete normally. Every task
/// should still make progress.
#[test]
fn test_select_cpu_under_full_load() {
    let _lock = common::setup_test();
    let nr = 4;
    let ntasks = 16;
    for (name, make) in SCHEDULERS {
        let mut b = Scenario::builder().cpus(nr);
        for i in 0..ntasks {
            b = b.add_task(
                &format!("h{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(500_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let t = Simulator::new(make(nr)).run(b.duration_ms(150).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let selects = all_selects(&t);
        assert!(
            selects.len() >= ntasks as usize,
            "{name}: too few select_cpu decisions under load: {}",
            selects.len()
        );
        for (prev, sel) in &selects {
            assert!(
                *sel < nr,
                "{name}: invalid CPU {sel} under full load (prev={prev})"
            );
        }
        for pid in 1..=ntasks {
            assert!(
                t.total_runtime(Pid(pid)) > 0,
                "{name}: task {pid} starved under full load"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (4) select_cpu keeps the strong majority of wakeups LLC-local.
// ---------------------------------------------------------------------------

/// With an LLC topology (8 CPUs, 4 per LLC → LLC0={0..3}, LLC1={4..7}), a
/// balanced wake/sleep workload should keep the strong majority of `select_cpu`
/// decisions within the previous CPU's LLC (cache-warm placement). We require a
/// clear majority rather than 100%, since occasional cross-LLC migration for
/// load balancing is legitimate.
#[test]
fn test_select_cpu_prefers_llc_local() {
    let _lock = common::setup_test();
    let nr = 8;
    let cpus_per_llc = 4;
    for (name, make) in SCHEDULERS {
        let mut b = Scenario::builder().cpus(nr).cpus_per_llc(cpus_per_llc);
        for i in 0..nr {
            b = b.add_task(
                &format!("t{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(3_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let t = Simulator::new(make(nr)).run(b.duration_ms(150).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let selects = all_selects(&t);
        assert!(!selects.is_empty(), "{name}: no select_cpu decisions");
        let llc_local = selects
            .iter()
            .filter(|(prev, sel)| prev / cpus_per_llc == sel / cpus_per_llc)
            .count();
        let frac = llc_local as f64 / selects.len() as f64;
        assert!(
            frac >= 0.70,
            "{name}: only {llc_local}/{} ({frac:.2}) select_cpu decisions were LLC-local; \
             expected a strong majority",
            selects.len()
        );
    }
}

// ---------------------------------------------------------------------------
// (5) select_cpu honors per-NUMA-node affinity.
// ---------------------------------------------------------------------------

/// Under a NUMA topology (cosmos, 4 CPUs / 2 nodes → node0={0,1}, node1={2,3}),
/// tasks pinned to a single node's CPUs must have `select_cpu` keep them within
/// that node, and all decisions must be valid. This exercises NUMA-aware CPU
/// selection with node-local affinity.
#[test]
fn test_select_cpu_numa_awareness() {
    let _lock = common::setup_test();
    let nr = 4;
    let node0: BTreeSet<u32> = [0, 1].into_iter().collect();
    let node1: BTreeSet<u32> = [2, 3].into_iter().collect();

    let scenario = Scenario::builder()
        .cpus(nr)
        .task(pinned("n0", 1, vec![CpuId(0), CpuId(1)]))
        .task(pinned("n1", 2, vec![CpuId(2), CpuId(3)]))
        .add_task("free", 0, waky())
        .duration_ms(150)
        .build();
    let t = Simulator::new(DynamicScheduler::cosmos_with_numa(nr, 2)).run(scenario);
    assert_eq!(
        t.exit_kind(),
        &ExitKind::Normal,
        "cosmos numa: not normal exit"
    );

    // All decisions valid.
    for (prev, sel) in all_selects(&t) {
        assert!(sel < nr, "cosmos numa: invalid CPU {sel} (prev={prev})");
    }
    // Node-pinned tasks stay within their node.
    let n0 = selected_for(&t, Pid(1));
    let n1 = selected_for(&t, Pid(2));
    assert!(
        !n0.is_empty() && !n1.is_empty(),
        "numa tasks got no select_cpu"
    );
    assert!(
        n0.iter().all(|c| node0.contains(c)),
        "node0 task select_cpu left node0: {n0:?}"
    );
    assert!(
        n1.iter().all(|c| node1.contains(c)),
        "node1 task select_cpu left node1: {n1:?}"
    );
}

// ---------------------------------------------------------------------------
// (6) The correctness invariants hold identically across all schedulers.
// ---------------------------------------------------------------------------

/// Run one identical affinity scenario under all three schedulers and confirm
/// each independently upholds the select_cpu contract: it fires, every decision
/// is valid, and the pinned task's decisions stay within its cpumask. (The
/// schedulers may pick *different* CPUs by policy; the shared requirement is
/// that all choices are valid and mask-respecting.)
#[test]
fn test_select_cpu_contract_uniform_across_schedulers() {
    let _lock = common::setup_test();
    let nr = 4;
    let allowed: BTreeSet<u32> = [0, 2].into_iter().collect();
    for (name, make) in SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(nr)
            .task(pinned("pin02", 1, vec![CpuId(0), CpuId(2)]))
            .add_task("bg0", 0, waky())
            .add_task("bg1", 0, waky())
            .duration_ms(150)
            .build();
        let t = Simulator::new(make(nr)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let all = all_selects(&t);
        assert!(!all.is_empty(), "{name}: no select_cpu decisions");
        assert!(
            all.iter().all(|(_, sel)| *sel < nr),
            "{name}: some select_cpu decision was invalid"
        );
        let pinned_sel = selected_for(&t, Pid(1));
        assert!(
            !pinned_sel.is_empty(),
            "{name}: pinned task got no select_cpu"
        );
        assert!(
            pinned_sel.iter().all(|c| allowed.contains(c)),
            "{name}: pinned select_cpu left cpumask {allowed:?}: {pinned_sel:?}"
        );
    }
}
