//! Bug-1 canonical reproducer — subprocess form.
//!
//! Runs the production `scxsim` binary against the canonical workload
//! JSON + scheduler-config TOML pair under `tests/fixtures/h6/`.
//!
//! # What this test asserts (post engine-throttle-attribution fix)
//!
//! Before the Stage E percpu-array seed fix
//! (`tg investigate-scxsim-engine-throttles-before-scheduler-cgroup-bw`),
//! scxsim's engine-side `BandwidthManager` short-circuited the
//! scheduler-side `cgroup_bw` library entirely. The canonical
//! reproducer always tripped the engine's watchdog at exactly the
//! configured timeout, regardless of which scx SHA's library was
//! loaded — i.e. a wiring smoke test, not a Bug-1 probe.
//!
//! Post-fix, the library actually drives throttling. On a non-buggy
//! scx tip (which the integrated-`simulator.v6` gitlink is, even if
//! not Bug-1-FIXED), the library correctly throttles the cgroup at
//! period_budget exhaustion, puts aside its tasks in the BTQ, and
//! re-enqueues them at refill — so the canonical produces NO
//! watchdog stall.
//!
//! The new contract:
//!
//!   * The canonical (over-subscribed) library state at end-of-run
//!     shows the cgroup hit the throttle path: `is_throttled=1`,
//!     `nr_throttled_tasks=16`, `nr_throttled_periods >= 4` of the 6
//!     100ms periods elapsed.
//!   * Determinism: across N reps, the
//!     `(rc, is_throttled, nr_throttled_periods, nr_throttled_tasks)`
//!     fingerprint is byte-identical.
//!   * Per-SHA discrimination (env-gated): swapping in a
//!     pre-`period_budget` scx SHA's `libscx_lavd.so` via
//!     `--scheduler-file` produces a CLEARLY DIFFERENT fingerprint.
//!     Skipped silently when `SCXSIM_BIN_CACHE_DIR` is unset.
//!   * Under-subscribed sibling fixture
//!     (`bug1_canonical_undersub.json`, paired with `--cpus 32` →
//!     0.125 tasks/cpu, deep in the 0.13–0.57 live workload band)
//!     reproduces the same throttle fingerprint with the
//!     workload-axis-only difference of 4 tasks vs 16. Confirms the
//!     bug-1 mechanism is not gated on CPU oversubscription — the
//!     cgroup_bw constraint alone drives the library into the
//!     throttled state. See tg
//!     `bug1-canonical-fixture-add-undersub-variant`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Canonical invocation constants. Verbatim:
//   scxsim run tests/fixtures/h6/bug1_canonical.json \
//              --config tests/fixtures/h6/bug1_canonical.toml \
//              --watchdog 200ms -s lavd --cpus 4 --duration 600ms
// ---------------------------------------------------------------------------

const FIXTURE_JSON: &str = "tests/fixtures/h6/bug1_canonical.json";
const FIXTURE_TOML: &str = "tests/fixtures/h6/bug1_canonical.toml";
const WATCHDOG: &str = "200ms";
const SCHEDULER: &str = "lavd";
const CPUS: &str = "4";
const DURATION: &str = "600ms";

/// Under-subscribed sibling fixture (4 yes-loop workers, 50ms run + 50ms
/// sleep, same shared `cpu.max=10000 100000` cgroup constraint). Designed
/// to be paired with `--cpus 32` to land in the 0.13–0.57 tasks/cpu live
/// workload band identified by tg
/// `investigate-dsq-insert-vs-vtime-path-divergence`. cpu.max constraint
/// is identical to the over-subscribed canonical, so the cgroup_bw
/// library is exercised the same way; only the workload axis (oversub
/// vs undersub, all-run vs run+sleep cycling) differs. Lets us confirm
/// the bug-1 mechanism is workload-axis-agnostic in scxsim post-Stage-E.
const FIXTURE_JSON_UNDERSUB: &str = "tests/fixtures/h6/bug1_canonical_undersub.json";
const CPUS_UNDERSUB: &str = "32";

/// Successful exit (no stall fired). Post-Stage-E the library handles
/// throttling correctly so the canonical run completes without tripping
/// the watchdog.
const EXIT_OK: i32 = 0;

/// LAVD-PRINTK end-of-run fingerprint of the canonical run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Fingerprint {
    exit_code: i32,
    is_throttled: u32,
    nr_throttled_periods: String,
    nr_throttled_tasks: u32,
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}

