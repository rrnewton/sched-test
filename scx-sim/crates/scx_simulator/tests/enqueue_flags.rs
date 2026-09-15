//! Tests for enqueue-flag (`SCX_ENQ_*` / `SCX_WAKE_*`) handling, exercised
//! across all three natively-supported schedulers (simple, lavd, cosmos).
//!
//! # What the simulator models (and what it does not)
//!
//! In sched_ext the KERNEL computes the enqueue/wake flags and passes them to
//! the BPF scheduler's `runnable` / `select_cpu` / `enqueue` callbacks; the
//! scheduler only READS them. scxsim mirrors that split: the engine
//! (`safe/engine.rs`) is the kernel and computes the flags, so the flag VALUE
//! delivered for a given event is scheduler-independent. That is exactly what
//! these tests pin down.
//!
//! The flags are observable on two `TraceKind`s:
//!
//! * `Runnable { enq_flags }` — emitted for every wake (always fires, even
//!   when the wake is immediately direct-dispatched).
//! * `EnqueueTask { enq_flags }` — emitted when a task is placed on a DSQ via
//!   `ops.enqueue` (skipped when `select_cpu` direct-dispatches).
//!
//! What the engine actually emits today (verified empirically):
//!
//! * `SCX_ENQ_WAKEUP` (0x1) on every wake.
//! * `SCX_WAKE_SYNC` (0x10) additionally for a waker-driven (synchronous)
//!   wake — a `Phase::Wake` where one task wakes another — giving
//!   `Runnable.enq_flags == 0x11`. Non-waker wakes (initial activation,
//!   self-wake from sleep) stay at plain `0x1`.
//! * `enq_flags == 0` on a re-enqueue that is NOT a wake (slice-expiry and
//!   voluntary-yield re-enqueue).
//!
//! What the engine does NOT model (so these tests do NOT assert it — see the
//! filed issues): `SCX_WAKE_FORK` on a task's first activation (mb sim-5fa8dd
//! — note "SCX_ENQ_FORK" is not a real kernel flag; fork is a WAKE flag), and
//! the other enqueue flags `SCX_ENQ_HEAD/PREEMPT/REENQ/LAST/NESTED` (never
//! emitted). Kernel-fidelity policy allows modelling a subset; the tests
//! assert only bits the engine genuinely delivers, and additionally assert
//! that NO unmodeled bit ever leaks in.
//!
//! Kernel-fidelity note: every assertion is on flags the engine (kernel)
//! delivers to the real scheduler `.so`; nothing here stubs or approximates
//! scheduler logic. Scenarios are deterministic (`seed` + `instant_timing`).

use scx_simulator::*;
use std::collections::BTreeSet;

mod common;

/// `SCX_ENQ_WAKEUP` — set when the enqueue is caused by a wakeup.
const SCX_ENQ_WAKEUP: u64 = 0x1;
/// `SCX_WAKE_SYNC` — synchronous wakeup (waker is about to yield the CPU).
/// A wake flag (kernel value 16) that the engine folds into the flags it
/// passes down the runnable/select_cpu path.
const SCX_WAKE_SYNC: u64 = 0x10;
/// The complete set of flag bits the engine is expected to ever emit. Any
/// other bit appearing in a trace is an unmodeled-flag regression.
const KNOWN_FLAG_BITS: u64 = SCX_ENQ_WAKEUP | SCX_WAKE_SYNC;

/// The three natively-supported schedulers. `simple` is single-CPU only, so
/// cross-scheduler scenarios use a single CPU where all three are comparable.
const SCHEDS: &[&str] = &["simple", "lavd", "cosmos"];

fn make_sched(name: &str, ncpu: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(ncpu),
        "cosmos" => DynamicScheduler::cosmos(ncpu),
        other => panic!("unknown scheduler {other}"),
    }
}

fn run_forever(ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(ns)],
        repeat: RepeatMode::Forever,
    }
}

fn sleeper(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// `enq_flags` from every `Runnable` event (one per wake).
fn runnable_flags(trace: &Trace) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Runnable { enq_flags, .. } => Some(*enq_flags),
            _ => None,
        })
        .collect()
}

/// `enq_flags` from every `EnqueueTask` event (one per `ops.enqueue`).
fn enqueue_flags(trace: &Trace) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::EnqueueTask { enq_flags, .. } => Some(*enq_flags),
            _ => None,
        })
        .collect()
}

fn distinct(vals: &[u64]) -> BTreeSet<u64> {
    vals.iter().copied().collect()
}

