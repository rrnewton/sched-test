//! Tests for dispatch-queue (DSQ) operations across the supported schedulers.
//!
//! DSQs are the sched_ext dispatch queues tasks flow through between enqueue
//! and running. This file validates, via the trace's DSQ events and length
//! samples, the DSQ lifecycle (create → insert → consume-to-local), the
//! local-vs-global/shared DSQ paths, vtime-ordered insertion, queue
//! backpressure under contention, and the use of multiple DSQs.
//!
//! Schedulers differ in DSQ policy, and the assertions reflect the *actual*
//! observed behavior rather than forcing a uniform model:
//!
//! - simple: inserts to a shared vtime DSQ (id 0) + a LOCAL fast-path from
//!   select_cpu, and consumes via `scx_bpf_dsq_move_to_local`.
//! - lavd:   per-cpdom vtime DSQs + LOCAL, also consumed via move-to-local.
//! - cosmos: inserts tasks directly onto CPU-local / per-domain DSQs (FIFO,
//!   no vtime, no move-to-local, no shared-DSQ backpressure).

use std::collections::BTreeSet;

use scx_simulator::*;

#[macro_use]
mod common;

fn task(name: &str, pid: i32, nice: i8, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Distinct DSQ ids that tasks were *inserted* into (DsqInsert/DsqInsertVtime).
fn distinct_insert_dsqs(trace: &Trace) -> BTreeSet<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::DsqInsert { dsq_id, .. } | TraceKind::DsqInsertVtime { dsq_id, .. } => {
                Some(dsq_id.0)
            }
            _ => None,
        })
        .collect()
}

/// Max queue length observed over every non-builtin DSQ that was inserted into.
fn max_dsq_length_any(trace: &Trace) -> usize {
    distinct_insert_dsqs(trace)
        .into_iter()
        .filter_map(|d| trace.max_dsq_length(DsqId(d)))
        .max()
        .unwrap_or(0)
}

/// `n` always-runnable CPU hogs on `cpus` CPUs — creates DSQ contention.
fn hogs(cpus: u32, n: i32, seed: u32) -> Scenario {
    let mut b = Scenario::builder()
        .cpus(cpus)
        .seed(seed)
        .detect_bpf_errors();
    for i in 0..n {
        b = b.task(task(
            &format!("hog{i}"),
            1 + i,
            0,
            workloads::cpu_bound(100_000_000),
        ));
    }
    b.duration_ms(150).build()
}

fn run(label: &str, cpus: u32, scenario: Scenario) -> Trace {
    let sched = match label {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(cpus),
        "cosmos" => DynamicScheduler::cosmos(cpus),
        _ => unreachable!(),
    };
    let trace = Simulator::new(sched).run(scenario);
    assert!(
        !trace.has_error(),
        "[{label}] simulation error: {:?}",
        trace.exit_kind()
    );
    trace
}

const ALL: [&str; 3] = ["simple", "lavd", "cosmos"];