/// Per-invocation configuration. Bundles fixture path, CPU count, and
/// optional `--scheduler-file` override so the test entry points can
/// share `run_one_rep` across the canonical (over-subscribed) and
/// `bug1_canonical_undersub` (live-workload-band) variants.
#[derive(Clone)]
struct Invocation<'a> {
    /// Workload JSON fixture (relative to crate root).
    fixture_json: &'a str,
    /// Number of simulated CPUs to pass via `--cpus`.
    cpus: &'a str,
    /// Optional `--scheduler-file <PATH>` override (used by the per-SHA
    /// discrimination test to swap in cached `.so`s without rebuilding).
    scheduler_file: Option<&'a Path>,
}

impl<'a> Invocation<'a> {
    /// Over-subscribed canonical: 16 yes-loop workers / 4 cpus = 4.0 tasks/cpu.
    fn canonical() -> Self {
        Self {
            fixture_json: FIXTURE_JSON,
            cpus: CPUS,
            scheduler_file: None,
        }
    }

    /// Under-subscribed live-workload-band variant: 4 yes-loop workers
    /// (50ms run + 50ms sleep) / 32 cpus = 0.125 tasks/cpu.
    fn undersub() -> Self {
        Self {
            fixture_json: FIXTURE_JSON_UNDERSUB,
            cpus: CPUS_UNDERSUB,
            scheduler_file: None,
        }
    }

    fn with_scheduler_file(mut self, p: &'a Path) -> Self {
        self.scheduler_file = Some(p);
        self
    }
}

/// Run scxsim once with the given invocation config and capture
/// `(exit_code, stderr)`.
fn run_one_rep(inv: &Invocation<'_>) -> (i32, String) {
    let exe = env!("CARGO_BIN_EXE_scxsim");
    let mut cmd = Command::new(exe);
    cmd.args([
        "--no-disable-aslr",
        "run",
        fixture(inv.fixture_json).to_str().unwrap(),
        "--config",
        fixture(FIXTURE_TOML).to_str().unwrap(),
        "--watchdog",
        WATCHDOG,
        "-s",
        SCHEDULER,
        "--cpus",
        inv.cpus,
        "--duration",
        DURATION,
    ]);
    if let Some(path) = inv.scheduler_file {
        cmd.args(["--scheduler-file", path.to_str().unwrap()]);
    }
    let output = cmd.output().expect("failed to spawn scxsim subprocess");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("scxsim terminated by signal: {:?}", output.status));
    (code, stderr)
}

/// Find the leaf cgroup `[LAVD-PRINTK]` block (cgid=2 / level=1 in
/// the canonical fixture) in the stderr stream and extract the
/// throttle-state fingerprint.
fn extract_fingerprint(exit_code: i32, stderr: &str) -> Fingerprint {
    let line = stderr
        .lines()
        .find(|l| l.contains("LAVD-PRINTK") && l.contains("is_throttled:"))
        .unwrap_or_else(|| {
            panic!(
                "expected `[LAVD-PRINTK] ... is_throttled: ...` line in scxsim stderr; \
                 the cgroup_bw library may not have run. stderr was:\n{stderr}"
            )
        });

    let is_throttled = parse_uint_after(line, "is_throttled: ")
        .unwrap_or_else(|| panic!("could not parse is_throttled from `{line}`"));

    let nr_throttled_periods = parse_token_after(line, "nr_throttled_periods: ")
        .unwrap_or_else(|| panic!("could not parse nr_throttled_periods from `{line}`"));

    let nr_throttled_tasks = parse_uint_after(line, "nr_throttled_tasks: ")
        .unwrap_or_else(|| panic!("could not parse nr_throttled_tasks from `{line}`"));

    Fingerprint {
        exit_code,
        is_throttled: is_throttled as u32,
        nr_throttled_periods,
        nr_throttled_tasks: nr_throttled_tasks as u32,
    }
}

