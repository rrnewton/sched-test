//! Tests for task run-delay (scheduling-latency) and runnable-time tracking
//! accuracy.
//!
//! The simulator's `TraceStats::from_trace()` computes, per task, the
//! **run delay** = the wall-clock time from when a task is placed on a runqueue
//! (`EnqueueTask`) to when it next starts running (`TaskScheduled`). This is the
//! `TaskStats::sched_latencies` distribution (sorted, with
//! `sched_latency_pctl()` percentile access) — the simulator's model of the
//! kernel's `se.statistics.wait_sum` / `sched:sched_stat_wait` run-delay signal.
//!
//! These tests validate that metric against the raw trace and across load
//! levels:
//!   1. run delay matches an independent recomputation from raw trace events;
//!   2. run-delay samples accumulate across preemption / re-enqueue cycles;
//!   3. a measured run delay reflects *actual* waiting (its enqueue→run window
//!      overlaps another task running on the same CPU);
//!   4. run delay grows with load (1 task on 4 CPUs vs 100 tasks on 4 CPUs).
//!
//! A task that idle-direct-dispatches (an idle CPU is found in `select_cpu`)
//! never goes through the enqueue path, so it records no run-delay sample — that
//! is the correct "no queueing delay" outcome and is accounted for below.
//!
//! All assertions are on observables (trace events and the derived `TraceStats`)
//! — no scheduler-side changes, per the No-Stub / "model the kernel, not the
//! scheduler" rules in `scx-sim/CLAUDE.md`.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A named scheduler factory (`simple` ignores the CPU count).
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

const SCHEDS: &[NamedSched] = &[
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", |n| DynamicScheduler::lavd(n)),
    ("cosmos", |n| DynamicScheduler::cosmos(n)),
];

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Independently recompute a task's run delays directly from the raw trace,
/// mirroring the exact rule `TraceStats` uses: each `EnqueueTask{pid}` starts (or
/// refreshes) a pending timer, and the next `TaskScheduled{pid}` closes it,
/// yielding one `scheduled - enqueued` sample. A `TaskScheduled` with no pending
/// enqueue (idle direct-dispatch) yields no sample. Returned sorted ascending.
fn recompute_run_delays(trace: &Trace, pid: Pid) -> Vec<TimeNs> {
    let mut pending: Option<TimeNs> = None;
    let mut out: Vec<TimeNs> = Vec::new();
    for e in trace.events() {
        match &e.kind {
            TraceKind::EnqueueTask { pid: p, .. } if *p == pid => {
                pending = Some(e.time_ns);
            }
            TraceKind::TaskScheduled { pid: p } if *p == pid => {
                if let Some(enq) = pending.take() {
                    out.push(e.time_ns.saturating_sub(enq));
                }
            }
            _ => {}
        }
    }
    out.sort_unstable();
    out
}

/// All run-delay samples across every task in the trace (unsorted).
fn all_run_delays(stats: &TraceStats) -> Vec<TimeNs> {
    stats
        .tasks
        .values()
        .flat_map(|t| t.sched_latencies.iter().copied())
        .collect()
}

fn mean_ns(v: &[TimeNs]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<TimeNs>() as f64 / v.len() as f64
    }
}

