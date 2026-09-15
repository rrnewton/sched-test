//! Comprehensive regression matrix: every `scx-sim/examples/*.json` workload
//! run through every scheduler (simple, lavd, cosmos, layered), asserting no parse
//! failure, no panic/crash, no hang, and a valid (non-empty, cleanly
//! terminated) trace.
//!
//! This is the in-process Rust complement to the CLI-driven
//! `docs/guide/tests/test_examples.sh` / `make test-examples` (which shells out
//! to the `scxsim` binary). Running in-process lets us assert on the structured
//! `ExitKind` and trace rather than just the process exit code, and covers the
//! example × scheduler matrix in a single deterministic test.
//!
//! Examples are auto-discovered at runtime from the crate-relative
//! `../../examples` directory (via `CARGO_MANIFEST_DIR`), so new examples are
//! picked up with no further wiring — matching the auto-discovery contract
//! documented in `examples/README.md`.
//!
//! "Hang" protection: the discrete-event engine is bounded by the workload's
//! `global.duration`, and the watchdog (default 30s simulated) plus the
//! dispatch-loop-exhaustion guard turn any runaway scheduler into a terminal
//! `ExitKind::Error*` rather than an infinite loop — so a hang surfaces as a
//! reported failure, not a wedged test.

use std::path::{Path, PathBuf};

use scx_simulator::*;

mod common;

/// The schedulers under test. `simple` takes no CPU count; the others are
/// constructed with the scenario's CPU count.
const SCHEDULERS: [&str; 4] = ["simple", "lavd", "cosmos", "layered"];

/// CPUs to run every example with (examples are authored for `--cpus 4`).
const NR_CPUS: u32 = 4;

/// Cap the simulated duration (mirrors `make test-examples` / the shell
/// example test's `--duration 100ms`) so the 21-run matrix stays fast while
/// still exercising each combination end-to-end.
const DURATION_CAP_NS: u64 = 100_000_000;

/// Load an example and cap its duration for speed. Parse errors are stringified
/// (the concrete error type is not re-exported from the crate).
fn load_example(json: &str) -> Result<Scenario, String> {
    let mut scenario = load_rtapp(json, NR_CPUS).map_err(|e| format!("{e:?}"))?;
    scenario.duration_ns = scenario.duration_ns.min(DURATION_CAP_NS);
    Ok(scenario)
}

/// Combinations that are known and accepted to NOT exit `Normal`, with a reason.
/// Empty means "every combination must exit Normal." Populated only for
/// genuinely documented, understood deviations.
const KNOWN_NON_NORMAL: &[(&str, &str, &str)] = &[
    // (example_file, scheduler, reason)
];

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/scx-sim/crates/scx_simulator (compile-time,
    // absolute), so ../../examples resolves to <repo>/scx-sim/examples.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Sorted list of `*.json` example paths (deterministic order).
fn example_files() -> Vec<PathBuf> {
    let dir = examples_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read examples dir {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
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

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<?>")
        .to_string()
}

fn is_known_non_normal(example: &str, sched: &str) -> Option<&'static str> {
    KNOWN_NON_NORMAL
        .iter()
        .find(|(e, s, _)| *e == example && *s == sched)
        .map(|(_, _, reason)| *reason)
}

/// Every example × every scheduler must parse, run without panic/hang, produce
/// a non-empty trace, and exit `Normal` (unless explicitly allowlisted).
#[test]
fn test_all_examples_all_schedulers() {
    let _lock = common::setup_test();

    let files = example_files();
    // Guard against a path/discovery bug silently turning this into a no-op.
    assert!(
        files.len() >= 5,
        "expected to discover the example workloads, found {} in {}",
        files.len(),
        examples_dir().display()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;

    for path in &files {
        let name = file_name(path);
        let json = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

        for sched_name in SCHEDULERS {
            ran += 1;

            // 1. Must parse as rt-app JSON.
            let scenario = match load_example(&json) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("{name} x {sched_name}: parse error: {e}"));
                    continue;
                }
            };

            // 2. Must run without panicking (a panic fails the test outright)
            //    and produce a trace.
            let sched = make_scheduler(sched_name, NR_CPUS);
            let trace = Simulator::new(sched).run(scenario);
            let exit = trace.exit_kind().clone();
            let n_events = trace.events().len();

            eprintln!("{name:28} x {sched_name:7} -> {exit:?} ({n_events} events)");

            // 3. Trace must be non-empty (the sim actually did something).
            if n_events == 0 {
                failures.push(format!("{name} x {sched_name}: empty trace"));
                continue;
            }

            // 4. Must exit cleanly (Normal), unless documented.
            if exit != ExitKind::Normal {
                match is_known_non_normal(&name, sched_name) {
                    Some(reason) => {
                        eprintln!("    (accepted non-Normal: {reason})");
                    }
                    None => {
                        failures.push(format!("{name} x {sched_name}: exit {exit:?}"));
                    }
                }
            }
        }
    }

    eprintln!(
        "examples matrix: {} files x {} schedulers = {ran} runs, {} failures",
        files.len(),
        SCHEDULERS.len(),
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "example/scheduler regressions:\n  {}",
        failures.join("\n  ")
    );
}

/// Every example must parse into a well-formed scenario (correct CPU count and
/// at least one task) under the rt-app loader — an isolated, fast guard so a
/// malformed example is reported distinctly from a run-time failure.
#[test]
fn test_all_examples_parse() {
    let _lock = common::setup_test();
    let files = example_files();
    assert!(
        files.len() >= 5,
        "expected to discover example workloads, found {}",
        files.len()
    );
    for path in &files {
        let name = file_name(path);
        let json = std::fs::read_to_string(path).unwrap();
        let scenario = load_example(&json).unwrap_or_else(|e| panic!("{name}: parse error: {e}"));
        assert_eq!(scenario.nr_cpus, NR_CPUS, "{name}: wrong cpu count");
        assert!(!scenario.tasks.is_empty(), "{name}: parsed to zero tasks");
    }
}
