//! Cross-scheduler comparison harness: run an IDENTICAL workload through
//! `simple`, `lavd`, and `cosmos` and verify (a) all three produce valid
//! results, (b) they exhibit genuinely different scheduling behavior, and
//! (c) documented directional differences hold — across a small (4 CPU) and a
//! large (32 CPU) topology.
//!
//! Distinct from `compare.rs` (which compares the simulator against real
//! bpftrace output for fidelity) — this compares the three schedulers against
//! each other on the same input.
//!
//! Expected behavioral differences (documented, and asserted where robust):
//! - `simple`: single global `SHARED_DSQ` (vtime) plus an idle-CPU direct-
//!   dispatch fast path; no active cross-CPU load balancing. Routes contended
//!   tasks through the global DSQ.
//! - `lavd`: latency-aware vtime with tick + slice preemption and ACTIVE load
//!   balancing / task stealing across CPUs/domains → migrates readily.
//! - `cosmos`: favors `prev_cpu` cache affinity and is migration-averse → far
//!   fewer task migrations than lavd on the same workload.
//!
//! Deterministic serial engine (fixed seed + instant_timing); metrics are
//! reproducible per (scheduler, topology).

use std::collections::HashSet;

use scx_simulator::*;

mod common;

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Comparable per-run metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Metrics {
    normal: bool,
    errored: bool,
    events: usize,
    preemptions: usize,
    migrations: usize,
    distinct_cpus: usize,
    global_dsq: usize,
    local_dsq: usize,
    all_scheduled: bool,
    total_runtime: u64,
}

/// Build the shared comparison workload: `nr_tasks` CPU-bound forever tasks
/// (oversubscribed relative to CPUs) so placement, preemption, and balancing
/// behavior all come into play. Identical across schedulers.
fn workload(nr_cpus: u32, nr_tasks: u32, duration_ms: u64) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 1..=nr_tasks {
        b = b.add_task(&format!("t{i}"), 0, forever_run(50_000_000));
    }
    b.duration_ms(duration_ms).build()
}

fn make_scheduler(name: &str, nr_cpus: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(nr_cpus),
        "cosmos" => DynamicScheduler::cosmos(nr_cpus),
        "layered" => DynamicScheduler::layered(nr_cpus),
        other => panic!("unknown scheduler {other}"),
    }
}

fn migration_count(trace: &Trace, pid: Pid) -> usize {
    let cpus: Vec<CpuId> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.cpu),
            _ => None,
        })
        .collect();
    cpus.windows(2).filter(|w| w[0] != w[1]).count()
}

fn collect(trace: &Trace, nr_tasks: u32) -> Metrics {
    let distinct_cpus: HashSet<CpuId> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu),
            _ => None,
        })
        .collect();
    let (global_dsq, local_dsq) = trace.dsq_dispatch_counts();
    let preemptions = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskPreempted { .. }))
        .count();
    let migrations: usize = (1..=nr_tasks as i32)
        .map(|p| migration_count(trace, Pid(p)))
        .sum();
    let all_scheduled = (1..=nr_tasks as i32).all(|p| trace.schedule_count(Pid(p)) > 0);
    let total_runtime = (1..=nr_tasks as i32)
        .map(|p| trace.total_runtime(Pid(p)))
        .sum();

    Metrics {
        normal: trace.exit_kind() == &ExitKind::Normal,
        errored: trace.has_error(),
        events: trace.events().len(),
        preemptions,
        migrations,
        distinct_cpus: distinct_cpus.len(),
        global_dsq,
        local_dsq,
        all_scheduled,
        total_runtime,
    }
}

/// Run the shared workload under every compared scheduler on `nr_cpus`.
fn run_all(nr_cpus: u32, nr_tasks: u32, duration_ms: u64) -> Vec<(&'static str, Metrics)> {
    ["simple", "lavd", "cosmos", "layered"]
        .iter()
        .map(|&name| {
            let trace = Simulator::new(make_scheduler(name, nr_cpus)).run(workload(
                nr_cpus,
                nr_tasks,
                duration_ms,
            ));
            (name, collect(&trace, nr_tasks))
        })
        .collect()
}

