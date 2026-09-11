//! End-to-end tests for the `scxsim` command-line interface.
//!
//! These spawn the real built `scxsim` binary as a subprocess (via
//! `env!("CARGO_BIN_EXE_scxsim")`) and assert on exit codes, error messages,
//! and produced trace files — exercising the actual clap parser + dispatch, not
//! an in-process reimplementation. This complements the parser-level unit tests
//! inside `src/bin/scxsim/main.rs` (`Cli::try_parse_from` flag-conflict checks),
//! which cannot cover process exit codes, custom validation errors, determinism,
//! or trace-file output.
//!
//! Every invocation passes `--no-disable-aslr` so the binary skips its
//! ASLR-disable re-exec (which requires `personality(2)` and would otherwise
//! fork a child), matching the pattern in `bug1_canonical_repro.rs`. All runs
//! use a tiny workload and a short `--duration` to stay fast.
//!
//! Exit-code scheme under test (documented in `main.rs`): `0` normal, `1`
//! generic CLI/IO error (missing workload, unknown scheduler, unreadable file),
//! `2` clap usage error (unknown flag, out-of-range value, flag conflict,
//! missing subcommand).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// A small, valid rt-app workload shipped in the crate (2 tasks). We always
/// override its duration with `--duration` to keep runs short.
const WORKLOAD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/workloads/two_runners.json");

/// The five schedulers scxsim links in (see `--list-schedulers`).
const SCHEDULERS: [&str; 5] = ["simple", "lavd", "cosmos", "mitosis", "tickless"];

/// Captured result of one `scxsim` invocation.
struct CliOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run `scxsim <args...>`, always prefixed with `--no-disable-aslr`, and capture
/// the exit code + streams. Panics only if the process could not be spawned or
/// was killed by a signal (no stable exit code).
fn run_cli(args: &[&str]) -> CliOutput {
    let exe = env!("CARGO_BIN_EXE_scxsim");
    let mut cmd = Command::new(exe);
    cmd.arg("--no-disable-aslr");
    cmd.args(args);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn scxsim ({exe}): {e}"));
    let code = out
        .status
        .code()
        .unwrap_or_else(|| panic!("scxsim terminated by signal: {:?}", out.status));
    CliOutput {
        code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A unique temp path for a per-test output file (no external tempfile dep).
fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "scxsim_cli_test_{}_{}.json",
        std::process::id(),
        tag
    ));
    p
}

/// Distinct CPU indices named in a Chrome-trace JSON (`"name":"CPU <n>"`), i.e.
/// the simulated topology width the run actually used.
fn distinct_cpu_ids(json: &str) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    for chunk in json.split("\"CPU ").skip(1) {
        if let Some(end) = chunk.find('"') {
            if let Ok(n) = chunk[..end].trim().parse::<u32>() {
                ids.insert(n);
            }
        }
    }
    ids
}

// ===========================================================================
// 1. Required arguments are validated.
// ===========================================================================

/// `run` with no workload path is a usage error: the binary reports the missing
/// required argument and exits with the generic-error code (1), not success.
#[test]
fn test_run_requires_workload() {
    let out = run_cli(&["run"]);
    assert_eq!(out.code, 1, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("missing required argument") && out.stderr.contains("WORKLOAD"),
        "expected a missing-workload message, got stderr:\n{}",
        out.stderr
    );
}

/// Invoking `scxsim` with no subcommand at all is a clap usage error (exit 2)
/// and names the available subcommands.
#[test]
fn test_missing_subcommand_rejected() {
    let out = run_cli(&[]);
    assert_eq!(out.code, 2, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("subcommand"),
        "expected a 'requires a subcommand' message, got stderr:\n{}",
        out.stderr
    );
}

// ===========================================================================
// 2. --scheduler flag with each scheduler.
// ===========================================================================

/// Every linked-in scheduler must be selectable via `--scheduler` and complete
/// a short run cleanly (exit 0). This is the real end-to-end load+run path for
/// each scheduler `.so`.
#[test]
fn test_each_scheduler_runs() {
    for sched in SCHEDULERS {
        let out = run_cli(&["run", WORKLOAD, "-s", sched, "--duration", "10ms"]);
        assert_eq!(
            out.code, 0,
            "scheduler {sched} did not run cleanly (exit {});\nstderr:\n{}",
            out.code, out.stderr
        );
    }
}

/// `--list-schedulers` enumerates all five schedulers on stdout and exits 0,
/// without needing a workload.
#[test]
fn test_list_schedulers() {
    let out = run_cli(&["run", "--list-schedulers"]);
    assert_eq!(out.code, 0, "stderr:\n{}", out.stderr);
    for sched in SCHEDULERS {
        assert!(
            out.stdout.contains(sched),
            "--list-schedulers omitted {sched}; stdout:\n{}",
            out.stdout
        );
    }
}

