//! Determinism & deterministic-replay tests (tg test-determinism-replay).
//!
//! scxsim's core reproducibility guarantee is *seed-based logical determinism*:
//! for a fixed scenario + seed, the event-driven engine produces a byte-identical
//! trace, independent of wall-clock, run order, or host. This holds even with
//! timing noise enabled (the default), because the noise (tick/run jitter) is
//! driven by a seeded `SmallRng` (`engine.rs`: `SmallRng::seed_from_u64(seed)`).
//!
//! These tests verify, across every supported scheduler:
//!   1. same workload + same seed  -> byte-identical trace  (same_seed_byte_identical)
//!   2. same workload + diff seed  -> the trace *changes*    (seed_sensitivity)
//!      (proving determinism is genuinely seed-driven, not "seed ignored")
//!   3. noise disabled             -> exact & reproducible per-seed (noise_off_exact)
//!   4. cooperative preemptive mode is deterministic          (cooperative_preemptive)
//!
//! Byte-identical is checked via `trace_signature()` — the full `Debug`
//! serialization of the event stream. `TraceKind` derives `Eq` and contains
//! only logical IDs (`Pid`/`CpuId`/`DsqId`/flags), never raw addresses, so its
//! `Debug` form is stable across runs and is a faithful proxy for the recorded
//! trace bytes.
//!
//! Record-replay determinism (item 5) requires PMU hardware + hardware
//! breakpoints, which are unavailable in VMs/containers/CI (the replay backend
//! panics without them — see `unsafe_impl/backend/replay.rs`) and whose raw
//! PMU-RBC recording is subject to the ~10% nondeterminism tracked in
//! mb sim-70abc8. It is therefore `#[ignore]`d and run manually on capable
//! hardware; see `record_replay_determinism` below.

use scx_simulator::*;

#[macro_use]
mod common;

/// Constructs a scheduler for a given CPU count.
type SchedFactory = fn(u32) -> DynamicScheduler;

/// A byte-identical signature of a trace: the full `Debug` serialization of the
/// event stream (see module docs for why this is address-stable).
fn trace_signature(trace: &Trace) -> String {
    format!("{:?}", trace.events())
}

/// The supported schedulers, as `(name, factory)` pairs. `simple` ignores the
/// CPU count; the rest take it.
fn schedulers() -> [(&'static str, SchedFactory); 5] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
        ("mitosis", DynamicScheduler::mitosis),
        ("tickless", DynamicScheduler::tickless),
    ]
}