/// Contention scenario: two CPU-bound tasks plus a sleeper, on `ncpu` CPUs.
/// Produces plain (non-sync) wakeups (initial activations + the sleeper's
/// self-wakes) AND `enq_flags==0` re-enqueues (slice-expiry / yield).
fn contention_scenario(ncpu: u32) -> Scenario {
    Scenario::builder()
        .cpus(ncpu)
        .seed(42)
        .instant_timing()
        .add_task("a", 0, run_forever(5_000_000))
        .add_task("b", 0, run_forever(5_000_000))
        .add_task("sleeper", 0, sleeper(3_000_000, 4_000_000))
        .duration_ms(60)
        .build()
}

/// Ping-pong scenario: two tasks that wake each other via `Phase::Wake`,
/// producing synchronous (`SCX_WAKE_SYNC`) wakeups.
fn pingpong_scenario(ncpu: u32) -> Scenario {
    let (ping, pong) = workloads::ping_pong(Pid(1), Pid(2), 2_000_000);
    Scenario::builder()
        .cpus(ncpu)
        .seed(42)
        .instant_timing()
        .add_task("ping", 0, ping)
        .add_task("pong", 0, pong)
        .duration_ms(60)
        .build()
}

// ---------------------------------------------------------------------------
// 1. SCX_ENQ_WAKEUP is set on every wake — for all three schedulers.
// ---------------------------------------------------------------------------