// ===========================================================================
// 3. --seed flag for determinism.
// ===========================================================================

/// Two runs with the *same* `--seed` produce byte-identical trace output, and a
/// run with a *different* seed diverges — the seed fully controls the simulated
/// nondeterminism (tick jitter, overhead noise, event tiebreaking).
#[test]
fn test_seed_determinism() {
    let a = temp_path("seed_a");
    let b = temp_path("seed_b");
    let c = temp_path("seed_c");
    let base = |seed: &str, out: &PathBuf| {
        vec![
            "run".to_string(),
            WORKLOAD.to_string(),
            "-s".to_string(),
            "lavd".to_string(),
            "--duration".to_string(),
            "20ms".to_string(),
            "--seed".to_string(),
            seed.to_string(),
            "--perfetto".to_string(),
            out.to_string_lossy().into_owned(),
        ]
    };
    let run = |seed: &str, out: &PathBuf| {
        let args = base(seed, out);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run_cli(&refs);
        assert_eq!(r.code, 0, "seed run failed; stderr:\n{}", r.stderr);
    };
    run("123", &a);
    run("123", &b);
    run("999", &c);

    let ta = std::fs::read(&a).expect("trace a");
    let tb = std::fs::read(&b).expect("trace b");
    let tc = std::fs::read(&c).expect("trace c");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    let _ = std::fs::remove_file(&c);

    assert_eq!(
        ta,
        tb,
        "same --seed produced different traces ({} vs {} bytes)",
        ta.len(),
        tb.len()
    );
    assert_ne!(ta, tc, "different --seed produced identical traces");
}

/// The special `--seed entropy` value (OS randomness) is accepted and runs.
#[test]
fn test_seed_entropy_accepted() {
    let out = run_cli(&[
        "run",
        WORKLOAD,
        "-s",
        "simple",
        "--duration",
        "10ms",
        "--seed",
        "entropy",
    ]);
    assert_eq!(out.code, 0, "stderr:\n{}", out.stderr);
}

// ===========================================================================
// 4. --cpus flag for topology configuration.
// ===========================================================================

/// `--cpus N` configures the simulated topology width, observable in the trace:
/// the emitted Chrome-trace names exactly CPUs `0..N`. Checked for two distinct
/// widths so the flag is proven to actually take effect (not a fixed default).
#[test]
fn test_cpus_configures_topology() {
    for n in [2u32, 8] {
        let out_path = temp_path(&format!("cpus{n}"));
        let out = run_cli(&[
            "run",
            WORKLOAD,
            "-s",
            "simple",
            "--duration",
            "20ms",
            "--cpus",
            &n.to_string(),
            "--perfetto",
            &out_path.to_string_lossy(),
        ]);
        assert_eq!(out.code, 0, "cpus={n} run failed; stderr:\n{}", out.stderr);
        let json = std::fs::read_to_string(&out_path).expect("read perfetto trace");
        let _ = std::fs::remove_file(&out_path);
        let ids = distinct_cpu_ids(&json);
        let expected: BTreeSet<u32> = (0..n).collect();
        assert_eq!(
            ids, expected,
            "--cpus {n} should yield CPUs {expected:?} in the trace, got {ids:?}"
        );
    }
}

/// `--cpus 0` is out of the accepted range (minimum 1) — a clap usage error
/// (exit 2) with a clear range message.
#[test]
fn test_cpus_zero_rejected() {
    let out = run_cli(&["run", WORKLOAD, "--cpus", "0", "--duration", "10ms"]);
    assert_eq!(out.code, 2, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("--cpus") && out.stderr.contains("0"),
        "expected an out-of-range --cpus message, got stderr:\n{}",
        out.stderr
    );
}

// ===========================================================================
// 5. --perfetto flag for trace-file output path.
// ===========================================================================

/// `--perfetto <PATH>` writes the trace to exactly that path; the file is
/// created, non-empty, and begins with the Chrome-trace envelope.
#[test]
fn test_perfetto_output_written() {
    let out_path = temp_path("out");
    // Ensure a stale file from a previous run doesn't mask a failure.
    let _ = std::fs::remove_file(&out_path);

    let out = run_cli(&[
        "run",
        WORKLOAD,
        "-s",
        "simple",
        "--duration",
        "20ms",
        "--perfetto",
        &out_path.to_string_lossy(),
    ]);
    assert_eq!(out.code, 0, "stderr:\n{}", out.stderr);

    let content = std::fs::read_to_string(&out_path)
        .unwrap_or_else(|e| panic!("--perfetto did not create {}: {e}", out_path.display()));
    let _ = std::fs::remove_file(&out_path);

    assert!(!content.is_empty(), "perfetto trace file is empty");
    assert!(
        content.starts_with("{\"traceEvents\":"),
        "perfetto output is not Chrome-trace JSON; starts with: {:?}",
        &content[..content.len().min(40)]
    );
}

