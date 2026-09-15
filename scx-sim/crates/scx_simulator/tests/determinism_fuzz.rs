//! Fuzz-style simulator determinism tests (tg test-sim-determinism-fuzzing).
//!
//! This is the *thorough* companion to `determinism.rs`. Where that file pins
//! the determinism guarantee on a single hand-written scenario, this file fuzzes
//! it: for each scheduler (`simple`, `lavd`, `cosmos`) it generates 100 distinct
//! seeds, and for every seed it (a) derives a pseudo-random scenario that is a
//! pure function of the seed — varying CPU count, task count, per-task behavior,
//! nice values, duration, plus a waker/wakee pair to exercise the wakeup path —
//! and (b) runs that scenario twice, asserting the two traces are byte-identical.
//!
//! The core reproducibility guarantee under test is *seed-based logical
//! determinism*: for a fixed scenario + seed the event-driven engine produces a
//! byte-identical trace regardless of wall-clock, run order, or host. Timing
//! noise is left ENABLED (the production default) so the seeded noise PRNG
//! (tick/run jitter) is part of what must reproduce.
//!
//! ## Reproducibility of the fuzzer itself
//! The 100 seeds are produced by a fixed-seeded SplitMix64 stream
//! (`gen_seeds`), and every scenario is a deterministic function of its seed
//! (`scenario_for_seed`), so the whole suite is itself fully reproducible — a
//! failure names the exact seed, which can be replayed in isolation.
//!
//! ## Failure reporting
//! Non-determinism does not abort on the first bad seed: each scheduler test
//! checks all 100 seeds, collects every seed whose two runs diverged (with the
//! first differing event), and fails once at the end listing them all — so one
//! run surfaces the complete set of non-deterministic seeds (requirement 4).
//!
//! Byte-identical is compared structurally on `(time_ns, cpu, kind)` per event
//! plus event count and `exit_kind`; `TraceKind` derives `Eq` and carries only
//! logical IDs (never raw addresses), so this is a faithful trace-bytes proxy
//! (same rationale as `determinism.rs::trace_signature`).

use scx_simulator::*;

#[macro_use]
mod common;

/// Seeds fuzzed per scheduler.
const NUM_SEEDS: usize = 100;

/// One SplitMix64 step (deterministic, well-distributed 64-bit mixing).
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic, reproducible set of `NUM_SEEDS` distinct u32 seeds.
fn gen_seeds() -> Vec<u32> {
    let mut state: u64 = 0xD1B5_4A32_D192_ED03; // fixed base -> reproducible
    let mut seeds = Vec::with_capacity(NUM_SEEDS);
    while seeds.len() < NUM_SEEDS {
        let s = splitmix64(&mut state) as u32;
        // Keep them distinct so "100 seeds" really means 100 different PRNG
        // streams (collisions are astronomically unlikely but cheap to exclude).
        if !seeds.contains(&s) {
            seeds.push(s);
        }
    }
    seeds
}

