//! Perfetto trace-output validation across schedulers (simple, lavd, cosmos).
//!
//! The existing `perfetto.rs` / `perfetto_pb.rs` tests exercise only the
//! `simple` scheduler and focus on the wire-format / ingester invariants.
//! This file adds cross-scheduler *correctness* validation of the emitted
//! trace against the in-memory `Trace` (the source of truth):
//!
//! 1. Valid JSON (serde) and valid protobuf (`perfetto_protos::Trace`).
//! 2. Correct task IDs, CPU assignments, and timestamps — every emitted
//!    event's CPU is in range, every scheduled-task id is a real task, and
//!    each Chrome-JSON `ts` (µs) equals the underlying event `time_ns / 1000`.
//! 3. Chronological ordering — timestamps are valid (bounded to the simulated
//!    window) and each CPU's slice timeline is monotonic. (Raw emission order
//!    is engine-processing order, not globally ts-sorted; Perfetto sorts by
//!    `ts` on load — see the inline note in `validate_json`.)
//! 4. Runs for all three supported schedulers.
//! 5. Completeness — the writer emits exactly one JSON event per `Trace`
//!    event (plus per-CPU metadata), and every `TaskScheduled` becomes a
//!    slice-begin.

use std::collections::HashSet;

use perfetto_protos::{trace::Trace as TraceProto, trace_packet::trace_packet};
use protobuf::Message;
use serde_json::Value;

use scx_simulator::*;

#[macro_use]
mod common;

/// PIDs used by [`rich_scenario`]. Returned so validators can check that every
/// scheduled-task id in the trace is a real task.
const TASK_PIDS: [i32; 4] = [1, 2, 3, 4];

/// A workload that produces a rich mix of trace events: a mutually-waking
/// ping-pong pair (wake / DSQ-insert / on-cpu slices), a CPU hog (long
/// slices, preemption), and an I/O task (sleep/wake, idle CPUs). Runs on
/// `nr_cpus` CPUs.
fn rich_scenario(nr_cpus: u32) -> Scenario {
    let (ping, pong) = workloads::ping_pong(Pid(1), Pid(2), 300_000);
    let mk = |name: &str, pid: i32, nice: i8, behavior: TaskBehavior, mm: Option<MmId>| TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice,
        behavior,
        start_time_ns: 0,
        mm_id: mm,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    };
    Scenario::builder()
        .cpus(nr_cpus)
        .seed(4242)
        .task(mk("ping", 1, -5, ping, Some(MmId(1))))
        .task(mk("pong", 2, -5, pong, Some(MmId(1))))
        .task(mk("hog", 3, 0, workloads::cpu_bound(20_000_000), None))
        .task(mk(
            "io",
            4,
            0,
            workloads::io_bound(200_000, 1_000_000),
            None,
        ))
        .duration_ms(200)
        .build()
}

/// Count `TaskScheduled` events in the in-memory trace (the slice-begin
/// source of truth).
fn scheduled_count(trace: &trace::Trace) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .count()
}