/// True if some `EnqueueTask{pid}` → next `TaskScheduled{pid}` window (a real,
/// positive-length run delay) contains a `TaskScheduled` of a *different* task on
/// the same CPU — i.e. the delay was spent waiting while another task ran there.
fn run_delay_overlaps_other_run(trace: &Trace, pid: Pid) -> bool {
    let events = trace.events();
    let mut pending: Option<(TimeNs, CpuId)> = None;
    for e in events {
        match &e.kind {
            TraceKind::EnqueueTask { pid: p, .. } if *p == pid => {
                pending = Some((e.time_ns, e.cpu));
            }
            TraceKind::TaskScheduled { pid: p } if *p == pid => {
                if let Some((enq_t, _enq_cpu)) = pending.take() {
                    let sched_t = e.time_ns;
                    if sched_t <= enq_t {
                        continue;
                    }
                    // The task landed on CPU `e.cpu`; check whether another task
                    // ran on that CPU during the wait window (enq_t, sched_t).
                    let overlapped = events.iter().any(|o| {
                        o.cpu == e.cpu
                            && o.time_ns > enq_t
                            && o.time_ns < sched_t
                            && matches!(o.kind, TraceKind::TaskScheduled { pid: q } if q != pid)
                    });
                    if overlapped {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

// ===========================================================================
// 1. run_delay is tracked correctly and is accessible from the trace.
// ===========================================================================

/// Under contention (more tasks than CPUs), `TraceStats` must compute each
/// task's run delay exactly as an independent pass over the raw trace does. This
/// validates both that the metric is correct and that it is derivable from —
/// i.e. accessible in — the trace output.
#[test]
fn test_run_delay_matches_raw_trace() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    const NR_TASKS: i32 = 6;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS).instant_timing();
        for _ in 0..NR_TASKS {
            builder = builder.add_task("hog", 0, forever_run(5_000_000));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let stats = TraceStats::from_trace(&trace);

        // At least some run delays must have been captured (contention forces the
        // enqueue path).
        let total_samples: usize = stats.tasks.values().map(|t| t.sched_latencies.len()).sum();
        assert!(
            total_samples > 0,
            "[{name}] no run-delay samples captured under contention"
        );

        for pid in 1..=NR_TASKS {
            let from_stats = &stats.tasks[&Pid(pid)].sched_latencies;
            let recomputed = recompute_run_delays(&trace, Pid(pid));
            assert_eq!(
                from_stats, &recomputed,
                "[{name}] pid={pid} run-delay samples from TraceStats disagree with raw trace"
            );
            // Run delays are non-negative by construction; percentiles are ordered.
            let ts = &stats.tasks[&Pid(pid)];
            if !ts.sched_latencies.is_empty() {
                assert!(
                    ts.sched_latency_pctl(0.50) <= ts.sched_latency_pctl(0.99),
                    "[{name}] pid={pid} p50 must be <= p99"
                );
            }
        }
    }
}

// ===========================================================================
// 2. Run-delay samples accumulate across preemption / re-enqueue cycles.
// ===========================================================================

/// A CPU-bound task contending on a single CPU is repeatedly taken off-CPU
/// (preempted or slice-expiry re-enqueued) and must re-queue each time. Every
/// re-enqueue→run cycle adds a run-delay sample, so the sample count accumulates
/// well beyond one and tracks the task's off-CPU cycles.
#[test]
fn test_run_delay_accumulates_across_preemptions() {
    let _lock = common::setup_test();
    const NR_TASKS: i32 = 3;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(1).instant_timing();
        for _ in 0..NR_TASKS {
            builder = builder.add_task("hog", 0, forever_run(10_000_000));
        }
        let scenario = builder.duration_ms(400).build();

        let trace = Simulator::new(make(1)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let stats = TraceStats::from_trace(&trace);

        for pid in 1..=NR_TASKS {
            let ts = &stats.tasks[&Pid(pid)];
            // Accumulation: many off-CPU/re-enqueue cycles → many run-delay samples.
            assert!(
                ts.sched_latencies.len() >= 3,
                "[{name}] pid={pid} run delay did not accumulate across cycles: {} samples",
                ts.sched_latencies.len()
            );
            // The task really was cycling off the CPU (preempted and/or yielded).
            assert!(
                ts.preempt_count + ts.yield_count > 0,
                "[{name}] pid={pid} never left the CPU involuntarily, yet re-queued"
            );
            // Each re-enqueue precedes a schedule, so samples cannot exceed the
            // number of times the task was scheduled.
            assert!(
                ts.sched_latencies.len() <= ts.schedule_count,
                "[{name}] pid={pid} more run-delay samples ({}) than schedules ({})",
                ts.sched_latencies.len(),
                ts.schedule_count
            );
        }
    }
}

// ===========================================================================
// 3. A measured run delay reflects actual waiting for the CPU.
// ===========================================================================

/// A run delay must correspond to real contention, not a bookkeeping artifact:
/// for a task under single-CPU contention, at least one of its enqueue→run
/// windows must overlap another task actually running on that CPU. That is the
/// definition of scheduling latency — time spent runnable while the CPU serves
/// someone else.
#[test]
fn test_run_delay_reflects_actual_waiting() {
    let _lock = common::setup_test();

    for (name, make) in SCHEDS {
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task("a", 0, forever_run(10_000_000))
            .add_task("b", 0, forever_run(10_000_000))
            .add_task("c", 0, forever_run(10_000_000))
            .duration_ms(300)
            .build();

        let trace = Simulator::new(make(1)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let stats = TraceStats::from_trace(&trace);

        // Some task must have accrued a strictly-positive run delay.
        let max_delay = stats
            .tasks
            .values()
            .flat_map(|t| t.sched_latencies.iter().copied())
            .max()
            .unwrap_or(0);
        assert!(
            max_delay > 0,
            "[{name}] no positive run delay under 3-way single-CPU contention"
        );

        // And that delay reflects a real wait: some task's enqueue→run window
        // overlapped another task running on the same CPU.
        let any_real_wait = (1..=3).any(|p| run_delay_overlaps_other_run(&trace, Pid(p)));
        assert!(
            any_real_wait,
            "[{name}] run delays never overlapped another task running — not real waiting"
        );
    }
}

// ===========================================================================
// 4. Run delay grows with load (1 task / 4 CPUs  vs  100 tasks / 4 CPUs).
// ===========================================================================

/// The run-delay metric must reflect scheduling pressure: on an under-loaded box
/// (1 task, 4 CPUs) queueing delay is negligible, while heavy over-subscription
/// (100 tasks, 4 CPUs) produces both far more run-delay samples and — for the
/// schedulers that model queueing wait in this metric — far larger delays.
///
/// Scheduler note: `simple` and `lavd` enqueue a task once and leave it queued
/// while others run, so their `EnqueueTask→TaskScheduled` run delay captures the
/// real wait (tens of ms under this load). `cosmos`, under multi-CPU
/// pull-dispatch, effectively (re-)enqueues each task at the instant it is
/// dispatched, so its enqueue timestamp coincides with the schedule timestamp
/// and this particular metric reads ~0 even under heavy load (it still produces
/// many *samples*, just near-zero-valued). We therefore assert the strong
/// magnitude/tail growth only for the queueing-exposing schedulers, and assert
/// the sample-count growth (scheduling pressure) for all three. See task notes
/// for test-runnable-rundelay-tracking. (cosmos *does* expose positive run delay
/// when tasks genuinely serialize on one CPU — see
/// `test_run_delay_reflects_actual_waiting`.)
#[test]
fn test_run_delay_scales_with_load() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    fn run_load(make: fn(u32) -> DynamicScheduler, nr_cpus: u32, n_tasks: i32) -> TraceStats {
        let mut builder = Scenario::builder().cpus(nr_cpus).instant_timing();
        for _ in 0..n_tasks {
            builder = builder.add_task("t", 0, forever_run(3_000_000));
        }
        let scenario = builder.duration_ms(100).build();
        let trace = Simulator::new(make(nr_cpus)).run(scenario);
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "load run should exit normally"
        );
        TraceStats::from_trace(&trace)
    }

    /// Schedulers whose `EnqueueTask→TaskScheduled` run delay captures queueing
    /// wait (enqueue-once-then-wait), so their run-delay *magnitude* grows with
    /// load. `cosmos` enqueues at dispatch time and is excluded from the
    /// magnitude assertions (see the doc comment above).
    fn exposes_queueing_run_delay(name: &str) -> bool {
        matches!(name, "simple" | "lavd")
    }

    for (name, make) in SCHEDS {
        let low = run_load(*make, NR_CPUS, 1);
        let high = run_load(*make, NR_CPUS, 100);

        let low_delays = all_run_delays(&low);
        let high_delays = all_run_delays(&high);
        let low_mean = mean_ns(&low_delays);
        let high_mean = mean_ns(&high_delays);
        eprintln!(
            "[{name}] run-delay: low(1 task) n={} mean={low_mean:.0}ns | high(100 tasks) n={} mean={high_mean:.0}ns",
            low_delays.len(),
            high_delays.len()
        );

        // Scheduling pressure — true for every scheduler: heavy over-subscription
        // produces strictly more run-delay samples than the under-loaded box.
        assert!(
            high_delays.len() > low_delays.len(),
            "[{name}] expected more run-delay samples under heavy load: low={}, high={}",
            low_delays.len(),
            high_delays.len()
        );

        if exposes_queueing_run_delay(name) {
            // Under-loaded: negligible queueing delay (an idle CPU is ~always
            // free). Heavily loaded: a task waits behind ~24 others per CPU, so
            // mean and tail run delay must both blow up past 1ms.
            assert!(
                high_mean > low_mean,
                "[{name}] heavy-load mean run delay ({high_mean:.0}ns) not greater than \
                 light-load ({low_mean:.0}ns)"
            );
            assert!(
                high_mean > 1_000_000.0,
                "[{name}] heavy-load mean run delay implausibly small: {high_mean:.0}ns"
            );
            let high_p99 = {
                let mut v = high_delays.clone();
                v.sort_unstable();
                percentile(&v, 0.99)
            };
            assert!(
                high_p99 > 1_000_000,
                "[{name}] heavy-load p99 run delay implausibly small: {high_p99}ns"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The kernel-overhead path: re-dispatch must be charged, not free
// ---------------------------------------------------------------------------

/// A preempted task's re-dispatch must be charged the modelled kernel cost,
/// not treated as free.
///
/// # The defect this pins
///
/// `enqueued_at_ns` gates the wakeup-latency floor in `start_running` (a 3us
/// log-normal with a 3.5% Pareto tail, modelling IPI, context switch and cache
/// warming). It used to be written in exactly ONE place — `handle_task_wake` —
/// so the floor applied to WAKEUPS ONLY. Every preempted or slice-expired task
/// went back through `stop_and_reenqueue`, whose trace callbacks receive
/// `SimulatorState` but not the task table, so the stamp could not be made
/// there. Those tasks were re-dispatched charged only the fixed dispatch
/// overheads: a flat 250ns with ZERO variance.
///
/// On CPU-bound spinners that never sleep that is 99.8% of dispatches, and it
/// made the simulator's scheduling delay 25-58x below a live guest's
/// `sched_info.run_delay` on the same scenario. The kernel charges this path:
/// `sched_info_enqueue` restamps `last_queued` on every enqueue, including the
/// re-enqueue of a preempted task.
///
/// # What is asserted, and what is deliberately NOT
///
/// The fix stamps the shared `stop_and_reenqueue` spine, which is necessary but
/// NOT sufficient: measured on this scenario, 34-36% of `simple` and `cosmos`
/// re-dispatches and ~80% of `lavd`'s still come in under 1us, so further
/// re-dispatch paths remain uncharged and are scheduler-dependent. This test
/// therefore pins the mechanism, not a calibrated magnitude:
///
///   * SPREAD, for every scheduler — the old behaviour produced ONE distinct
///     value, and a constant cannot have a spread. This fails on a regression
///     to any fixed cost, whatever its size.
///   * MAGNITUDE, only for `simple` — the scheduler the calibration fixture
///     uses, and the one whose gap to the live guest was measured. Asserting a
///     magnitude for `lavd` today would be asserting the residual defect.
///
/// The residual is a follow-up, and it is a finding rather than a caveat: it
/// means scheduling-delay figures remain understated, most for `lavd`.
#[test]
fn preemption_redispatch_is_charged_the_modelled_kernel_cost() {
    // Two spinners on two CPUs: no contention, so every sample is pure
    // per-dispatch cost with no queueing mixed in. That isolation is the point
    // — under contention real queueing would mask the defect entirely.
    let mut builder = Scenario::builder().cpus(2);
    for i in 0..2 {
        // ONE phase, longer than the run: see the note above. Anything that
        // repeats manufactures phase-boundary yields and measures those instead.
        builder = builder.add_task(&format!("spin{i}"), 0, forever_run(10_000_000_000));
    }
    // 4s, not less. The default slice is 20ms, but LAVD preempts far less often
    // than the others — 38 preemptions where `simple` has 198 on the same
    // scenario — so the run has to be long enough for LAVD to clear the sample
    // floor below, not just for `simple` to.
    let scenario = builder.duration_ms(4_000).build();

    for (name, make) in SCHEDS {
        let _lock = common::setup_test();
        let trace = Simulator::new(make(2)).run(scenario.clone());
        let stats = TraceStats::from_trace(&trace);

        for pid in scenario.tasks.iter().map(|t| t.pid) {
            let lat = match stats.tasks.get(&pid) {
                Some(t) if t.sched_latencies.len() >= 20 => t.sched_latencies.clone(),
                other => panic!(
                    "{name}: pid {pid:?} produced {} run-delay samples; this test needs \
                     a preemption-driven workload to say anything",
                    other.map_or(0, |t| t.sched_latencies.len())
                ),
            };

            let mut distinct = lat.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert!(
                distinct.len() > 1,
                "{name}: pid {pid:?} re-dispatch delay took ONE distinct value \
                 ({} ns) across {} episodes. That is the signature of a fixed \
                 dispatch overhead being charged instead of the modelled kernel \
                 path — the wakeup-latency floor is not reaching re-enqueue.",
                distinct[0],
                lat.len()
            );

            let mean = lat.iter().sum::<u64>() as f64 / lat.len() as f64;
            assert!(
                mean > 1_000.0,
                "{name}: pid {pid:?} mean re-dispatch delay {mean:.0} ns is below \
                 1us. The modelled floor is 3us before its heavy tail; anything \
                 at the few-hundred-ns scale means only the fixed dispatch \
                 overheads were charged."
            );
        }
    }
}

/// A task re-dispatched after an explicit `sched_yield()` must be charged the
/// modelled kernel cost too.
///
/// # Why this needs its own test
///
/// `preemption_redispatch_is_charged_the_modelled_kernel_cost` above uses one
/// continuous run phase, deliberately, so that it measures the preemption path
/// and not phase-boundary artifacts. That makes it blind to this path: removing
/// the stamp from `handle_task_phase_complete` leaves it green. Verified by
/// doing exactly that.
///
/// # Why charging here is modelling the kernel, not papering over a gap
///
/// `Phase::Yield` is a real `sched_yield()`. The task stays runnable, the kernel
/// re-enqueues it, and `sched_info_enqueue` restamps `last_queued` — so the next
/// dispatch accrues `run_delay` exactly as a preemption's does. A yielding task
/// getting back onto the CPU for free is the same defect as a preempted one
/// doing so.
///
/// Note the separate question this does NOT address: a `Phase::Run` followed by
/// another `Phase::Run` also routes through `handle_task_phase_complete` and is
/// emitted as a yield, for a task that never stopped running. That is a
/// workload-scripting artifact — the anti-pattern `676b42f` fixed on the
/// lowering side — and whether the engine should coalesce consecutive run
/// phases is a modelling decision, not a stamping one.
#[test]
fn yield_redispatch_is_charged_the_modelled_kernel_cost() {
    // A yielder ALONE on its CPU. Not sharing with a hog: a competitor would
    // make every post-yield wait a full 20ms slice of real queueing, which
    // swamps the few-microsecond overhead this test is about and passes whether
    // or not the path is stamped. Verified — that is what the first version of
    // this test did, and it stayed green with the fix reverted.
    for (name, make) in SCHEDS {
        let _lock = common::setup_test();
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task(
                "yielder",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000), Phase::Yield],
                    repeat: RepeatMode::Count(40),
                },
            )
            .duration_ms(4_000)
            .build();

        let trace = Simulator::new(make(1)).run(scenario.clone());
        let stats = TraceStats::from_trace(&trace);
        let pid = scenario.tasks[0].pid;

        let lat = match stats.tasks.get(&pid) {
            Some(t) if t.sched_latencies.len() >= 20 => t.sched_latencies.clone(),
            other => panic!(
                "{name}: yielder produced {} run-delay samples; this test needs the \
                 yield path to actually be exercised to say anything",
                other.map_or(0, |t| t.sched_latencies.len())
            ),
        };

        let mean = lat.iter().sum::<u64>() as f64 / lat.len() as f64;
        assert!(
            mean > 1_000.0,
            "{name}: mean post-yield re-dispatch delay {mean:.0} ns is below 1us \
             across {} samples. The yield path in `handle_task_phase_complete` is \
             not stamping `enqueued_at_ns`, so the modelled kernel cost is being \
             skipped and only the fixed dispatch overheads charged.",
            lat.len()
        );
    }
}

/// A `Run` -> `Run` phase boundary must NOT be charged the wakeup-latency floor.
///
/// # This guards a decision, not a bug
///
/// The other two tests in this group assert that re-dispatch IS charged. This
/// one asserts the single case where it deliberately is not, and it exists
/// because a decision nobody can see is indistinguishable from an oversight —
/// four genuinely-uncharged paths were found and fixed, and without this test
/// the natural next move is to "fix" this one too and turn every test green
/// while making the simulator wrong.
///
/// The reasoning, in short (the long form is at the stamp site in
/// `handle_task_phase_complete`): `wakeup_latency_floor_ns` models the cost of
/// getting a task ONTO a cpu, and a task crossing a Run -> Run boundary never
/// left one. The boundary is an artifact of how the workload was scripted, not
/// a kernel event. Charging it was measured and made no difference to the
/// live-guest match, so the principled model decides.
///
/// An explicit `Phase::Yield` IS charged — see
/// `yield_redispatch_is_charged_the_modelled_kernel_cost`. The pair of tests is
/// what makes the distinction visible.
#[test]
fn run_to_run_phase_boundary_is_deliberately_not_charged() {
    // One task alone on one cpu, in chunks. Alone, so nothing can preempt it and
    // every sample is a phase boundary; chunked, so there are boundaries at all.
    for (name, make) in SCHEDS {
        let _lock = common::setup_test();
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task(
                "chunked",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000)],
                    repeat: RepeatMode::Count(60),
                },
            )
            .duration_ms(4_000)
            .build();

        let trace = Simulator::new(make(1)).run(scenario.clone());
        let stats = TraceStats::from_trace(&trace);
        let pid = scenario.tasks[0].pid;

        let lat = match stats.tasks.get(&pid) {
            Some(t) if t.sched_latencies.len() >= 20 => t.sched_latencies.clone(),
            other => panic!(
                "{name}: produced {} run-delay samples; this test needs the Run -> Run \
                 boundary to actually be exercised to say anything",
                other.map_or(0, |t| t.sched_latencies.len())
            ),
        };

        let mean = lat.iter().sum::<u64>() as f64 / lat.len() as f64;
        assert!(
            mean < 1_000.0,
            "{name}: mean Run -> Run re-dispatch delay is {mean:.0} ns across {} \
             samples, i.e. the wakeup-latency floor is now being applied here. \
             That is a DELIBERATE non-charge, not a missed path — a task crossing \
             a phase boundary never left the cpu, so it must not pay the cost of \
             getting onto one. Read the comment at the stamp site in \
             `handle_task_phase_complete` before changing this.",
            lat.len()
        );
    }
}