// ---------------------------------------------------------------------------
// 1. DSQ lifecycle: every scheduler creates a DSQ, inserts tasks into it, and
//    delivers them to a CPU-local run queue for execution.
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_lifecycle_all_schedulers() {
    let _lock = common::setup_test();
    for label in ALL {
        let trace = run(label, 2, hogs(2, 6, 1));

        // Create: at least one DSQ is created at init.
        assert!(
            count(&trace, |k| matches!(k, TraceKind::CreateDsq { .. })) > 0,
            "[{label}] no DSQ was created"
        );
        // Insert: tasks are inserted into a DSQ (FIFO or vtime).
        let inserts = count(&trace, |k| {
            matches!(
                k,
                TraceKind::DsqInsert { .. } | TraceKind::DsqInsertVtime { .. }
            )
        });
        assert!(inserts > 0, "[{label}] no DSQ insertions");
        // Consume-to-local: tasks reach a CPU-local run queue, either via an
        // explicit move-to-local (simple/lavd) or by direct local insertion
        // (cosmos). Both surface as local dispatches or DsqMoveToLocal events.
        let moves = count(&trace, |k| {
            matches!(k, TraceKind::DsqMoveToLocal { success: true, .. })
        });
        let (_global, local) = trace.dsq_dispatch_counts();
        assert!(
            moves + local > 0,
            "[{label}] no tasks delivered to a local run queue (moves={moves}, local={local})"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Local DSQ fast-path: every scheduler dispatches some tasks to a CPU-local
//    DSQ (the SCX_DSQ_LOCAL fast path from select_cpu / direct local insert).
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_local_dispatch_every_scheduler() {
    let _lock = common::setup_test();
    for label in ALL {
        // Under-loaded: 2 tasks, 4 CPUs — idle CPUs favor the local fast path.
        let trace = run(label, 4, {
            let mut b = Scenario::builder().cpus(4).seed(2).detect_bpf_errors();
            for i in 0..2i32 {
                b = b.task(task(
                    &format!("t{i}"),
                    1 + i,
                    0,
                    TaskBehavior {
                        phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
                        repeat: RepeatMode::Forever,
                    },
                ));
            }
            b.duration_ms(120).build()
        });
        let (_g, local) = trace.dsq_dispatch_counts();
        assert!(
            local > 0,
            "[{label}] no local-DSQ dispatches on an under-loaded system"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Global/shared DSQ path: simple and lavd route contended tasks through a
//    shared (non-local) DSQ. (cosmos inserts directly to CPU-local DSQs, so
//    its global count is 0 by design — excluded and documented.)
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_global_shared_dispatch_simple_lavd() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd"] {
        let trace = run(label, 2, hogs(2, 12, 3));
        let (global, _local) = trace.dsq_dispatch_counts();
        assert!(
            global > 0,
            "[{label}] no global/shared-DSQ dispatches under contention (global={global})"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. vtime-ordered insertion: simple and lavd insert into a weight-scaled
//    vtime DSQ (the mechanism behind fair scheduling); the ordering key varies
//    across tasks. (cosmos uses FIFO DsqInsert — no vtime — excluded.)
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_vtime_ordering_simple_lavd() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd"] {
        // Mixed priorities so weight-scaled vtimes differ across tasks.
        let trace = run(label, 2, {
            let mut b = Scenario::builder().cpus(2).seed(4).detect_bpf_errors();
            for (i, nice) in [-10i8, -3, 0, 5, 12].into_iter().enumerate() {
                b = b.task(task(
                    &format!("t{i}"),
                    1 + i as i32,
                    nice,
                    workloads::cpu_bound(80_000_000),
                ));
            }
            b.duration_ms(150).build()
        });

        let vtimes: Vec<u64> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::DsqInsertVtime { vtime, .. } => Some(vtime.0),
                _ => None,
            })
            .collect();
        assert!(
            !vtimes.is_empty(),
            "[{label}] no vtime-ordered DSQ insertions"
        );
        // The vtime ordering key must actually vary (not a constant), i.e. the
        // DSQ is genuinely ordered, not degenerate.
        let distinct: BTreeSet<u64> = vtimes.iter().copied().collect();
        assert!(
            distinct.len() > 1,
            "[{label}] all DSQ vtimes identical ({}); ordering key is degenerate",
            vtimes[0]
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Backpressure: under heavy contention a shared/domain DSQ accumulates a
//    backlog (queue length > 1). simple and lavd expose this via non-builtin
//    DSQ length samples; cosmos dispatches to CPU-local DSQs (not sampled),
//    so it is excluded and documented.
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_backpressure_simple_lavd() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd"] {
        // 16 hogs on 2 CPUs: many tasks must wait in a DSQ at any instant.
        let trace = run(label, 2, hogs(2, 16, 5));
        let maxlen = max_dsq_length_any(&trace);
        assert!(
            maxlen >= 2,
            "[{label}] DSQ never showed backpressure (max observed length {maxlen})"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Multiple DSQs with different policies: lavd (per-cpdom DSQs) and cosmos
//    with NUMA (per-node/per-CPU DSQs) route tasks through more than one DSQ.
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_multiple_dsqs() {
    let _lock = common::setup_test();

    // lavd under contention routes tasks through a per-cpdom DSQ plus the
    // LOCAL fast-path DSQ (≥2 distinct DSQs).
    let trace = run("lavd", 4, hogs(4, 12, 6));
    let lavd_dsqs = distinct_insert_dsqs(&trace);
    assert!(
        lavd_dsqs.len() >= 2,
        "lavd used only {} DSQ(s): {lavd_dsqs:?}",
        lavd_dsqs.len()
    );

    // cosmos with 2 NUMA nodes uses a DSQ per node (+ local).
    let trace = Simulator::new(DynamicScheduler::cosmos_with_numa(4, 2)).run(hogs(4, 12, 7));
    assert!(
        !trace.has_error(),
        "cosmos_numa error: {:?}",
        trace.exit_kind()
    );
    let cosmos_dsqs = distinct_insert_dsqs(&trace);
    assert!(
        cosmos_dsqs.len() >= 2,
        "cosmos_with_numa used only {} DSQ(s): {cosmos_dsqs:?}",
        cosmos_dsqs.len()
    );
}