/// A mixed multi-task workload (CPU-bound + sleep/wake cyclers, varied nice)
/// that gives timing noise something to perturb, so that seed sensitivity is
/// observable when noise is enabled.
fn det_scenario(nr_cpus: u32, seed: u32, noise: bool) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).seed(seed).noise(noise);
    for i in 0..5u32 {
        let nice = ((i as i16 % 5) - 2) as i8;
        b = b.add_task(
            &format!("t{i}"),
            nice,
            TaskBehavior {
                phases: vec![
                    Phase::Run(3_000_000 + (i as u64) * 400_000),
                    Phase::Sleep(1_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
        );
    }
    b.duration_ms(120).build()
}

/// (1) Same workload + same seed => byte-identical trace, for every scheduler.
///
/// Runs each scheduler twice with an explicit seed and noise enabled (the
/// production default), and asserts the two traces are byte-for-byte identical.
#[test]
fn determinism_same_seed_byte_identical() {
    let _lock = common::setup_test();
    let nr = 4;
    let seed = 0x5eed_1234;

    for (name, make) in schedulers() {
        let t1 = Simulator::new(make(nr)).run(det_scenario(nr, seed, true));
        let t2 = Simulator::new(make(nr)).run(det_scenario(nr, seed, true));

        assert!(
            !t1.has_error(),
            "{name}: run 1 errored: {:?}",
            t1.exit_kind()
        );
        assert!(
            !t2.has_error(),
            "{name}: run 2 errored: {:?}",
            t2.exit_kind()
        );
        assert_eq!(
            t1.events().len(),
            t2.events().len(),
            "{name}: event counts differ ({} vs {}) for the same seed",
            t1.events().len(),
            t2.events().len()
        );
        assert!(
            !t1.events().is_empty(),
            "{name}: trace is empty — nothing was exercised"
        );
        assert_eq!(
            trace_signature(&t1),
            trace_signature(&t2),
            "{name}: traces are not byte-identical for the same seed"
        );
    }
}

/// (2) Same workload + *different* seed => the trace changes.
///
/// This guards against a subtle failure mode where "determinism" is achieved by
/// simply ignoring the seed: if the seed did not drive the noise PRNG, all seeds
/// would yield the same trace and the same-seed test above would be vacuous.
/// With noise on (20% run-jitter CV), at least one of several distinct seeds
/// must perturb the schedule. Uses LAVD (rich enough that jitter reorders work).
#[test]
fn determinism_seed_sensitivity() {
    let _lock = common::setup_test();
    let nr = 4;

    let sig = |seed: u32| {
        trace_signature(
            &Simulator::new(DynamicScheduler::lavd(nr)).run(det_scenario(nr, seed, true)),
        )
    };

    let base = sig(1);
    // Control: the same seed must reproduce exactly.
    assert_eq!(base, sig(1), "same seed must reproduce byte-identically");

    // At least one different seed must change the trace.
    let differs = (2..12u32).any(|s| sig(s) != base);
    assert!(
        differs,
        "no seed in 2..12 changed the trace with noise enabled — the seed does \
         not appear to drive the noise PRNG (determinism would be vacuous)"
    );
}

/// (3) With noise disabled, the run is exactly reproducible (same seed).
///
/// This exercises the no-jitter engine path and confirms it is byte-identical
/// across runs. Note: noise-off is *not* seed-independent — the event queue's
/// tie-break RNG is always seeded from `scenario.seed` (engine.rs), so the seed
/// still determines the ordering of same-timestamp events even without jitter.
/// Determinism is therefore always *per-seed*, which is exactly what
/// reproducibility requires.
#[test]
fn determinism_noise_off_exact() {
    let _lock = common::setup_test();
    let nr = 4;
    let seed = 20_260_723;

    for (name, make) in schedulers() {
        let t1 = Simulator::new(make(nr)).run(det_scenario(nr, seed, false));
        let t2 = Simulator::new(make(nr)).run(det_scenario(nr, seed, false));

        assert!(!t1.has_error(), "{name}: errored: {:?}", t1.exit_kind());
        assert_eq!(
            trace_signature(&t1),
            trace_signature(&t2),
            "{name}: noise-off traces should be byte-identical for the same seed"
        );
    }
}

/// (4) Cooperative preemptive interleaving is deterministic.
///
/// `PreemptiveConfig::cooperative_only()` drives mid-C-code interleaving via the
/// futex `PreemptRing` and yields only at kfunc boundaries — no PMU timers — so
/// the interleaving is fully deterministic (unlike raw PMU-RBC, mb sim-70abc8).
/// This is the reliable form of "RBC-based preemption determinism". Verified for
/// the schedulers that go through the preemptive dispatch path.
#[test]
fn determinism_cooperative_preemptive() {
    let _lock = common::setup_test();
    let nr = 4;

    let scheds: [(&str, SchedFactory); 3] = [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ];

    for (name, make) in scheds {
        let mk = || {
            let mut b = Scenario::builder()
                .cpus(nr)
                .seed(42)
                .preemptive(PreemptiveConfig::cooperative_only());
            for i in 0..3u32 {
                b = b.add_task(
                    &format!("t{i}"),
                    0,
                    TaskBehavior {
                        phases: vec![Phase::Run(4_000_000), Phase::Sleep(1_000_000)],
                        repeat: RepeatMode::Forever,
                    },
                );
            }
            b.duration_ms(60).build()
        };

        let t1 = Simulator::new(make(nr)).run(mk());
        let t2 = Simulator::new(make(nr)).run(mk());

        assert!(!t1.has_error(), "{name}: errored: {:?}", t1.exit_kind());
        assert_eq!(
            trace_signature(&t1),
            trace_signature(&t2),
            "{name}: cooperative-preemptive traces are not deterministic"
        );
    }
}

/// (5) Record-replay determinism — replaying a *fixed* recorded preemption trace
/// must be byte-identical across runs.
///
/// IGNORED BY DEFAULT: recording drives real PMU counters and replay uses
/// hardware breakpoints; both are unavailable in VMs/containers/CI, where the
/// replay backend panics by design (see `unsafe_impl/backend/replay.rs`).
/// Raw PMU-RBC *recording* is also subject to the ~10% nondeterminism tracked in
/// mb sim-70abc8 — which is exactly why replay exists: a trace is recorded once,
/// then replay is deterministic *by construction*. This test asserts that
/// determinism of replay (two replays of the same recorded trace match),
/// independent of the recording's nondeterminism.
///
/// Run on PMU-capable hardware with:
///   cargo test -p scx_simulator --test determinism -- --ignored record_replay
#[test]
#[ignore = "requires PMU hardware + HW breakpoints (unavailable in CI); see mb sim-70abc8"]
fn record_replay_determinism() {
    let _lock = common::setup_test();
    let nr = 4u32;

    let record_scenario = || {
        let mut b = Scenario::builder()
            .cpus(nr)
            .seed(42)
            .preemptive(PreemptiveConfig::default()); // PMU mode
        for i in 0..3u32 {
            b = b.add_task(
                &format!("t{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(4_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        b.duration_ms(60).build()
    };

    // Record a preemption trace from a PMU-driven run.
    enable_preemption_collection();
    let _ = Simulator::new(DynamicScheduler::simple()).run(record_scenario());
    let records = drain_preemption_records();
    let ptrace =
        PreemptionTrace::from_records(&records, nr as usize, PmuEvent::RetiredBranchConditional);

    // Replay the SAME fixed trace twice; the two replays must match byte-for-byte.
    let replay_scenario = || {
        let mut b = Scenario::builder()
            .cpus(nr)
            .seed(42)
            .preemptive(PreemptiveConfig::default())
            .replay_trace(ptrace.clone());
        for i in 0..3u32 {
            b = b.add_task(
                &format!("t{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(4_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        b.duration_ms(60).build()
    };

    let r1 = Simulator::new(DynamicScheduler::simple()).run(replay_scenario());
    let r2 = Simulator::new(DynamicScheduler::simple()).run(replay_scenario());

    assert_eq!(
        trace_signature(&r1),
        trace_signature(&r2),
        "replaying a fixed recorded preemption trace must be deterministic"
    );
}
