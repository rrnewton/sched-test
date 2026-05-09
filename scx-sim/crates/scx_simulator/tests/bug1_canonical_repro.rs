//! Bug-1 canonical reproducer — subprocess form.
//!
//! Distills David Dai's R1 reproducer recipe
//! (`experiments/lavd_cpubw_stalls_202604/davids-artifacts/lavd_bw_repro_r1_tight.sh`)
//! into a deterministic scxsim integration test that runs the **production
//! `scxsim` binary** against the canonical workload JSON + scheduler-config
//! TOML pair stored under `tests/fixtures/h6/`.
//!
//! # Why subprocess form (not Scenario builder)
//!
//! Earlier iterations of this test built the scenario in Rust via
//! `Scenario::builder` and poked LAVD globals via libloading FFI. That worked,
//! but it duplicated the scenario shape between Rust test code and the
//! reference `bug1_canonical.json` fixture, so the fixture was reference
//! documentation only — drift between the two could go undetected.
//!
//! This refactor consumes the same fixtures via `--config` + the workload
//! JSON, exercising:
//!   * the rt-app-rs JSON loader's `taskgroup` + `cpu.max` parser
//!   * the `--config` TOML loader and per-symbol BPF-global writer
//!   * the `scxsim` exit-code mapping (Bug-1 stall = exit 42)
//!   * the stable single-line stderr marker
//!     `scxsim: ExitKind::ErrorStall pid=<N> runnable_for_ns=<N>`
//!
//! # Determinism contract
//!
//! - Fixed scenario, fixed CPU count, fixed cgroup quota, fixed task count,
//!   fixed run length per task, fixed watchdog timeout.
//! - 100% reproduction across N reps.
//! - Each subprocess rep completes well under 5s wall.
//!
//! # Why a tight watchdog
//!
//! Production uses the kernel sched_ext default (30s). In simulation we
//! explicitly target a watchdog short enough to fire within a single
//! simulated throttle window (10ms of run + 90ms of wait per 100ms period).
//! 80ms is comfortably less than the 90ms throttle wait, so a task waiting
//! through the throttle window WILL trip an 80ms watchdog while leaving
//! headroom for short admission re-checks.
//!
//! # IMPORTANT: SHAPE-of-stall, NOT mechanism-of-stall
//!
//! This test reproduces the *shape* of Bug-1 (oversubscribed cgroup →
//! throttle → tasks pile up runnable → watchdog fires) but does **NOT**
//! reproduce production Bug-1's *mechanism*. The chosen invocation
//! parameters (`--watchdog 80ms` against a 100ms `cpu.max` period) are
//! a deliberate **test-design choice** — `watchdog < period` guarantees
//! the watchdog fires before the engine's first refill event at
//! `t = period_ns = 100ms` is reachable in the event queue. In other
//! words, the simulator's exit here is by **construction**, not by any
//! refill bug.
//!
//! Evidence: with realistic parameters (`--watchdog 5s --duration 10s`)
//! against the *same* workload, the simulator does **not** reproduce a
//! multi-second stall. Max `runnable_for_ns` peaks at ~200ms (≈2
//! periods) because refill fires at every period boundary and the
//! engine's `BandwidthManager` unthrottles correctly. See
//! `experiments/lavd_cpubw_stalls_202604/overnight_2026-05-08/STREAM_C_FOLLOWUP_REFILL_INVESTIGATION.md`
//! sections 5–6 for the full investigation, with file:line citations
//! for the engine code paths that were verified.
//!
//! This test is therefore best understood as a **shape regression
//! guard** (the engine still throttles + the watchdog still fires
//! deterministically with the same `runnable_for_ns` value across
//! reps). Treat any future "Bug-1 reproduces in scxsim" claim with
//! skepticism unless it uses a watchdog ≥ period and still produces a
//! multi-second stall.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Canonical invocation constants. The verbatim invocation matches the design
// doc's "Standalone-binary reproducer" section:
//
//   scxsim run tests/fixtures/h6/bug1_canonical.json \
//              --config tests/fixtures/h6/bug1_canonical.toml \
//              --watchdog 80ms -s lavd --cpus 4 --duration 500ms
// ---------------------------------------------------------------------------

const FIXTURE_JSON: &str = "tests/fixtures/h6/bug1_canonical.json";
const FIXTURE_TOML: &str = "tests/fixtures/h6/bug1_canonical.toml";
const WATCHDOG: &str = "80ms";
const SCHEDULER: &str = "lavd";
const CPUS: &str = "4";
const DURATION: &str = "500ms";

/// Stable stderr marker prefix produced by the binary's exit-code mapping.
/// The test asserts this prefix is present and the recorded
/// `runnable_for_ns=<N>` value is identical across repetitions.
const STALL_MARKER_PREFIX: &str = "scxsim: ExitKind::ErrorStall pid=";

/// Process exit code mapped from `ExitKind::ErrorStall` by `scxsim`'s
/// top-level main (see `bin/scxsim/main.rs::exit_code_for`).
const EXIT_STALL: i32 = 42;