/// Independent per-key hash derived from a seed, for deriving scenario shape.
fn seed_hash(seed: u32, key: u64) -> u64 {
    let mut s = (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(key.wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    splitmix64(&mut s)
}

/// Build a pseudo-random scenario that is a *pure function of `seed`*: two calls
/// with the same seed produce identical scenarios. Returns the CPU count too so
/// the scheduler can be constructed to match. Noise is left at its default (on).
fn scenario_for_seed(seed: u32) -> (u32, Scenario) {
    let nr_cpus = [1u32, 2, 4][(seed_hash(seed, 1) % 3) as usize];
    let ntasks = 4 + (seed_hash(seed, 2) % 5) as u32; // 4..=8
    let dur_ms = 25 + seed_hash(seed, 3) % 20; // 25..=44

    let mut b = Scenario::builder().cpus(nr_cpus).seed(seed);

    // A waker (pid 1) / wakee (pid 2) pair to exercise the cross-task wakeup
    // path under the fuzzed timing. pid 2 always exists (ntasks >= 4).
    b = b.add_task(
        "waker",
        0,
        TaskBehavior {
            phases: vec![
                Phase::Run(2_000_000),
                Phase::Wake(Pid(2)),
                Phase::Sleep(3_000_000),
            ],
            repeat: RepeatMode::Forever,
        },
    );
    b = b.add_task(
        "wakee",
        0,
        TaskBehavior {
            phases: vec![Phase::Run(1_000_000), Phase::Sleep(8_000_000)],
            repeat: RepeatMode::Forever,
        },
    );

    // The remaining tasks: a per-seed mix of CPU hogs and sleep/run cyclers.
    for i in 2..ntasks {
        let h = seed_hash(seed, 100 + i as u64);
        let nice = ((h % 7) as i8) - 3; // -3..=3
        let behavior = match h % 3 {
            0 => TaskBehavior {
                // CPU hog.
                phases: vec![Phase::Run(1_000_000_000)],
                repeat: RepeatMode::Forever,
            },
            1 => TaskBehavior {
                // Sleep/run cycler with seed-derived timings.
                phases: vec![
                    Phase::Run(1_000_000 + (h >> 8) % 3_000_000),
                    Phase::Sleep(1_000_000 + (h >> 16) % 3_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
            _ => TaskBehavior {
                // Short bursty task (frequent context switches).
                phases: vec![Phase::Run(500_000), Phase::Sleep(1_500_000)],
                repeat: RepeatMode::Forever,
            },
        };
        b = b.add_task(&format!("t{i}"), nice, behavior);
    }

    (nr_cpus, b.duration_ms(dur_ms).build())
}

/// Structural diff of two traces: `None` if byte-identical, else a short
/// description of the first divergence (event count, first differing event, or
/// exit kind).
fn trace_divergence(a: &Trace, b: &Trace) -> Option<String> {
    let (ea, eb) = (a.events(), b.events());
    if ea.len() != eb.len() {
        return Some(format!("event count {} vs {}", ea.len(), eb.len()));
    }
    for (i, (x, y)) in ea.iter().zip(eb.iter()).enumerate() {
        if x.time_ns != y.time_ns || x.cpu != y.cpu || x.kind != y.kind {
            return Some(format!(
                "event {i}: (t={}, cpu={}, {:?}) vs (t={}, cpu={}, {:?})",
                x.time_ns, x.cpu.0, x.kind, y.time_ns, y.cpu.0, y.kind
            ));
        }
    }
    if a.exit_kind() != b.exit_kind() {
        return Some(format!(
            "exit_kind {:?} vs {:?}",
            a.exit_kind(),
            b.exit_kind()
        ));
    }
    None
}

/// Run the 100-seed determinism fuzz for one scheduler, reporting every
/// non-deterministic seed at once.
fn fuzz_scheduler(name: &str, make: fn(u32) -> DynamicScheduler) {
    let seeds = gen_seeds();
    assert_eq!(
        seeds.len(),
        NUM_SEEDS,
        "seed generator produced too few seeds"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut total_events: u64 = 0;

    for &seed in &seeds {
        let (nr, s1) = scenario_for_seed(seed);
        let (_, s2) = scenario_for_seed(seed);

        let t1 = Simulator::new(make(nr)).run(s1);
        let t2 = Simulator::new(make(nr)).run(s2);

        // A non-empty trace confirms the fuzzed scenario actually exercised the
        // engine (an empty pair would be "deterministic" but vacuous).
        if t1.events().is_empty() {
            failures.push(format!(
                "seed {seed:#010x} (nr_cpus={nr}): produced an empty trace"
            ));
            continue;
        }
        total_events += t1.events().len() as u64;

        if let Some(why) = trace_divergence(&t1, &t2) {
            failures.push(format!("seed {seed:#010x} (nr_cpus={nr}): {why}"));
        }
    }

    assert!(
        failures.is_empty(),
        "{name}: {} of {NUM_SEEDS} seeds were NON-DETERMINISTIC:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );

    eprintln!("{name}: {NUM_SEEDS} seeds x2 runs all byte-identical ({total_events} total events)");
}

// ---------------------------------------------------------------------------
// One #[test] per scheduler: 100 seeds, each run twice, all pairs identical.
// ---------------------------------------------------------------------------

#[test]
fn determinism_fuzz_simple() {
    let _lock = common::setup_test();
    fuzz_scheduler("simple", |_n| DynamicScheduler::simple());
}

#[test]
fn determinism_fuzz_lavd() {
    let _lock = common::setup_test();
    fuzz_scheduler("lavd", DynamicScheduler::lavd);
}

#[test]
fn determinism_fuzz_cosmos() {
    let _lock = common::setup_test();
    fuzz_scheduler("cosmos", DynamicScheduler::cosmos);
}

/// Guard against a vacuous suite: the fuzzer must actually generate 100 distinct
/// seeds and distinct scenario shapes (otherwise "100 seeds" would be a lie).
#[test]
fn fuzzer_generates_distinct_seeds_and_varied_scenarios() {
    let _lock = common::setup_test();
    let seeds = gen_seeds();
    assert_eq!(seeds.len(), NUM_SEEDS);
    let distinct: std::collections::HashSet<u32> = seeds.iter().copied().collect();
    assert_eq!(distinct.len(), NUM_SEEDS, "seeds are not all distinct");

    // The derived scenarios must span more than one CPU count (proof the fuzz
    // actually varies scenario shape, not just the PRNG seed).
    let cpu_counts: std::collections::HashSet<u32> =
        seeds.iter().map(|&s| scenario_for_seed(s).0).collect();
    assert!(
        cpu_counts.len() >= 2,
        "fuzzed scenarios did not vary CPU count: {cpu_counts:?}"
    );
}
