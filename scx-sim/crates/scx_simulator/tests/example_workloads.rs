//! Comprehensive regression matrix over the guide example workloads.
//!
//! Every rt-app JSON workload under `scx-sim/examples/*.json` is loaded and run
//! against each supported scheduler (simple, lavd, cosmos). This is the
//! in-tree, `cargo nextest`-native counterpart to the CLI smoke test
//! `docs/guide/tests/test_examples.sh`: where the bash script asserts only
//! `scxsim run` exit code 0, this test additionally validates the in-memory
//! `Trace` and the emitted Perfetto output. Per (example × scheduler) cell it
//! checks:
//!
//!   1. **Parse** — the rt-app JSON loads into a `Scenario` (loader regression
//!      guard).
//!   2. **No crash / panic / hang** — the simulation reaches a `Normal` exit:
//!      no `scx_bpf_error()` (`ErrorBpf`), no stall-watchdog trip
//!      (`ErrorStall`), no dispatch-loop exhaustion
//!      (`ErrorDispatchLoopExhausted`), no cgroup exhaustion.
//!   3. **Liveness** — the `Trace` is non-empty and every task declared in the
//!      workload is scheduled at least once within the run window.
//!   4. **Valid traces** — the Perfetto output is well-formed: the Chrome-JSON
//!      has a non-empty `traceEvents` array and the Perfetto protobuf decodes
//!      with a non-empty packet stream.
//!
//! Examples are discovered at runtime from `examples/`, so a newly added
//! `examples/*.json` is covered automatically with no wiring here (mirroring
//! the bash script's `find`-based discovery).
//!
//! The whole matrix runs even when a cell fails: every failure is collected and
//! reported together at the end, so one broken workload/scheduler combination
//! doesn't mask the others.

use perfetto_protos::trace::Trace as TraceProto;
use protobuf::Message;
use scx_simulator::*;
use serde_json::Value;

mod common;

/// Simulated CPU count for every run (matches the bash smoke test default).
const CPUS: u32 = 4;

/// Simulated duration per run. Overrides each example's native
/// `global.duration` to keep the matrix fast while still exercising the full
/// enqueue/dispatch/tick machinery. Matches `SCXSIM_TEST_DURATION`'s 100ms
/// default in `docs/guide/tests/test_examples.sh`.
const TEST_DURATION_NS: TimeNs = 100_000_000;

/// The supported schedulers, keyed by their CLI name.
const SCHEDULERS: [&str; 3] = ["simple", "lavd", "cosmos"];

/// Locate the `scx-sim/examples/` directory relative to this crate.
///
/// `CARGO_MANIFEST_DIR` is `scx-sim/crates/scx_simulator`; the examples live
/// two levels up under `examples/`.
fn examples_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples directory should exist")
}

/// Discover every `*.json` workload under `examples/`, sorted for deterministic
/// iteration order.
fn discover_examples() -> Vec<std::path::PathBuf> {
    let dir = examples_dir();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no *.json examples found under {}",
        dir.display()
    );
    files
}

/// Construct a fresh scheduler by CLI name. A fresh instance is built
/// immediately before each run because only one C scheduler may be alive at a
/// time.
fn make_scheduler(label: &str) -> DynamicScheduler {
    match label {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(CPUS),
        "cosmos" => DynamicScheduler::cosmos(CPUS),
        other => panic!("unknown scheduler {other}"),
    }
}

/// Run one (example × scheduler) cell. Returns `Ok(())` on success or
/// `Err(reason)` describing the first failed check for this cell.
fn run_cell(label: &str, name: &str, json: &str) -> Result<(), String> {
    // (1) Parse.
    let mut scenario = load_rtapp(json, CPUS)
        .map_err(|e| format!("[{label}] {name}: rt-app parse failed: {e}"))?;

    // Keep the run short and bounded; keep the stall watchdog enabled (it is on
    // by default from the rt-app loader) so a genuine hang surfaces as an
    // `ErrorStall` rather than spinning.
    scenario.duration_ns = TEST_DURATION_NS;

    // Capture task PIDs before the scenario is consumed by `run`.
    let pids: Vec<Pid> = scenario.tasks.iter().map(|t| t.pid).collect();

    // (2) No crash / panic / hang: run to a Normal exit.
    let sched = make_scheduler(label);
    let trace = Simulator::new(sched).run(scenario);
    if trace.has_error() {
        return Err(format!(
            "[{label}] {name}: simulation error: {:?}",
            trace.exit_kind()
        ));
    }

    // (3) Liveness: non-empty trace, every task scheduled at least once.
    if trace.events().is_empty() {
        return Err(format!("[{label}] {name}: trace has no events"));
    }
    for pid in &pids {
        if trace.schedule_count(*pid) == 0 {
            return Err(format!(
                "[{label}] {name}: task {pid:?} was never scheduled"
            ));
        }
    }

    // (4) Valid traces: Chrome-JSON and Perfetto protobuf both well-formed.
    validate_perfetto(&trace, label, name)?;

    Ok(())
}

/// Validate the emitted Perfetto trace outputs for one cell.
fn validate_perfetto(trace: &trace::Trace, label: &str, name: &str) -> Result<(), String> {
    // Chrome-JSON: parseable, with a non-empty `traceEvents` array.
    let mut json_buf = Vec::new();
    trace
        .write_perfetto_json(&mut json_buf)
        .map_err(|e| format!("[{label}] {name}: perfetto JSON write failed: {e}"))?;
    let parsed: Value = serde_json::from_slice(&json_buf)
        .map_err(|e| format!("[{label}] {name}: perfetto JSON is not valid JSON: {e}"))?;
    let events = parsed["traceEvents"]
        .as_array()
        .ok_or_else(|| format!("[{label}] {name}: perfetto JSON traceEvents is not an array"))?;
    if events.is_empty() {
        return Err(format!(
            "[{label}] {name}: perfetto JSON traceEvents is empty"
        ));
    }

    // Perfetto protobuf: decodable, with a non-empty packet stream.
    let mut pb_buf = Vec::new();
    trace
        .write_perfetto_pb(&mut pb_buf)
        .map_err(|e| format!("[{label}] {name}: perfetto pb write failed: {e}"))?;
    let proto = TraceProto::parse_from_bytes(&pb_buf)
        .map_err(|e| format!("[{label}] {name}: perfetto pb is not decodable: {e}"))?;
    if proto.packet.is_empty() {
        return Err(format!("[{label}] {name}: perfetto pb has no packets"));
    }

    Ok(())
}

/// Full regression matrix: every `examples/*.json` × {simple, lavd, cosmos}.
///
/// Runs every cell, collecting all failures, and reports them together so a
/// single broken combination doesn't hide the rest.
#[test]
fn test_all_examples_all_schedulers() {
    let _lock = common::setup_test();

    let examples = discover_examples();
    let mut failures: Vec<String> = Vec::new();
    let mut passed = 0usize;
    let mut total = 0usize;

    for label in SCHEDULERS {
        for path in &examples {
            total += 1;
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>");
            let json = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("[{label}] {name}: cannot read file: {e}"));
                    continue;
                }
            };
            match run_cell(label, name, &json) {
                Ok(()) => {
                    passed += 1;
                    eprintln!("PASS [{label}] {name}");
                }
                Err(reason) => {
                    eprintln!("FAIL {reason}");
                    failures.push(reason);
                }
            }
        }
    }

    eprintln!(
        "\nexample-workload matrix: {passed}/{total} cells passed \
         ({} examples × {} schedulers)",
        examples.len(),
        SCHEDULERS.len()
    );

    assert!(
        failures.is_empty(),
        "{} of {total} example/scheduler cells failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
