//! Tick-timer accuracy & event-ordering tests (tg test-tick-timer-accuracy).
//!
//! The simulator's periodic timer is the per-CPU scheduler tick, fired every
//! `TICK_INTERVAL_NS = 4ms` (HZ=250; see `safe/engine.rs`). These tests pin the
//! tick's *accuracy* (rate + interval regularity, noise-off and under jitter),
//! timer-event time *monotonicity*, and deterministic ordering of events that
//! land on the same timestamp — beyond the existing `tick.rs` (which only checks
//! that some ticks are recorded).
//!
//! ## Note on items 3 & 4 (multiple BPF timers / cancellation / rescheduling)
//! Besides the tick, the only other timers in the sim are *scheduler-internal*
//! BPF timers (`EventKind::TimerFired`, driven by each scheduler's
//! `sim_timer_start` → `<sched>_fire_timer`, e.g. cgroup_bw replenish/accounting
//! and LAVD/cosmos wakeup timers). Their period, cancellation, and rescheduling
//! are decided by the BPF scheduler, not by the workload, so they are not
//! black-box configurable through the public scenario API. They are exercised
//! indirectly by the cgroup-bandwidth and scheduler suites; here we test the
//! one workload-observable periodic timer (the tick) and the engine's event
//! time ordering, which every timer feeds into.

use scx_simulator::*;

#[macro_use]
mod common;

/// Nominal tick interval: HZ=250 -> 4ms (mirrors engine.rs TICK_INTERVAL_NS).
const TICK_INTERVAL_NS: u64 = 4_000_000;

type SchedFactory = fn(u32) -> DynamicScheduler;

fn schedulers() -> [(&'static str, SchedFactory); 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

fn hog(pid: i32) -> TaskDef {
    TaskDef {
        name: format!("hog{pid}"),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(1_000_000_000)], // effectively always on-CPU
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Tick timestamps observed on a given CPU, in trace order.
fn tick_times_on(trace: &Trace, cpu: CpuId) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::Tick { .. }))
        .map(|e| e.time_ns)
        .collect()
}

/// Item 1/2: the tick rate matches HZ. On a single always-busy CPU with noise
/// disabled, the number of ticks over a fixed duration must equal
/// `duration / TICK_INTERVAL_NS` (within +/-1 for boundary effects).
#[test]
fn tick_rate_matches_hz_noise_off() {
    let _lock = common::setup_test();
    let duration_ms = 100u64;
    let expected = (duration_ms * 1_000_000) / TICK_INTERVAL_NS; // 100ms / 4ms = 25

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(1)
            .noise(false)
            .task(hog(1))
            .duration_ms(duration_ms)
            .build();
        let trace = Simulator::new(make(1)).run(scenario);

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");
        let ticks = trace.tick_count(CpuId(0)) as u64;
        assert!(
            ticks.abs_diff(expected) <= 1,
            "{name}: expected ~{expected} ticks over {duration_ms}ms at 4ms/tick, got {ticks}"
        );
    }
}

/// Item 1/2: with noise disabled the tick interval is *regular and accurate*.
/// Every consecutive gap on a busy CPU is identical (perfectly regular, since
/// there is no jitter) and within a few microseconds of the nominal 4ms. Note
/// the deterministic spacing is 4ms plus a small fixed per-tick scheduling
/// overhead (~250ns), so we assert equality-across-gaps plus closeness to
/// nominal rather than an exact 4ms literal.
#[test]
fn tick_intervals_regular_noise_off() {
    let _lock = common::setup_test();

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(1)
            .noise(false)
            .task(hog(1))
            .duration_ms(80)
            .build();
        let trace = Simulator::new(make(1)).run(scenario);

        let times = tick_times_on(&trace, CpuId(0));
        assert!(
            times.len() >= 5,
            "{name}: too few ticks ({}) to check spacing",
            times.len()
        );
        let deltas: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        let min = *deltas.iter().min().unwrap();
        let max = *deltas.iter().max().unwrap();
        // Regularity: with no jitter the gaps are near-constant. (The first gap
        // is exactly 4ms; steady-state gaps add a fixed ~250ns per-tick
        // scheduling overhead, so the spread is tiny and deterministic.)
        assert!(
            max - min <= 1_000,
            "{name}: tick spacing not regular under noise-off: spread {}ns (min={min}, max={max})",
            max - min
        );
        // Accuracy: every gap within a few us of the nominal 4ms.
        for &delta in &deltas {
            assert!(
                delta.abs_diff(TICK_INTERVAL_NS) <= 10_000,
                "{name}: tick interval {delta}ns strayed >10us from nominal {TICK_INTERVAL_NS}ns"
            );
        }
    }
}