/// Validate the Chrome-JSON output of `trace` for a run on `nr_cpus` CPUs.
fn validate_json(trace: &trace::Trace, nr_cpus: u32, label: &str) {
    let mut buf = Vec::new();
    trace
        .write_perfetto_json(&mut buf)
        .unwrap_or_else(|e| panic!("[{label}] write_perfetto_json failed: {e}"));

    // (1) Valid JSON with a non-empty traceEvents array.
    let parsed: Value =
        serde_json::from_slice(&buf).unwrap_or_else(|e| panic!("[{label}] invalid JSON: {e}"));
    let events = parsed["traceEvents"]
        .as_array()
        .unwrap_or_else(|| panic!("[{label}] traceEvents is not an array"));
    assert!(!events.is_empty(), "[{label}] traceEvents is empty");

    // Partition metadata ("M") vs data (everything with a "ts").
    let meta: Vec<&Value> = events.iter().filter(|e| e["ph"] == "M").collect();
    let data: Vec<&Value> = events.iter().filter(|e| e["ph"] != "M").collect();

    // (4-meta) Every CPU has a "CPU N" process_name metadata entry.
    let process_names: HashSet<&str> = meta
        .iter()
        .filter(|e| e["name"] == "process_name")
        .filter_map(|e| e["args"]["name"].as_str())
        .collect();
    for cpu in 0..nr_cpus {
        let want = format!("CPU {cpu}");
        assert!(
            process_names.contains(want.as_str()),
            "[{label}] missing process_name metadata for {want}"
        );
    }

    // (5) Completeness: the writer emits exactly one data event per Trace
    // event (metadata is extra, per-CPU). This is the strongest completeness
    // invariant — no scheduling decision is dropped from the trace.
    assert_eq!(
        data.len(),
        trace.events().len(),
        "[{label}] JSON data-event count {} != Trace event count {} (events dropped by writer)",
        data.len(),
        trace.events().len()
    );

    // (2) Direct correctness: the writer emits data events in Trace order, so
    // data[i] must faithfully reflect events()[i] — the CPU assignment and the
    // Chrome-JSON `ts` (µs) equal the source event's CPU and `time_ns / 1000`.
    for (i, (je, te)) in data.iter().zip(trace.events().iter()).enumerate() {
        let jcpu = je["pid"].as_i64().unwrap_or(-1);
        assert_eq!(
            jcpu,
            i64::from(te.cpu.0),
            "[{label}] event {i} CPU mismatch: JSON {jcpu} vs Trace {}",
            te.cpu.0
        );
        let jts = je["ts"].as_i64().unwrap_or(-1);
        assert_eq!(
            jts,
            (te.time_ns / 1000) as i64,
            "[{label}] event {i} ts mismatch: JSON {jts}µs vs Trace {}ns/1000",
            te.time_ns
        );
    }

    // (2) + (3): CPU range, timestamp validity, and chronological ordering of
    // the CPU *timeline*.
    //
    // Note on ordering: scxsim emits events in engine-processing order with
    // each event carrying its correct logical timestamp — instant "sub-events"
    // of a scheduling decision (wake, select_task_rq, dsq_insert, all logically
    // at the wake instant) are recorded *after* the slice-begin they precede,
    // so the raw emission order is deliberately NOT globally ts-sorted (the
    // Perfetto/Chrome consumer sorts by `ts` on load). What MUST hold is that a
    // single CPU's *slice timeline* is monotonic: on each CPU, successive
    // slice-begin (`B`) events are non-decreasing in time, and likewise for
    // slice-end (`E`) events — a CPU cannot start (or finish) running a task at
    // t=5 and then start (or finish) another at t=3.
    let task_pids: HashSet<i32> = TASK_PIDS.into_iter().collect();
    let duration_us = 200_000i64; // rich_scenario runs for 200 ms
    let mut last_begin_per_cpu: Vec<i64> = vec![0; nr_cpus as usize];
    let mut last_end_per_cpu: Vec<i64> = vec![0; nr_cpus as usize];
    for (i, e) in data.iter().enumerate() {
        // CPU assignment ("pid" field is the CPU index).
        let cpu = e["pid"]
            .as_i64()
            .unwrap_or_else(|| panic!("[{label}] event {i} has no integer pid/cpu: {e}"));
        assert!(
            cpu >= 0 && (cpu as u32) < nr_cpus,
            "[{label}] event {i} CPU {cpu} out of range 0..{nr_cpus}: {e}"
        );

        // Timestamp: a non-negative integer µs within the simulated window.
        let ts = e["ts"]
            .as_i64()
            .unwrap_or_else(|| panic!("[{label}] event {i} ts is not an integer: {e}"));
        assert!(ts >= 0, "[{label}] event {i} negative ts {ts}");
        assert!(
            ts <= duration_us + 1000,
            "[{label}] event {i} ts {ts}µs exceeds simulated window {duration_us}µs"
        );

        // Per-CPU slice-timeline monotonicity.
        let ph = e["ph"].as_str().unwrap_or("");
        if ph == "B" {
            let last = &mut last_begin_per_cpu[cpu as usize];
            assert!(
                ts >= *last,
                "[{label}] CPU {cpu} slice-begin not chronological at event {i}: ts {ts} < previous {last}"
            );
            *last = ts;
        } else if ph == "E" {
            let last = &mut last_end_per_cpu[cpu as usize];
            assert!(
                ts >= *last,
                "[{label}] CPU {cpu} slice-end not chronological at event {i}: ts {ts} < previous {last}"
            );
            *last = ts;
        }

        // Any event carrying a task id (`args.pid`) must reference a real task.
        if let Some(arg_pid) = e["args"]["pid"].as_i64() {
            assert!(
                task_pids.contains(&(arg_pid as i32)),
                "[{label}] event {i} references unknown task pid {arg_pid}: {e}"
            );
        }
    }

    // (2/5) Slice-begin correctness: one "B" event per TaskScheduled, each
    // with a resolved, non-empty task name.
    let begins: Vec<&Value> = data.iter().filter(|e| e["ph"] == "B").copied().collect();
    assert_eq!(
        begins.len(),
        scheduled_count(trace),
        "[{label}] B-event count {} != TaskScheduled count {}",
        begins.len(),
        scheduled_count(trace)
    );
    for b in &begins {
        let name = b["name"].as_str().unwrap_or("");
        assert!(
            !name.is_empty(),
            "[{label}] B event has empty task name: {b}"
        );
        assert_ne!(
            name, "???",
            "[{label}] B event has unresolved task name: {b}"
        );
    }

    // Ticks/idle can make begin/end asymmetric only via the final open slice,
    // which the simulator closes at SimulationEnd; begins and ends must match.
    let end_count = data.iter().filter(|e| e["ph"] == "E").count();
    assert_eq!(
        begins.len(),
        end_count,
        "[{label}] slice begin/end mismatch: {} begins vs {} ends",
        begins.len(),
        end_count
    );
}

