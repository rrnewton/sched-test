//! Tick-timer accuracy & event-ordering tests (tg test-tick-timer-accuracy).
//!
//! The simulator's periodic timer is the per-CPU scheduler tick, fired every
//! `TICK_INTERVAL_NS = 4ms` (HZ=250; see `safe/engine.rs`). These tests pin the
//! tick's *accuracy* (rate + interval regularity, noise-off and under jitter),
//! timer-event time *monotonicity*, and deterministic ordering of events that
//! land on the same timestamp — beyond the existing `tick.rs` (which only checks
//! that some ticks are recorded).
//!
//! ## Coverage of items 3 & 4 (multiple periods / cancellation / rescheduling)
//! Besides the tick, the workload-observable periodic timers reachable through
//! the public scenario API are:
//!   * the per-CPU tick chains — N independent timers of the *same* period
//!     (`per_cpu_tick_timers_are_independent`);
//!   * injected periodic IRQs (`ScenarioBuilder::periodic_irq`) — arbitrary,
//!     *different* periods on different CPUs, each firing on its own grid
//!     (`periodic_irq_timer_fires_on_exact_grid`,
//!     `multiple_periodic_timers_distinct_periods`); and
//!   * a task's sleep→wake timer (`Phase::Sleep` → `TaskWake`), cancelled and
//!     re-armed every cycle (`periodic_sleeper_wake_timer_reschedules_*`).
//!
//! Tick-timer *cancellation and rescheduling* is exercised directly via CPU
//! hotplug: taking a CPU offline stops its tick chain and bringing it back
//! online restarts it (`tick_timer_cancelled_offline_and_restarted_online`).
//!
//! The remaining timers — *scheduler-internal* BPF timers
//! (`EventKind::TimerFired`, driven by each scheduler's `sim_timer_start` →
//! `<sched>_fire_timer`, e.g. cgroup_bw replenish/accounting and LAVD/cosmos
//! wakeup timers) — have periods/cancellation decided by the BPF scheduler, not
//! the workload, so they are not black-box configurable here; they are covered
//! indirectly by the cgroup-bandwidth and scheduler suites.

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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
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

// ===========================================================================
// Items 3 & 4: multiple timers with distinct periods; cancellation + reschedule
//
// These extend the file beyond the tick alone. They use the public scenario
// API — per-CPU tick chains, injected periodic IRQs, CPU hotplug, and the
// sleep→wake timer — to cover the two requirements the original suite skipped.
// `.instant_timing()` (noise + overhead off) makes every timer land on an exact
// grid, so these assert precise firing times, not just approximate rates.
// ===========================================================================

/// A CPU hog pinned to a single CPU (explicit PID). Used to keep the sim alive
/// and a chosen CPU busy while *other* CPUs stay idle for IRQ-timer observation.
fn pinned_hog(pid: i32, cpu: CpuId) -> TaskDef {
    let mut def = hog(pid);
    def.name = format!("pinned{pid}");
    def.allowed_cpus = Some(vec![cpu]);
    def
}

/// `IrqStart` timestamps on `cpu`, in trace order.
fn irq_start_times_on(trace: &Trace, cpu: CpuId) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::IrqStart { .. }))
        .map(|e| e.time_ns)
        .collect()
}

/// `TaskWoke` timestamps for `pid`, in trace order.
fn wake_times_of(trace: &Trace, pid: Pid) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter(|e| matches!(&e.kind, TraceKind::TaskWoke { pid: p } if *p == pid))
        .map(|e| e.time_ns)
        .collect()
}

/// Item 3: the per-CPU tick chains are N independent timers of the *same*
/// period. On a saturated 4-CPU box every CPU must accrue its own
/// ~duration/4ms ticks — no CPU is starved of ticks by another. Swept across
/// schedulers because the tick is engine-driven, independent of policy.
#[test]
fn per_cpu_tick_timers_are_independent() {
    let _lock = common::setup_test();
    let nr = 4;
    let expected = (60u64 * 1_000_000) / TICK_INTERVAL_NS; // 15

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr).instant_timing();
        for i in 0..(nr * 2) {
            b = b.task(hog(i as i32 + 1));
        }
        let trace = Simulator::new(make(nr)).run(b.duration_ms(60).build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        for cpu in 0..nr {
            let n = trace.tick_count(CpuId(cpu)) as u64;
            assert!(
                n.abs_diff(expected) <= 2,
                "{name}: CPU{cpu} accrued {n} ticks (expected ~{expected})"
            );
        }
    }
}

/// Item 2/3: an injected periodic IRQ timer fires *exactly* on its configured
/// grid. One task is pinned to CPU0 (keeps the sim alive + CPU0 busy); CPU1 is
/// idle and carries a hardirq every 10ms. Recorded `IrqStart` times must be
/// exactly {10ms, 20ms, ..., 100ms} — a periodic timer with a non-tick period
/// landing precisely on schedule.
#[test]
fn periodic_irq_timer_fires_on_exact_grid() {
    let _lock = common::setup_test();
    let period_ns = 10_000_000u64; // 10ms
    let dur_ms = 100u64;

    let scenario = Scenario::builder()
        .cpus(2)
        .instant_timing()
        .task(pinned_hog(1, CpuId(0)))
        .duration_ms(dur_ms) // periodic_irq uses the scenario duration as its bound
        .periodic_irq(
            CpuId(1),
            IrqType::HardIrq,
            period_ns,
            period_ns,
            50_000,
            &[],
        )
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "exit");

    let expected: Vec<u64> = (1..=(dur_ms * 1_000_000 / period_ns))
        .map(|k| k * period_ns)
        .collect();
    assert_eq!(
        irq_start_times_on(&trace, CpuId(1)),
        expected,
        "periodic IRQ did not fire on its exact 10ms grid"
    );
}