/// Item 1/2: with noise *enabled*, tick spacing jitters around the nominal 4ms
/// but stays close (the jitter stddev is a few microseconds) and never goes
/// backward or to zero — the timer stays well-behaved under jitter.
#[test]
fn tick_intervals_bounded_under_noise() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        .noise(true)
        .seed(12345)
        .task(hog(1))
        .duration_ms(120)
        .build();
    let trace = Simulator::new(DynamicScheduler::lavd(1)).run(scenario);

    let times = tick_times_on(&trace, CpuId(0));
    assert!(
        times.len() >= 10,
        "too few ticks to assess jitter: {}",
        times.len()
    );
    for w in times.windows(2) {
        let delta = w[1] - w[0];
        // Jitter stddev is ~2us; allow a generous +/-1ms band around 4ms and
        // require strictly positive progress.
        assert!(delta > 0, "tick interval must be positive, got {delta}");
        assert!(
            delta.abs_diff(TICK_INTERVAL_NS) <= 1_000_000,
            "tick interval {delta}ns strayed >1ms from nominal {TICK_INTERVAL_NS}ns under noise"
        );
    }
}

/// Item 6 / item 2: the periodic timer fires in strict time order. Every `Tick`
/// event's timestamp is >= the previous tick's (globally, across all CPUs), and
/// strictly increasing per CPU. This is the clean, meaningful monotonicity
/// invariant for the timer subsystem.
///
/// (Note: the *raw* trace stream is intentionally not globally sorted — the
/// startup preamble records setup/initial-wakeup bookkeeping on small, out-of-
/// order timestamps before the sim clock settles — so whole-stream monotonicity
/// is not a valid invariant. The timer events themselves, which drive tick
/// accounting, are the ordered signal and are what this test pins.)
#[test]
fn tick_events_monotonic_in_time() {
    let _lock = common::setup_test();
    let nr = 4;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr).seed(7);
        for i in 0..6 {
            b = b.add_task(
                &format!("t{i}"),
                (i % 3) as i8 - 1,
                TaskBehavior {
                    phases: vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let trace = Simulator::new(make(nr)).run(b.duration_ms(100).build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        // Global: tick timestamps are non-decreasing in recording (= processing)
        // order across all CPUs.
        let mut prev = 0u64;
        let mut n_ticks = 0usize;
        for e in trace.events() {
            if matches!(e.kind, TraceKind::Tick { .. }) {
                assert!(
                    e.time_ns >= prev,
                    "{name}: tick time went backward: {} < {prev}",
                    e.time_ns
                );
                prev = e.time_ns;
                n_ticks += 1;
            }
        }
        assert!(n_ticks > 0, "{name}: no ticks recorded");

        // Per CPU: ticks are strictly increasing (no duplicate/stalled timer).
        for cpu in 0..nr {
            let times = tick_times_on(&trace, CpuId(cpu));
            for w in times.windows(2) {
                assert!(
                    w[1] > w[0],
                    "{name}: cpu{cpu} tick timestamps not strictly increasing: {} then {}",
                    w[0],
                    w[1]
                );
            }
        }
    }
}

/// Item 5: when multiple events fire at the same simulated timestamp, their
/// relative ordering is deterministic across runs (the engine's event queue uses
/// a seeded tie-break). Confirms (a) same-timestamp collisions actually occur,
/// and (b) two identical runs order the colliding events identically.
#[test]
fn same_timestamp_event_ordering_deterministic() {
    let _lock = common::setup_test();
    let nr = 4;

    let run = || {
        // Many tasks starting together => bursts of same-timestamp events.
        let mut b = Scenario::builder().cpus(nr).seed(99);
        for i in 0..8 {
            b = b.add_task(
                &format!("t{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(5_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        Simulator::new(DynamicScheduler::lavd(nr)).run(b.duration_ms(60).build())
    };

    let t1 = run();
    let t2 = run();

    // (a) collisions exist: at least one timestamp carries >= 2 events.
    let mut max_run = 1usize;
    let mut cur = 1usize;
    let ev1 = t1.events();
    for w in ev1.windows(2) {
        if w[0].time_ns == w[1].time_ns {
            cur += 1;
            max_run = max_run.max(cur);
        } else {
            cur = 1;
        }
    }
    assert!(
        max_run >= 2,
        "expected same-timestamp event collisions to exercise tie-breaking, max run={max_run}"
    );

    // (b) the ordering of colliding events is identical across runs.
    let ev2 = t2.events();
    assert_eq!(ev1.len(), ev2.len(), "run length differs");
    for (i, (a, b)) in ev1.iter().zip(ev2.iter()).enumerate() {
        assert_eq!(
            (a.time_ns, a.cpu, format!("{:?}", a.kind)),
            (b.time_ns, b.cpu, format!("{:?}", b.kind)),
            "event {i} ordering differs across identical runs (same-tick order not deterministic)"
        );
    }
}