#[test]
fn test_wakeup_flag_set_on_every_wake_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let trace = Simulator::new(make_sched(name, 1)).run(contention_scenario(1));
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: should not stall"
        );

        let flags = runnable_flags(&trace);
        assert!(
            !flags.is_empty(),
            "{name}: expected at least one Runnable (wake) event"
        );
        // Every wake carries the WAKEUP bit ...
        for f in &flags {
            assert!(
                f & SCX_ENQ_WAKEUP != 0,
                "{name}: Runnable enq_flags {f:#x} is missing SCX_ENQ_WAKEUP"
            );
        }
        // ... and, with no waker anywhere in this scenario, the SYNC bit is
        // never set: the only value observed is exactly SCX_ENQ_WAKEUP.
        assert_eq!(
            distinct(&flags),
            BTreeSet::from([SCX_ENQ_WAKEUP]),
            "{name}: non-waker scenario should yield only plain WAKEUP wakes"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. SCX_WAKE_SYNC marks synchronous (waker-driven) wakes — all schedulers.
// ---------------------------------------------------------------------------

#[test]
fn test_sync_flag_on_waker_driven_wakes_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let trace = Simulator::new(make_sched(name, 1)).run(pingpong_scenario(1));
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: should not stall"
        );

        let flags = runnable_flags(&trace);
        assert!(!flags.is_empty(), "{name}: expected wake events");

        // At least one synchronous wake (WAKEUP|SYNC) — the ping-pong hand-off.
        let sync_wakes = flags.iter().filter(|f| *f & SCX_WAKE_SYNC != 0).count();
        assert!(
            sync_wakes > 0,
            "{name}: expected >=1 synchronous (SCX_WAKE_SYNC) wake in ping-pong; \
             observed flags={:?}",
            distinct(&flags)
        );

        // SYNC never appears on its own: a synchronous wake is still a wake,
        // so the WAKEUP bit must accompany it (combination semantics).
        for f in &flags {
            if f & SCX_WAKE_SYNC != 0 {
                assert!(
                    f & SCX_ENQ_WAKEUP != 0,
                    "{name}: SCX_WAKE_SYNC set without SCX_ENQ_WAKEUP ({f:#x})"
                );
            }
        }

        // The two initial activations (no waker) show up as plain WAKEUP, so
        // both the sync (0x11) and non-sync (0x1) values coexist.
        assert!(
            distinct(&flags).contains(&SCX_ENQ_WAKEUP),
            "{name}: expected plain WAKEUP for the initial (non-waker) wakes; \
             observed flags={:?}",
            distinct(&flags)
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Re-enqueue (non-wake) carries no WAKEUP bit — all schedulers.
// ---------------------------------------------------------------------------

#[test]
fn test_reenqueue_has_no_wakeup_flag_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let trace = Simulator::new(make_sched(name, 1)).run(contention_scenario(1));
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: should not stall"
        );

        let flags = enqueue_flags(&trace);
        assert!(
            !flags.is_empty(),
            "{name}: expected EnqueueTask events under contention"
        );

        // Slice-expiry / yield re-enqueues are NOT wakeups: they appear with
        // enq_flags == 0. At least one must be present under contention.
        assert!(
            flags.contains(&0),
            "{name}: expected >=1 re-enqueue with enq_flags=0; observed {:?}",
            distinct(&flags)
        );

        // Every EnqueueTask is either a re-enqueue (0) or a wakeup-enqueue
        // (WAKEUP bit set) — never a bare SYNC and never an unmodeled bit.
        for f in &flags {
            assert!(
                *f == 0 || f & SCX_ENQ_WAKEUP != 0,
                "{name}: EnqueueTask enq_flags {f:#x} is neither 0 (re-enqueue) \
                 nor a WAKEUP enqueue"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. WAKEUP wakes and zero-flag re-enqueues are both present and distinct.
// ---------------------------------------------------------------------------

/// The core "SCX_ENQ_WAKEUP vs other flags" distinction: within a single run
/// a wake (WAKEUP bit set, via Runnable) and a re-enqueue (enq_flags==0, via
/// EnqueueTask) coexist and are cleanly distinguishable — for every scheduler.
#[test]
fn test_wakeup_vs_reenqueue_distinct_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let trace = Simulator::new(make_sched(name, 1)).run(contention_scenario(1));
        assert_eq!(trace.exit_kind(), &ExitKind::Normal);

        let woke_with_wakeup = runnable_flags(&trace)
            .iter()
            .any(|f| f & SCX_ENQ_WAKEUP != 0);
        let reenqueued_without_wakeup = enqueue_flags(&trace).contains(&0);

        assert!(
            woke_with_wakeup,
            "{name}: expected at least one WAKEUP-flagged wake"
        );
        assert!(
            reenqueued_without_wakeup,
            "{name}: expected at least one zero-flag (non-wakeup) re-enqueue"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Only modeled flag bits ever appear (no unmodeled-flag leakage).
// ---------------------------------------------------------------------------

#[test]
fn test_only_known_flag_bits_appear_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        // Exercise both wake kinds in one sweep per scheduler.
        for scenario in [contention_scenario(1), pingpong_scenario(1)] {
            let trace = Simulator::new(make_sched(name, 1)).run(scenario);
            assert_eq!(
                trace.exit_kind(),
                &ExitKind::Normal,
                "{name}: should not stall"
            );

            let mut all = runnable_flags(&trace);
            all.extend(enqueue_flags(&trace));
            for f in &all {
                assert_eq!(
                    f & !KNOWN_FLAG_BITS,
                    0,
                    "{name}: enq_flags {f:#x} contains an unmodeled bit \
                     (known bits = {KNOWN_FLAG_BITS:#x}); if a new flag was \
                     intentionally added, update KNOWN_FLAG_BITS and mb sim-5fa8dd"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Flag DELIVERY is scheduler-independent (kernel sets flags, not the sched).
// ---------------------------------------------------------------------------

/// The engine (kernel) computes enqueue flags identically regardless of which
/// scheduler is loaded — the scheduler only reads them. So for the same
/// scenario the SET of distinct flag values delivered must match across
/// simple/lavd/cosmos. (Counts may differ because each scheduler makes
/// different dispatch decisions, but the flag VOCABULARY the kernel hands out
/// does not.)
#[test]
fn test_flag_delivery_consistent_across_schedulers() {
    let _lock = common::setup_test();

    // Non-waker scenario: every scheduler should see exactly {WAKEUP}.
    let contention_sets: Vec<BTreeSet<u64>> = SCHEDS
        .iter()
        .map(|&name| {
            let trace = Simulator::new(make_sched(name, 1)).run(contention_scenario(1));
            distinct(&runnable_flags(&trace))
        })
        .collect();
    for (name, set) in SCHEDS.iter().zip(&contention_sets) {
        assert_eq!(
            set,
            &BTreeSet::from([SCX_ENQ_WAKEUP]),
            "{name}: expected exactly {{WAKEUP}} in the non-waker scenario, got {set:?}"
        );
    }

    // Waker scenario: every scheduler should see {WAKEUP, WAKEUP|SYNC}.
    let expected_sync = BTreeSet::from([SCX_ENQ_WAKEUP, SCX_ENQ_WAKEUP | SCX_WAKE_SYNC]);
    for &name in SCHEDS {
        let trace = Simulator::new(make_sched(name, 1)).run(pingpong_scenario(1));
        assert_eq!(
            distinct(&runnable_flags(&trace)),
            expected_sync,
            "{name}: expected {{WAKEUP, WAKEUP|SYNC}} in the ping-pong scenario"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Determinism: the flag stream is fully reproducible for a fixed seed.
// ---------------------------------------------------------------------------

#[test]
fn test_enqueue_flag_stream_deterministic() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let t1 = Simulator::new(make_sched(name, 1)).run(pingpong_scenario(1));
        let t2 = Simulator::new(make_sched(name, 1)).run(pingpong_scenario(1));
        assert_eq!(
            runnable_flags(&t1),
            runnable_flags(&t2),
            "{name}: Runnable flag stream not deterministic"
        );
        assert_eq!(
            enqueue_flags(&t1),
            enqueue_flags(&t2),
            "{name}: EnqueueTask flag stream not deterministic"
        );
    }
}