// ===========================================================================
// 6. Error messages for invalid arguments.
// ===========================================================================

/// An unknown scheduler name is a generic error (exit 1) with a message that
/// points at `--list-schedulers`.
#[test]
fn test_unknown_scheduler_error() {
    let out = run_cli(&[
        "run",
        WORKLOAD,
        "-s",
        "nope_not_a_sched",
        "--duration",
        "10ms",
    ]);
    assert_eq!(out.code, 1, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("unknown scheduler"),
        "expected 'unknown scheduler' message, got stderr:\n{}",
        out.stderr
    );
}

/// An unrecognized flag is a clap usage error (exit 2).
#[test]
fn test_unknown_flag_error() {
    let out = run_cli(&[
        "run",
        WORKLOAD,
        "--definitely-not-a-flag",
        "--duration",
        "10ms",
    ]);
    assert_eq!(out.code, 2, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.to_lowercase().contains("unexpected")
            || out.stderr.contains("--definitely-not-a-flag"),
        "expected an unknown-flag message, got stderr:\n{}",
        out.stderr
    );
}

/// A workload path that does not exist is a generic IO error (exit 1) naming the
/// read failure, not a panic or success.
#[test]
fn test_missing_workload_file_error() {
    let out = run_cli(&[
        "run",
        "/no/such/scxsim/workload_xyz.json",
        "--duration",
        "10ms",
    ]);
    assert_eq!(out.code, 1, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("failed to read"),
        "expected a 'failed to read' message, got stderr:\n{}",
        out.stderr
    );
}

/// Mutually-exclusive flags (`--no-rbc` disables RBC while `--rbc-ns` sets it)
/// are rejected by clap as a usage error (exit 2).
#[test]
fn test_conflicting_flags_rejected() {
    let out = run_cli(&[
        "run",
        WORKLOAD,
        "--no-rbc",
        "--rbc-ns",
        "5",
        "--duration",
        "10ms",
    ]);
    assert_eq!(out.code, 2, "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.to_lowercase().contains("cannot be used with")
            || out.stderr.contains("--rbc-ns")
            || out.stderr.contains("--no-rbc"),
        "expected a flag-conflict message, got stderr:\n{}",
        out.stderr
    );
}

/// A non-numeric, non-"entropy" `--seed` is rejected (non-zero exit). The exact
/// code is left unpinned: today it surfaces as a parse panic (101); a future
/// clean-error refactor would make it 1/2. Either way it must not succeed.
#[test]
fn test_invalid_seed_rejected() {
    let out = run_cli(&[
        "run",
        WORKLOAD,
        "-s",
        "simple",
        "--duration",
        "10ms",
        "--seed",
        "not_a_number",
    ]);
    assert_ne!(
        out.code, 0,
        "invalid --seed unexpectedly succeeded; stderr:\n{}",
        out.stderr
    );
}

// ===========================================================================
// Smoke: --help works at both levels.
// ===========================================================================

/// `--help` (top level) and `run --help` both print usage and exit 0.
#[test]
fn test_help_succeeds() {
    let top = run_cli(&["--help"]);
    assert_eq!(top.code, 0, "top --help failed; stderr:\n{}", top.stderr);
    assert!(
        top.stdout.contains("run") && top.stdout.contains("Usage"),
        "top --help missing usage/subcommands; stdout:\n{}",
        top.stdout
    );

    let sub = run_cli(&["run", "--help"]);
    assert_eq!(sub.code, 0, "run --help failed; stderr:\n{}", sub.stderr);
    assert!(
        sub.stdout.contains("--scheduler") && sub.stdout.contains("--cpus"),
        "run --help missing key flags; stdout:\n{}",
        sub.stdout
    );
}

// ===========================================================================
// --layer-config: loading a real scx_layered JSON config through the CLI.
//
// `tests/layered_config.rs` covers the parser and drives the scheduler
// in-process. What only a subprocess can show is the thing the CLI is for:
// a config file named on the command line reaching the scheduler, and the
// report telling you which rule fired. Those are the two tests below that
// assert on content rather than on an exit code.
// ===========================================================================

/// The shipped example layer config, and the workload it is written against.
const LAYER_CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/layer_configs/edge_like.json"
);
const CGROUP_WORKLOAD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/cgroup_hierarchy.json"
);

