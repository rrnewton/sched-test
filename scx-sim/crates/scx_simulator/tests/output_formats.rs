//! Simulator output-format tests (tg test-sim-output-formats).
//!
//! Covers the correctness/robustness of the simulator's output surfaces beyond
//! the existing per-format suites (`perfetto.rs` / `perfetto_pb.rs` /
//! `perfetto_validation.rs` focus on the Perfetto wire format; `csv_experiment.rs`
//! on CSV metrics). This file adds:
//!   * an all-formats-at-once validity check on a minimal run, across schedulers
//!     (Perfetto JSON, Perfetto protobuf, structops JSONL);
//!   * `TraceStats` summary field completeness + `print_summary()` no-panic;
//!   * output written to real files (creation + re-parse round-trip);
//!   * the empty-simulation (no tasks) edge — which the builder rejects.
//!
//! ## Note on item 5 (empty simulations)
//! A zero-task scenario is a rejected configuration: `ScenarioBuilder::build`
//! panics with "scenario must have at least one task" (see `safe/scenario.rs`).
//! So the "empty simulation" case is tested as a rejected build; the smallest
//! *valid* simulation (one task) is used to check that every formatter produces
//! well-formed, non-empty output at the minimal edge.

use serde_json::Value;
use std::io::Read;

use scx_simulator::*;

#[macro_use]
mod common;

type SchedFactory = fn(u32) -> DynamicScheduler;

fn schedulers() -> [(&'static str, SchedFactory); 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

/// A small but non-trivial scenario: a CPU-bound task and a run/sleep cycler.
fn small_scenario(nr_cpus: u32) -> (Scenario, Vec<Pid>) {
    let scenario = Scenario::builder()
        .cpus(nr_cpus)
        .seed(1)
        .add_task(
            "hog",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(8_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .add_task(
            "cycler",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .duration_ms(60)
        .build();
    (scenario, vec![Pid(1), Pid(2)])
}

/// Item 5: an empty simulation (no tasks) is a rejected configuration.
#[test]
#[should_panic(expected = "at least one task")]
fn empty_scenario_no_tasks_rejected() {
    let _ = Scenario::builder().cpus(2).duration_ms(10).build();
}

/// Items 1/2/3/5: every output formatter produces valid, non-empty output on a
/// minimal run, for every scheduler.
#[test]
fn all_output_formats_valid_across_schedulers() {
    let _lock = common::setup_test();
    let nr = 2;

    for (name, make) in schedulers() {
        let (scenario, _pids) = small_scenario(nr);
        let trace = Simulator::new(make(nr)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        // --- Perfetto JSON ---
        let mut json_buf = Vec::new();
        trace
            .write_perfetto_json(&mut json_buf)
            .unwrap_or_else(|e| panic!("{name}: write_perfetto_json failed: {e}"));
        assert!(!json_buf.is_empty(), "{name}: empty perfetto JSON");
        let parsed: Value = serde_json::from_slice(&json_buf)
            .unwrap_or_else(|e| panic!("{name}: invalid perfetto JSON: {e}"));
        let events = parsed["traceEvents"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: perfetto JSON missing traceEvents array"));
        assert!(!events.is_empty(), "{name}: perfetto traceEvents empty");
        // Every event must carry the Chrome-trace required fields.
        for (i, ev) in events.iter().enumerate() {
            assert!(ev.get("ph").is_some(), "{name}: event {i} missing 'ph'");
            assert!(ev.get("pid").is_some(), "{name}: event {i} missing 'pid'");
        }

        // --- Perfetto protobuf ---
        let mut pb_buf = Vec::new();
        trace
            .write_perfetto_pb(&mut pb_buf)
            .unwrap_or_else(|e| panic!("{name}: write_perfetto_pb failed: {e}"));
        assert!(!pb_buf.is_empty(), "{name}: empty perfetto protobuf");

        // --- structops JSONL ---
        let mut jsonl_buf = Vec::new();
        scx_simulator::structops_jsonl::write_jsonl(&trace, &mut jsonl_buf)
            .unwrap_or_else(|e| panic!("{name}: write_jsonl failed: {e}"));
        let jsonl = String::from_utf8(jsonl_buf).expect("jsonl utf8");
        let mut n_lines = 0;
        for (i, line) in jsonl.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let _: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{name}: jsonl line {i} invalid JSON: {e}\n{line}"));
            n_lines += 1;
        }
        assert!(n_lines > 0, "{name}: JSONL output had no records");
    }
}

/// Item 1/3: the `TraceStats` summary contains all expected fields, populated
/// consistently, and `print_summary()` does not panic.
#[test]
fn trace_stats_summary_fields_complete() {
    let _lock = common::setup_test();
    let nr = 4;
    let (scenario, pids) = small_scenario(nr);
    let trace = Simulator::new(DynamicScheduler::lavd(nr)).run(scenario);

    let stats = scx_simulator::stats::TraceStats::from_trace(&trace);

    // Duration is recorded and positive.
    assert!(stats.duration_ns > 0, "stats.duration_ns not populated");

    // Per-task stats exist for every task that ran.
    for &pid in &pids {
        assert!(
            stats.tasks.contains_key(&pid),
            "stats.tasks missing entry for {pid:?}"
        );
    }

    // Per-CPU stats exist for the CPUs.
    assert!(!stats.cpus.is_empty(), "stats.cpus is empty");

    // The dispatch histogram's totals are consistent with the individual
    // dispatch counters (both count DSQ inserts).
    let histogram_total: usize = stats.dsq_dispatch_histogram.values().sum();
    let counter_total = stats.dsq_insert_count + stats.dsq_insert_vtime_count;
    assert_eq!(
        histogram_total, counter_total,
        "dispatch histogram total ({histogram_total}) != insert counters ({counter_total})"
    );

    // The human summary must render without panicking.
    stats.print_summary();
}

/// Item 4: output written to real files is created, non-empty, and re-parses.
#[test]
fn output_written_to_files_roundtrip() {
    let _lock = common::setup_test();
    let nr = 2;
    let (scenario, _pids) = small_scenario(nr);
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    let dir = std::env::temp_dir();
    // nextest runs each test in its own process, so the pid uniquely names files.
    let tag = std::process::id();
    let json_path = dir.join(format!("scxsim_out_{tag}.perfetto.json"));
    let jsonl_path = dir.join(format!("scxsim_out_{tag}.structops.jsonl"));

    // Write Perfetto JSON to a file.
    {
        let mut f = std::fs::File::create(&json_path).expect("create json file");
        trace
            .write_perfetto_json(&mut f)
            .expect("write perfetto json");
    }
    // Write structops JSONL to a file.
    {
        let mut f = std::fs::File::create(&jsonl_path).expect("create jsonl file");
        scx_simulator::structops_jsonl::write_jsonl(&trace, &mut f).expect("write jsonl");
    }

    // Both files exist and are non-empty.
    for p in [&json_path, &jsonl_path] {
        let meta =
            std::fs::metadata(p).unwrap_or_else(|e| panic!("output file {p:?} not created: {e}"));
        assert!(meta.len() > 0, "output file {p:?} is empty");
    }

    // Re-read the JSON file and confirm it parses with a traceEvents array.
    let mut contents = String::new();
    std::fs::File::open(&json_path)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    let parsed: Value = serde_json::from_str(&contents).expect("file JSON re-parse");
    assert!(
        parsed["traceEvents"].as_array().is_some(),
        "round-tripped perfetto file missing traceEvents"
    );

    // Clean up.
    let _ = std::fs::remove_file(&json_path);
    let _ = std::fs::remove_file(&jsonl_path);
}