fn report(ctx: &str, results: &[(&'static str, Metrics)]) {
    eprintln!("=== scheduler comparison: {ctx} ===");
    eprintln!(
        "{:8} {:>8} {:>7} {:>6} {:>6} {:>8} {:>8}",
        "sched", "events", "preempt", "migr", "cpus", "gDSQ", "lDSQ"
    );
    for (name, m) in results {
        eprintln!(
            "{:8} {:>8} {:>7} {:>6} {:>6} {:>8} {:>8}",
            name, m.events, m.preemptions, m.migrations, m.distinct_cpus, m.global_dsq, m.local_dsq
        );
    }
}

/// Assert every scheduler produced a valid result (Normal, no error, every task
/// scheduled, nonzero aggregate runtime).
fn assert_all_valid(ctx: &str, results: &[(&'static str, Metrics)]) {
    for (name, m) in results {
        assert!(m.normal, "{ctx}: {name} did not exit Normal");
        assert!(!m.errored, "{ctx}: {name} raised an error");
        assert!(m.all_scheduled, "{ctx}: {name} starved a task");
        assert!(m.total_runtime > 0, "{ctx}: {name} produced no runtime");
        assert!(m.events > 0, "{ctx}: {name} produced an empty trace");
    }
}

// ---------------------------------------------------------------------------
// 1+2. All three schedulers produce valid results — small (4 CPU) topology.
// ---------------------------------------------------------------------------

#[test]
fn test_all_schedulers_valid_small_topology() {
    let _lock = common::setup_test();
    let results = run_all(4, 12, 200);
    report("4 CPUs / 12 tasks", &results);
    assert_all_valid("4cpu", &results);
}

// ---------------------------------------------------------------------------
// 5. Large (32 CPU) topology.
// ---------------------------------------------------------------------------

#[test]
fn test_all_schedulers_valid_large_topology() {
    let _lock = common::setup_test();
    let nr_cpus = 32;
    let nr_tasks = 48;
    // Enough duration for all oversubscribed tasks to rotate onto a CPU at
    // least once under every scheduler's slice/balancing policy.
    let results = run_all(nr_cpus, nr_tasks, 400);
    report("32 CPUs / 48 tasks", &results);
    assert_all_valid("32cpu", &results);

    // On a large box a saturating workload must spread across many CPUs (not
    // pile onto a few) for every scheduler.
    for (name, m) in &results {
        assert!(
            m.distinct_cpus >= 8,
            "32cpu: {name} only used {} distinct CPUs (expected wide spread)",
            m.distinct_cpus
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The schedulers genuinely differ in behavior.
// ---------------------------------------------------------------------------

/// The three schedulers must NOT be interchangeable: on the identical workload
/// at least one comparable metric differs between them (otherwise they'd be the
/// same scheduler). This is the robust "behavioral difference" check.
#[test]
fn test_schedulers_behaviorally_differ() {
    let _lock = common::setup_test();
    let results = run_all(4, 12, 200);
    report("4 CPUs / 12 tasks (difference check)", &results);
    assert_all_valid("differ", &results);

    let simple = &results[0].1;
    let lavd = &results[1].1;
    let cosmos = &results[2].1;

    // No two schedulers should be metric-identical across the board.
    assert_ne!(simple, lavd, "simple and lavd produced identical metrics");
    assert_ne!(
        simple, cosmos,
        "simple and cosmos produced identical metrics"
    );
    assert_ne!(lavd, cosmos, "lavd and cosmos produced identical metrics");

    // At least the event stream sizes differ across the three (different
    // amounts of scheduler work for the same input).
    let event_counts: HashSet<usize> = results.iter().map(|(_, m)| m.events).collect();
    assert!(
        event_counts.len() >= 2,
        "expected differing event counts across schedulers, got {event_counts:?}"
    );
}

// ---------------------------------------------------------------------------
// 3 (cont.). Documented directional differences that hold robustly.
// ---------------------------------------------------------------------------

/// Placement/migration policy: LAVD actively balances/steals across CPUs, while
/// COSMOS favors cache affinity — so on the same contended workload LAVD
/// migrates at least as much as COSMOS (in practice far more).
#[test]
fn test_lavd_migrates_at_least_as_much_as_cosmos() {
    let _lock = common::setup_test();
    let results = run_all(4, 12, 200);
    let lavd = &results[1].1;
    let cosmos = &results[2].1;
    eprintln!(
        "migration policy: lavd={} cosmos={}",
        lavd.migrations, cosmos.migrations
    );
    assert!(
        lavd.migrations >= cosmos.migrations,
        "expected LAVD (active balancing) to migrate >= COSMOS (cache-affinity): lavd={} cosmos={}",
        lavd.migrations,
        cosmos.migrations
    );
}

/// `simple` routes contended tasks through the global `SHARED_DSQ` (vtime), so
/// under oversubscription it records global-DSQ dispatches.
#[test]
fn test_simple_uses_global_shared_dsq_under_contention() {
    let _lock = common::setup_test();
    let results = run_all(4, 12, 200);
    let simple = &results[0].1;
    eprintln!(
        "simple DSQ dispatches: global={} local={}",
        simple.global_dsq, simple.local_dsq
    );
    assert!(
        simple.global_dsq > 0,
        "expected simple to use the global SHARED_DSQ under contention, got {}",
        simple.global_dsq
    );
}

/// Under heavy oversubscription every scheduler must preempt (no scheduler can
/// let one task monopolize a CPU forever while others are runnable).
#[test]
fn test_all_schedulers_preempt_under_oversubscription() {
    let _lock = common::setup_test();
    let results = run_all(4, 12, 200);
    for (name, m) in &results {
        assert!(
            m.preemptions > 0,
            "{name} never preempted under 12-tasks/4-CPUs oversubscription"
        );
    }
}

// ---------------------------------------------------------------------------
// No-flake guard: each scheduler is deterministic per (scheduler, topology).
// ---------------------------------------------------------------------------

#[test]
fn test_comparison_is_deterministic() {
    let _lock = common::setup_test();
    let a = run_all(4, 12, 150);
    let b = run_all(4, 12, 150);
    assert_eq!(
        a, b,
        "cross-scheduler comparison metrics are not reproducible"
    );
}