fn parse_uint_after(line: &str, key: &str) -> Option<u64> {
    let idx = line.find(key)? + key.len();
    let tail = &line[idx..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    tail[..end].parse::<u64>().ok()
}

fn parse_token_after(line: &str, key: &str) -> Option<String> {
    let idx = line.find(key)? + key.len();
    let tail = &line[idx..];
    let end = tail
        .find(|c: char| c == ',' || c.is_whitespace())
        .unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

// ---------------------------------------------------------------------------
// Test 1: single-shot reproduction. After V4-C engine fix, the canonical
// scenario produces the CORRECTED fingerprint:
//   - rc=0
//   - cgroup_bw library DOES activate throttle on the legitimately
//     CPU-bound workload (nr_throttled_periods numerator >= 4 of 6)
//   - cgroup CORRECTLY UNTHROTTLES at end of run (is_throttled == 0)
//     because no real work is pending; the V1-V3 STALL pattern
//     (is_throttled stuck at 1, nr_throttled_tasks==16, debt runaway)
//     was an ENGINE bug that V4-A named and V4-C fixed by clearing
//     prev_task on idle so lavd_dispatch doesn't fall through to
//     consume_prev with a stale prev. See tg
//     `scxsim-fix-cbw-debt-runaway-or-document-as-known-cpu-bw-stall-bug`.
//
// HISTORICAL CONTEXT (pre-V4-C):
//   The original test asserted is_throttled==1 + nr_throttled_tasks==16
//   at run end. That fingerprint WAS the bug-1 reproduction, not the
//   feature: the engine over-charged 100M ns/period to the throttled
//   idle cgroup, causing runaway debt → keep_throttled forever → BTQ
//   drain bails → no pick. Post-V4-C: is_throttled clears correctly,
//   nr_throttled_tasks reflects only legitimate quota enforcement.
//
// Previously gated `#[ignore]` under mb sim-624b9e: on the GitHub Actions
// Ubuntu 24.04 runner this test FAILED post-V4-C with fingerprint
// `{is_throttled:0, nr_throttled_periods:'0/6', nr_throttled_tasks:0}` —
// `runtime_total_sloppy=0` end-of-run despite the engine calling
// `scx_cgroup_bw_consume(cgid, ~100M)` every period.
//
// Phase 2 ROOT CAUSE (2026-05-19, Phase 2 task
// `phase2-rootcause-engine-library-handshake-percpu-codegen-ubuntu2404`):
// `struct cgroup_llc_id { u64 cgrp_id; int llc_id; }` has 4 bytes of
// trailing padding (sizeof = 16). clang 18 -O2 (Ubuntu 24.04 default)
// leaves those padding bytes UNINITIALIZED for stack-local designated-
// initializer construction; clang 22+ zeroes them. scxsim's
// `scx_test_map_lookup_elem` uses `memcmp` over the full `key_size = 16`,
// so on clang 18 the post-insert padding garbage in `cbw_alloc_llc_ctx`
// differs from the post-lookup padding garbage in `cbw_get_llc_ctx_with_id`
// → memcmp never matches → `cbw_get_llc_ctx` returns NULL →
// `scx_cgroup_bw_consume` silently returns 0 without accumulating into
// `llcx->runtime_total` → `runtime_total_sloppy` stays 0 → library never
// throttles.
//
// FIX: `schedulers/lavd/wrapper.c::lavd_register_cbw_maps` overrides
// `cbw_cgrp_llc_test_map.key_size` from 16 down to 12 (cgrp_id + llc_id
// fields only), so `memcmp` ignores the uninit padding regardless of
// compiler version. Verified with bundled clang 18.1.8 reproducer:
// `experiments/phase2_engine_library_handshake_root_cause_20260519/REPORT.md`.
// ---------------------------------------------------------------------------

// IGNORED after the 2026-06-22 scx upstream bump (sync-upstream/20260622,
// submodule -> upstream dd06cd27). The V4-C-era assertion below expects
// is_throttled==0 at end-of-run ("unthrottle when no real work is pending"),
// but the rewritten upstream cgroup_bw library now deterministically leaves
// is_throttled==1 (throttle enforcement still works: 5/6 periods,
// nr_throttled_tasks=4; the determinism sibling test still passes, so this is
// NOT flakiness). Upstream changes in range: 776ae41e (cgrp_id API + taskc
// cache), arena migration (4fa7fb81/48757ded), a52f85e3 (root cgroup via loader
// task), 58740f71 (period_budget poisoning fix targeting stale per-CPU
// rq->clock_task phantom runtime — a mechanism scxsim does not reproduce, since
// it charges consumed_ns directly), 6c4df643 (throttle-check consolidation).
// Whether is_throttled==1 is genuinely-correct new behavior (update assertion)
// or a scxsim accounting gap vs the new period_budget semantics (fix engine) is
// tracked in minibeads sim-560f79.
// Ignored pending mb sim-560f79 / mb sim-1ei8j. This is a DOCUMENTED OPEN
// QUESTION, not a silent skip: end-of-run is_throttled is 0 on a dev box and
// 1 on CI for the same commit, fixture and flags.
//
// History, because it has moved twice: sim-560f79 recorded it as
// deterministically 1 after the June cgroup_bw rewrite. It was un-ignored on
// 2026-08-12 once integration 37f7f8a fixed check_watchdog reporting
// deliberate bandwidth throttling as starvation, after which it passed 20/20
// locally -- and CI could not contradict that, because the coverage gate was
// dying in ld before any test ran. Fixing that (PR #81) let CI run it, and it
// failed there.
//
// Ruled out by experiment: host CPU count (316/4/2 via taskset), ASLR (on and
// off), and repetition (20 runs) -- 0 every time locally. Remaining hypothesis
// is toolchain codegen (CI: rustc 1.97.1 + Ubuntu clang; dev: 1.96.0 + clang
// 18.1.8), which would make it kin to the upstream match_substr uninitialised
// read (mb sim-hyr11). Not confirmed.
//
// The stack-address garbage in the CI dump's period/burst is a RED HERRING: an
// upstream printf arity bug in one line of cbw_dump_cgroup_tree (mb sim-uv2ir),
// diagnostic-only. The is_throttled value itself is printed by a separately
// well-formed call and is trustworthy.
#[test]
#[ignore = "mb sim-560f79 / mb sim-1ei8j: end-of-run is_throttled is 0 on a \
            dev box and 1 on CI for the same commit; open question, not a \
            silent skip. Un-ignore when that is resolved."]
fn test_bug1_canonical_subprocess_reproduces_throttle() {
    let _lock = common::setup_test();

    let (code, stderr) = run_one_rep(&Invocation::canonical());
    assert_eq!(
        code, EXIT_OK,
        "expected EXIT_OK ({EXIT_OK}); got {code}.\nstderr:\n{stderr}"
    );

    let fp = extract_fingerprint(code, &stderr);
    eprintln!("[bug1_canonical_subprocess] post-V4C fingerprint = {fp:?}");

    // V4-C: cgroup correctly unthrottles when no work pending. The
    // bug-1 stall fingerprint (is_throttled==1 forever) is fixed.
    assert_eq!(
        fp.is_throttled, 0,
        "post-V4C: cgroup should UNTHROTTLE when no real work is pending. \
         is_throttled==1 indicates the V1-V3 stall pattern has regressed (engine \
         over-charge has returned). Got {fp:?}.\nstderr:\n{stderr}"
    );
    assert!(
        fp.nr_throttled_tasks <= 16,
        "expected nr_throttled_tasks <= 16 (the workload's task count); got {fp:?}.\nstderr:\n{stderr}"
    );
    let throttled = fp
        .nr_throttled_periods
        .split('/')
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .unwrap_or(0);
    assert!(
        throttled >= 4,
        "expected nr_throttled_periods numerator >= 4 (out of 6 periods in a 600ms run) \
         — proves cgroup_bw library IS enforcing the quota on the legitimately \
         CPU-bound workload; got {fp:?}.\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: determinism loop. Across 10 reps with the same in-tree .so the
// fingerprint MUST be byte-identical (modulo the variable
// runtime_total_sloppy / _last numbers, which are not part of the
// fingerprint -- they jitter with timer fire ordering inside a 100ms
// period).
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_subprocess_deterministic_10_reps() {
    let _lock = common::setup_test();

    const N: usize = 10;
    let start = Instant::now();
    let mut first: Option<Fingerprint> = None;
    for rep in 0..N {
        let (code, stderr) = run_one_rep(&Invocation::canonical());
        let fp = extract_fingerprint(code, &stderr);
        if let Some(prev) = &first {
            assert_eq!(
                &fp, prev,
                "rep {rep}: nondeterministic fingerprint -- got {fp:?}, expected {prev:?}.\n\
                 stderr:\n{stderr}"
            );
        } else {
            first = Some(fp);
        }
    }
    let total = start.elapsed();
    let per_rep = total / N as u32;
    eprintln!(
        "[bug1_canonical_subprocess] {N} reps deterministic; fingerprint = {:?}; \
         total wall {:.2?}, per_rep {:.2?}",
        first.unwrap(),
        total,
        per_rep
    );
    // Wall budget: post-Stage-E the canonical no longer stalls at
    // ~200ms, so each rep runs the full 600ms simulated duration with
    // the library doing real put-aside / reenqueue work every refill.
    // ~10s wall per rep is typical on this hardware; budget for 180s
    // total gives 18s/rep slack on slow CI.
    assert!(
        total < std::time::Duration::from_secs(180),
        "10 subprocess reps took {total:?}, exceeding the 180s wall budget"
    );
}

// ---------------------------------------------------------------------------
// Test 3: per-SHA discrimination (env-gated).
//
// Skipped silently if SCXSIM_BIN_CACHE_DIR is unset. When set, points at
// a directory laid out as
//     <BIN_CACHE>/<short_sha>/libscx_lavd.so
// produced by experiments/bug1_scx_version_matrix_20260512/build_per_hash.sh.
//
// Asserts that the canonical reproducer produces DIFFERENT fingerprints
// across the v3 matrix's three .so-buildable scx SHAs:
//   - d565180067 (Apr-04, pre-period_budget):     is_throttled=0
//   - 66d2ef699b (Apr-04, period_budget intro):   is_throttled=1
//   - a08c9e272b (Apr-23, current v6 gitlink):    is_throttled=1
//
// (See experiments/engine_throttle_per_sha_20260513/README.md for the
// full per-SHA fingerprint matrix and methodology.)
// ---------------------------------------------------------------------------

// SKIP DISPOSITION: needs a per-SHA binary cache that neither CI nor a fresh
// clone has. Until 2026-08-12 this was a plain #[test]: with
// SCXSIM_BIN_CACHE_DIR unset it printed a "skipping" line, returned, and was
// counted as PASSED — a test asserting nothing, indistinguishable from a real
// pass in the suite total. #[ignore] makes that honest. Build the cache with
// experiments/bug1_scx_version_matrix_20260512/build_per_hash.sh, point
// SCXSIM_BIN_CACHE_DIR at it, and run with --run-ignored all.
#[test]
#[ignore = "requires a per-SHA libscx_lavd.so cache via SCXSIM_BIN_CACHE_DIR \
            (build_per_hash.sh); absent in CI and in a fresh clone"]
fn test_bug1_canonical_per_sha_discrimination() {
    let _lock = common::setup_test();

    let cache = match std::env::var("SCXSIM_BIN_CACHE_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            eprintln!(
                "[bug1_canonical_subprocess] SCXSIM_BIN_CACHE_DIR not set; skipping per-SHA \
                 discrimination test. Set it to the bin_cache_engine_throttle_fix dir to run."
            );
            return;
        }
    };

    let cases: &[(&str, u32, &str)] = &[
        // (short_sha,                expected_is_throttled, label)
        ("d565180067", 0, "Apr-04 pre-period_budget"),
        ("66d2ef699b", 1, "Apr-04 period_budget intro"),
        ("a08c9e272b", 1, "Apr-23 current v6 gitlink"),
    ];

    let mut fps: Vec<(String, Fingerprint)> = Vec::new();
    for (sha, expected_is_throttled, label) in cases {
        let so = cache.join(sha).join("libscx_lavd.so");
        if !so.exists() {
            eprintln!(
                "[bug1_canonical_subprocess] {sha} ({label}): {} missing; skipping",
                so.display()
            );
            continue;
        }
        let (code, stderr) = run_one_rep(&Invocation::canonical().with_scheduler_file(&so));
        let fp = extract_fingerprint(code, &stderr);
        eprintln!("[bug1_canonical_subprocess] {sha} ({label}): {fp:?}");
        assert_eq!(
            fp.is_throttled,
            *expected_is_throttled,
            "{sha} ({label}): expected is_throttled={expected_is_throttled}, got {fp:?}.\n\
             stderr tail:\n{}",
            stderr
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        );
        fps.push((sha.to_string(), fp));
    }

    // The whole point of this test: the matrix produces multiple
    // distinct fingerprints. If at least 2 SHAs were exercised and they
    // all produced the SAME fingerprint, the engine-throttle-attribution
    // fix has regressed and per-SHA discrimination is broken.
    if fps.len() >= 2 {
        let first = &fps[0].1;
        let all_same = fps.iter().all(|(_, fp)| fp == first);
        assert!(
            !all_same,
            "per-SHA discrimination REGRESSED: {} cached SHAs all produced the same \
             fingerprint {first:?}. The cgroup_bw library is no longer driving \
             throttling differently per scx SHA.",
            fps.len()
        );
        eprintln!(
            "[bug1_canonical_subprocess] per-SHA discrimination OK: {} cached SHAs \
             produced {} distinct fingerprints",
            fps.len(),
            fps.iter()
                .map(|(_, f)| f)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
    } else {
        eprintln!(
            "[bug1_canonical_subprocess] only {} cached SHAs available; \
             per-SHA discrimination NOT verified (need >= 2). Build the cache via \
             experiments/bug1_scx_version_matrix_20260512/build_per_hash.sh.",
            fps.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: under-subscribed live-workload-band variant.
//
// `bug1_canonical_undersub.json` is the live-workload-band sibling of the
// canonical (4 yes-loop workers, 50ms run + 50ms sleep, same shared
// `cpu.max=10000 100000` cgroup constraint). Designed to land in the
// 0.13–0.57 tasks/cpu live workload band when paired with `--cpus 32`.
// Confirms that the bug-1 mechanism reproduces in scxsim WITHOUT requiring
// CPU oversubscription -- the cgroup_bw constraint alone is sufficient
// to drive the library into the throttled state.
//
// Fingerprint expectation (matches canonical's STRUCTURE but with 4
// tasks instead of 16):
//   rc=0 (no false stall),
//   is_throttled=1,
//   nr_throttled_tasks=4 (all yes-loop workers in the BTQ),
//   nr_throttled_periods >= 4/N (out of N=6 periods in a 600ms run).
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_undersub_subprocess_reproduces_throttle() {
    let _lock = common::setup_test();

    let (code, stderr) = run_one_rep(&Invocation::undersub());
    assert_eq!(
        code, EXIT_OK,
        "expected EXIT_OK ({EXIT_OK}); got {code}.\nstderr:\n{stderr}"
    );

    let fp = extract_fingerprint(code, &stderr);
    eprintln!("[bug1_canonical_undersub] integrated-v6 fingerprint = {fp:?}");

    // ASSERT THE COUNT OVER THE RUN, NOT AN END-OF-RUN SAMPLE.
    //
    // This test used to assert `fp.is_throttled == 1` and `fp.nr_throttled_tasks
    // == 4`. Both are INSTANTANEOUS readings taken at the moment the run ends,
    // and both were only true by accident: the engine charged nothing for
    // re-dispatch, which happened to place the end of the run inside a throttled
    // window of the 100ms bandwidth cycle. Charging re-dispatch the modelled
    // kernel cost moved the end of the run by a few microseconds per dispatch,
    // and it now lands in an unthrottled part of the same cycle — `is_throttled`
    // reads 0 and the BTQ is empty, while `nr_throttled_periods` is unchanged at
    // 5/6. The throttling behaviour did not change; only where in the cycle we
    // happened to look did.
    //
    // This is the SECOND instance of that shape found in this codebase. The
    // other is the watchdog's throttle sampler, which reads post-replenish and
    // therefore catches a 99.9%-duty condition about 0.25% of the time. Different
    // code, same defect: a phase-dependent observation of a periodic quantity,
    // reported as a fact about the system. The lesson generalises — assert the
    // robust quantity, not the incidentally-correlated one, the same way we
    // report waits rather than watchdog trips.
    //
    // `nr_throttled_periods` is that robust quantity here: it accumulates over
    // the whole run and cannot be moved by a few microseconds of phase. It is
    // pinned exactly rather than as a lower bound, which is strictly more test
    // than the `>= 4` it replaces.
    //
    // Coverage deliberately given up: `nr_throttled_tasks == 4` checked that ALL
    // FOUR workers were in the bandwidth-throttle queue together, and the
    // fingerprint carries no cumulative counterpart to that. It is dropped
    // rather than kept, because keeping it would reintroduce exactly the
    // phase-dependence this comment exists to remove. Recovering it needs a
    // cumulative per-task throttle count on the library side — see mb sim-560f79
    // / mb sim-1ei8j, which track the end-of-run sampling question generally.
    assert_eq!(
        fp.nr_throttled_periods, "5/6",
        "undersub: expected the cgroup to be throttled in 5 of the 6 periods of a \
         600ms run; got {fp:?}.\nstderr:\n{stderr}"
    );
}