/// A config file on the command line reaches the scheduler, and the report
/// names WHICH rule put each task in its layer.
///
/// The assertion that carries the weight is the `background_1` line: it is in
/// `background/` like `background_0`, so the AND group's cgroup term held and
/// the comm term did not — `failed at term 1`. A run that compared nothing
/// cannot produce that, and comparing nothing is exactly what
/// `scxsim run -s layered` did before a config could be loaded at all.
#[test]
fn test_layer_config_rules_fire_and_the_report_names_them() {
    let out = run_cli(&[
        "run",
        "-s",
        "layered",
        "--cpus",
        "4",
        "--duration",
        "100ms",
        "--layer-config",
        LAYER_CONFIG,
        "--layer-report",
        CGROUP_WORKLOAD,
    ]);
    assert_eq!(out.code, 0, "run failed; stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("3 layer(s) from"),
        "the CLI should say what it loaded; stderr:\n{}",
        out.stderr
    );
    for expected in [
        // the AND group matching on both a cgroup path and a thread name
        r#"MATCHED  layer 1 "bg_worker_0" OR 0: CgroupPrefix("background/") AND CommPrefix("background_0")"#,
        // and the sibling that cleared term 0 and failed term 1
        r#"rejected layer 1 "bg_worker_0" OR 0: failed at term 1 = CommPrefix("background_0")"#,
        // a cgroup-only rule, on the path the scheduler itself renders
        r#"MATCHED  layer 0 "interactive" OR 0: CgroupPrefix("interactive/")"#,
    ] {
        assert!(
            out.stdout.contains(expected),
            "layer report missing {expected:?}; stdout:\n{}",
            out.stdout
        );
    }
}

/// Every run that loads a config says the CPU sets are the static split, not
/// the userspace allocator's, so `util_range` in the file cannot be read as
/// having sized anything.
#[test]
fn test_layer_config_run_declares_the_static_allocation() {
    let out = run_cli(&[
        "run",
        "-s",
        "layered",
        "--cpus",
        "4",
        "--duration",
        "50ms",
        "--layer-config",
        LAYER_CONFIG,
        CGROUP_WORKLOAD,
    ]);
    assert_eq!(out.code, 0, "run failed; stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("static weight-proportional split"),
        "stderr:\n{}",
        out.stderr
    );
}

/// A config asking for something scxsim cannot deliver fails by NAME, and the
/// message says which of them a waiver would cover.
#[test]
fn test_layer_config_refuses_by_name() {
    let dir = std::env::temp_dir().join(format!("scxsim-lcfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("gpu.json");
    std::fs::write(
        &path,
        r#"[
          { "name": "ai", "matches": [[{ "UsedGpuPid": true }]],
            "kind": { "Grouped": { "util_range": [0.5, 0.9], "membw_gb": 8.0 } } },
          { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
        ]"#,
    )
    .expect("write config");

    let out = run_cli(&[
        "run",
        "-s",
        "layered",
        "--cpus",
        "4",
        "--duration",
        "50ms",
        "--layer-config",
        path.to_str().unwrap(),
        CGROUP_WORKLOAD,
    ]);
    assert_ne!(out.code, 0, "a refused config must not run");
    assert!(
        out.stderr.contains("UsedGpuPid") && out.stderr.contains("membw_gb"),
        "both must be named; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("--layer-config-drop"),
        "the waivable one should say how; stderr:\n{}",
        out.stderr
    );

    // Waiving the field is not enough: the match kind is never waivable.
    let waived = run_cli(&[
        "run",
        "-s",
        "layered",
        "--cpus",
        "4",
        "--duration",
        "50ms",
        "--layer-config",
        path.to_str().unwrap(),
        "--layer-config-drop",
        "membw_gb",
        CGROUP_WORKLOAD,
    ]);
    assert_ne!(waived.code, 0, "the GPU match kind must still refuse");
    assert!(
        waived.stderr.contains("UsedGpuPid") && !waived.stderr.contains(": membw_gb —"),
        "stderr:\n{}",
        waived.stderr
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--layer-config` is an scx_layered configuration; pointing it at another
/// scheduler is a mistake worth naming rather than silently ignoring.
#[test]
fn test_layer_config_rejects_a_non_layered_scheduler() {
    let out = run_cli(&[
        "run",
        "-s",
        "simple",
        "--duration",
        "50ms",
        "--layer-config",
        LAYER_CONFIG,
        WORKLOAD,
    ]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("-s layered"),
        "should say what to do; stderr:\n{}",
        out.stderr
    );
}

/// `--layer-report` without a config would report on the single catch-all
/// layer, which evaluates no rule at all — an empty answer that reads like a
/// real one. Refuse instead.
#[test]
fn test_layer_report_requires_a_layer_config() {
    let out = run_cli(&[
        "run",
        "-s",
        "layered",
        "--duration",
        "50ms",
        "--layer-report",
        WORKLOAD,
    ]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("--layer-report needs --layer-config"),
        "stderr:\n{}",
        out.stderr
    );
}