/// Item 3: two periodic timers with *different* periods (10ms and 25ms) on two
/// idle CPUs each keep their own cadence, independently — neither perturbs the
/// other's schedule.
#[test]
fn multiple_periodic_timers_distinct_periods() {
    let _lock = common::setup_test();
    let fast = 10_000_000u64; // 10ms
    let slow = 25_000_000u64; // 25ms
    let dur_ms = 100u64;

    let scenario = Scenario::builder()
        .cpus(3)
        .instant_timing()
        .task(pinned_hog(1, CpuId(0)))
        .duration_ms(dur_ms)
        .periodic_irq(CpuId(1), IrqType::HardIrq, fast, fast, 50_000, &[])
        .periodic_irq(CpuId(2), IrqType::SoftIrq, slow, slow, 50_000, &[])
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "exit");

    let grid = |p: u64| -> Vec<u64> { (1..=(dur_ms * 1_000_000 / p)).map(|k| k * p).collect() };
    assert_eq!(
        irq_start_times_on(&trace, CpuId(1)),
        grid(fast),
        "fast (10ms) timer grid wrong"
    );
    assert_eq!(
        irq_start_times_on(&trace, CpuId(2)),
        grid(slow),
        "slow (25ms) timer grid wrong"
    );
}

/// Item 4: taking a CPU offline *cancels* its tick chain (no ticks recorded
/// while offline) and bringing it back online *reschedules* it (ticks resume).
/// The offline window is [30ms, 70ms); we assert ticks before 30ms, none
/// strictly inside the window, and ticks after 70ms. CPU0 (never hotplugged)
/// is the control and keeps ticking throughout.
#[test]
fn tick_timer_cancelled_offline_and_restarted_online() {
    let _lock = common::setup_test();
    let off_at = 30_000_000u64;
    let on_at = 70_000_000u64;

    // Hotplug handling is engine-driven; sweep the schedulers that go through
    // the cpu_online/offline structops cleanly.
    let cases: [(&str, SchedFactory); 2] = [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
    ];
    for (name, make) in cases {
        let mut b = Scenario::builder().cpus(2).instant_timing();
        for pid in 1..=4 {
            b = b.task(hog(pid));
        }
        let scenario = b
            .cpu_offline_at(CpuId(1), off_at)
            .cpu_online_at(CpuId(1), on_at)
            .duration_ms(100)
            .build();

        let trace = Simulator::new(make(2)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        let times = tick_times_on(&trace, CpuId(1));
        assert!(
            times.iter().any(|&t| t < off_at),
            "{name}: CPU1 had no ticks before it went offline"
        );
        let inside: Vec<u64> = times
            .iter()
            .copied()
            .filter(|&t| t > off_at && t < on_at)
            .collect();
        assert!(
            inside.is_empty(),
            "{name}: CPU1 kept ticking while offline (timer not cancelled): {inside:?}"
        );
        assert!(
            times.iter().any(|&t| t > on_at),
            "{name}: CPU1 tick chain did not restart after coming back online"
        );
        // Control CPU never stops ticking.
        assert!(
            tick_times_on(&trace, CpuId(0)).iter().any(|&t| t > on_at),
            "{name}: control CPU0 stopped ticking"
        );
    }
}

/// Item 4: a periodic sleeper's wake timer is cancelled and re-armed every
/// cycle. With Run(2ms)+Sleep(6ms) on an otherwise-idle CPU, the task must wake
/// on a stable 8ms cadence for the whole run — proof the sleep→wake timer
/// reschedules cleanly, without drift or starvation. Swept across schedulers.
#[test]
fn periodic_sleeper_wake_timer_reschedules_at_stable_cadence() {
    let _lock = common::setup_test();
    let run_ns = 2_000_000u64;
    let sleep_ns = 6_000_000u64;
    let cycle = run_ns + sleep_ns; // 8ms

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(1)
            .instant_timing()
            .add_task(
                "sleeper",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
                    repeat: RepeatMode::Forever,
                },
            )
            .duration_ms(96)
            .build();

        let trace = Simulator::new(make(1)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        let wakes = wake_times_of(&trace, Pid(1));
        assert!(
            wakes.len() >= 9,
            "{name}: sleeper woke only {} times in 96ms (~11-12 expected)",
            wakes.len()
        );
        // Consecutive wakes are one cycle apart. instant_timing makes the run
        // phase exact; allow a small tolerance for dispatch bookkeeping.
        for w in wakes.windows(2) {
            let gap = w[1] - w[0];
            assert!(
                gap.abs_diff(cycle) <= 500_000,
                "{name}: wake cadence drifted: gap {gap}ns vs cycle {cycle}ns"
            );
        }
    }
}