/// Validate the protobuf output of `trace`.
fn validate_pb(trace: &trace::Trace, label: &str) {
    let mut buf = Vec::new();
    trace
        .write_perfetto_pb(&mut buf)
        .unwrap_or_else(|e| panic!("[{label}] write_perfetto_pb failed: {e}"));

    // (1) Parses with the same primitive scxtop's loader uses.
    let proto = TraceProto::parse_from_bytes(&buf)
        .unwrap_or_else(|e| panic!("[{label}] protobuf parse_from_bytes failed: {e}"));
    assert!(
        !proto.packet.is_empty(),
        "[{label}] protobuf trace has no packets"
    );

    // F1 invariant: every packet must carry trusted_packet_sequence_id, else
    // trace_processor / scxtop / ui.perfetto.dev silently drop it.
    let missing_seq = proto
        .packet
        .iter()
        .filter(|p| p.optional_trusted_packet_sequence_id.is_none())
        .count();
    assert_eq!(
        missing_seq,
        0,
        "[{label}] {missing_seq}/{} packets missing trusted_packet_sequence_id (F1 regression)",
        proto.packet.len()
    );

    // (5) ONCPU slice-begins must exist and be TrackEvents.
    let track_events = proto
        .packet
        .iter()
        .filter(|p| matches!(p.data, Some(trace_packet::Data::TrackEvent(_))))
        .count();
    assert!(track_events > 0, "[{label}] no TrackEvents emitted");
}

/// Run `rich_scenario` under `sched`, validating both JSON and protobuf.
fn run_and_validate(sched: DynamicScheduler, nr_cpus: u32, label: &str) {
    let trace = Simulator::new(sched).run(rich_scenario(nr_cpus));
    assert!(
        !trace.has_error(),
        "[{label}] simulation error: {:?}",
        trace.exit_kind()
    );
    assert!(
        scheduled_count(&trace) > 0,
        "[{label}] no tasks were scheduled"
    );
    validate_json(&trace, nr_cpus, label);
    validate_pb(&trace, label);
}

#[test]
fn test_perfetto_validation_simple() {
    let _lock = common::setup_test();
    run_and_validate(DynamicScheduler::simple(), 4, "simple");
}

#[test]
fn test_perfetto_validation_lavd() {
    let _lock = common::setup_test();
    run_and_validate(DynamicScheduler::lavd(4), 4, "lavd");
}

#[test]
fn test_perfetto_validation_cosmos() {
    let _lock = common::setup_test();
    run_and_validate(DynamicScheduler::cosmos(4), 4, "cosmos");
}