/// Locate the rt-app workload JSON fixture relative to the integration-test
/// CWD (which cargo sets to the crate root, i.e. `crates/scx_simulator/`).
fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}

/// Run the canonical scxsim invocation once and capture
/// `(exit_code, stderr_string)`.
fn run_one_rep() -> (i32, String) {
    // `env!("CARGO_BIN_EXE_scxsim")` resolves to the cargo-managed path of
    // the freshly built `scxsim` binary, so the test always exercises the
    // current build (no PATH ambiguity, no separate copy).
    let exe = env!("CARGO_BIN_EXE_scxsim");

    // Pass `--no-disable-aslr` so scxsim does NOT re-exec itself for ASLR.
    // The re-exec works in production (and in the prior library-form test),
    // but inside a subprocess test it adds wall time and complicates exit
    // code propagation. The Bug-1 reproduction does not depend on ASLR
    // disabling.
    let output = Command::new(exe)
        .args([
            "--no-disable-aslr",
            "run",
            fixture(FIXTURE_JSON).to_str().unwrap(),
            "--config",
            fixture(FIXTURE_TOML).to_str().unwrap(),
            "--watchdog",
            WATCHDOG,
            "-s",
            SCHEDULER,
            "--cpus",
            CPUS,
            "--duration",
            DURATION,
        ])
        .output()
        .expect("failed to spawn scxsim subprocess");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("scxsim terminated by signal: {:?}", output.status));
    (code, stderr)
}

/// Extract the `runnable_for_ns=<N>` value from a stall stderr line. Returns
/// `None` if the marker is not present.
fn extract_runnable_for_ns(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        if !line.starts_with(STALL_MARKER_PREFIX) {
            continue;
        }
        // Line shape: `scxsim: ExitKind::ErrorStall pid=<N> runnable_for_ns=<N>`
        let key = "runnable_for_ns=";
        let idx = line.find(key)? + key.len();
        let tail = &line[idx..];
        let end = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        return tail[..end].parse::<u64>().ok();
    }
    None
}

// ---------------------------------------------------------------------------
// Test 1: single-shot reproduction. The canonical scenario fires the
// watchdog stall — the same observable shape Bug-1 produces in production
// sched_ext (a runnable task that fails to run for the watchdog interval).
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_subprocess_reproduces_stall() {
    let _lock = common::setup_test();

    let (code, stderr) = run_one_rep();

    assert_eq!(
        code, EXIT_STALL,
        "expected exit {EXIT_STALL} (ExitKind::ErrorStall); got {code}.\n\
         stderr was:\n{stderr}"
    );
    let runnable = extract_runnable_for_ns(&stderr).unwrap_or_else(|| {
        panic!(
            "expected stable stderr marker `{STALL_MARKER_PREFIX}…runnable_for_ns=<N>` in \
             scxsim stderr, but did not find it.\nstderr was:\n{stderr}"
        )
    });
    assert!(
        runnable >= 80_000_000,
        "watchdog fired with runnable_for_ns={runnable} but the configured \
         watchdog timeout is 80ms; the engine watchdog logic may be skewed. \
         Full stderr:\n{stderr}"
    );
    eprintln!(
        "[bug1_canonical_subprocess] reproduced ErrorStall with runnable_for_ns={runnable}ns ({}ms)",
        runnable / 1_000_000
    );
}

// ---------------------------------------------------------------------------
// Test 2: determinism loop — 10 reps, all must produce identical exit code
// AND identical `runnable_for_ns` value. Bounded total wall time.
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_subprocess_deterministic_10_reps() {
    let _lock = common::setup_test();

    const N: usize = 10;
    let start = Instant::now();
    let mut first: Option<u64> = None;

    for rep in 0..N {
        let (code, stderr) = run_one_rep();
        assert_eq!(
            code, EXIT_STALL,
            "rep {rep}: expected exit {EXIT_STALL}, got {code}.\nstderr:\n{stderr}"
        );
        let runnable = extract_runnable_for_ns(&stderr)
            .unwrap_or_else(|| panic!("rep {rep}: stall marker missing.\nstderr:\n{stderr}"));
        if let Some(prev) = first {
            assert_eq!(
                runnable, prev,
                "rep {rep}: nondeterministic stall: expected runnable_for_ns={prev} \
                 (from rep 0), got {runnable}.\nstderr:\n{stderr}"
            );
        } else {
            first = Some(runnable);
        }
    }

    let total = start.elapsed();
    let per_rep = total / N as u32;
    eprintln!(
        "[bug1_canonical_subprocess] {N} reps deterministically produced \
         ExitKind::ErrorStall runnable_for_ns={} (total wall {:.2?}, per_rep {:.2?})",
        first.unwrap(),
        total,
        per_rep
    );

    // Hard wall budget: subprocess overhead is generous, but 60s total covers
    // a comfortable per-rep ceiling of 6s on slow CI hardware. A single rep
    // takes ~150ms on a fast workstation.
    assert!(
        total < std::time::Duration::from_secs(60),
        "10 subprocess reps took {total:?}, exceeding the 60s wall budget"
    );
}
