//! Property-based scheduling-invariant tests (tg test-core-scheduling-invariants).
//!
//! Rather than asserting a specific schedule, these tests assert *invariants*
//! that must hold for ANY correct scheduler under ANY workload, then hammer them
//! with many randomized workloads and seeds. The invariants:
//!
//! 1. No task runs on two CPUs at once (per-task on-CPU intervals are disjoint).
//! 2. A CPU runs one task at a time (per-CPU on-CPU intervals are disjoint).
//! 3. No starvation (no watchdog stall; and, over a long run, every task runs).
//! 4. Time never runs backward (interval end >= start; per-CPU ticks ordered).
//! 5. Bounded CPU utilization: sum of on-CPU time <= nr_cpus * span. Catches
//!    "phantom"/over-charged CPU time (e.g. the V4-A idle-CPU over-charge class).
//!
//! On-CPU intervals are reconstructed from the trace exactly as `total_runtime`
//! does: `TaskScheduled{pid}` (with `.cpu`) opens an interval; the next
//! `TaskPreempted/TaskYielded/TaskSlept/TaskCompleted/SimulationEnd` for that pid
//! closes it. Overlap checks are done on interval *time ranges* (not recording
//! order), so they are immune to the trace's startup-preamble ordering quirks.

use scx_simulator::*;
use std::collections::HashMap;

#[macro_use]
mod common;

type SchedFactory = fn(u32) -> DynamicScheduler;

fn schedulers() -> [(&'static str, SchedFactory); 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

/// A tiny deterministic PRNG (SplitMix64-style) so each property iteration is
/// fully reproducible from its seed — no external `rand` dependency, and no
/// reliance on wall-clock entropy.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `[lo, hi]` inclusive.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(hi >= lo);
        lo + self.next_u64() % (hi - lo + 1)
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next_u64() % den < num
    }
}

/// A reconstructed on-CPU execution interval: `pid` ran on `cpu` during
/// `[start, end)` (nanoseconds).
#[derive(Clone, Copy, Debug)]
struct Interval {
    pid: Pid,
    cpu: CpuId,
    start: u64,
    end: u64,
}

/// Reconstruct on-CPU intervals from a trace, matching `total_runtime`'s
/// schedule-in / schedule-out event pairing.
fn reconstruct_intervals(trace: &Trace) -> Vec<Interval> {
    let mut intervals = Vec::new();
    // pid -> (cpu, start) for the currently-open interval, if any.
    let mut open: HashMap<Pid, (CpuId, u64)> = HashMap::new();

    let close =
        |intervals: &mut Vec<Interval>, open: &mut HashMap<Pid, (CpuId, u64)>, pid: Pid, t: u64| {
            if let Some((cpu, start)) = open.remove(&pid) {
                // Clamp against pathological ordering; a negative-length interval is
                // itself an invariant-4 violation caught by the caller.
                intervals.push(Interval {
                    pid,
                    cpu,
                    start,
                    end: t,
                });
            }
        };

    for e in trace.events() {
        match &e.kind {
            TraceKind::TaskScheduled { pid } => {
                // If somehow already open (double schedule-in without a stop),
                // close the prior one at this instant first so the pair is
                // well-formed; the overlap check will still flag any real issue.
                if open.contains_key(pid) {
                    close(&mut intervals, &mut open, *pid, e.time_ns);
                }
                open.insert(*pid, (e.cpu, e.time_ns));
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskYielded { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskCompleted { pid }
            | TraceKind::SimulationEnd { pid } => {
                close(&mut intervals, &mut open, *pid, e.time_ns);
            }
            _ => {}
        }
    }
    // Any still-open intervals: close at the last observed timestamp.
    if let Some(last) = trace.events().last().map(|e| e.time_ns) {
        let pids: Vec<Pid> = open.keys().copied().collect();
        for pid in pids {
            close(&mut intervals, &mut open, pid, last);
        }
    }
    intervals
}

/// Assert two sets of intervals (already filtered to one pid or one cpu) have no
/// overlapping time ranges. `[start, end)` half-open; touching is allowed.
fn assert_no_overlap(mut group: Vec<Interval>, ctx: &str, by: &str) {
    group.sort_by_key(|iv| (iv.start, iv.end));
    for w in group.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert!(
            a.end <= b.start,
            "{ctx}: overlapping on-CPU intervals by {by}: \
             pid={:?}/cpu={:?} [{},{}) overlaps pid={:?}/cpu={:?} [{},{})",
            a.pid,
            a.cpu,
            a.start,
            a.end,
            b.pid,
            b.cpu,
            b.start,
            b.end
        );
    }
}

/// Check the always-hold invariants for one completed run (1, 2, 4, 5, plus the
/// watchdog form of no-starvation). These hold for ANY workload regardless of
/// run length. The "every task eventually runs" form of no-starvation needs a
/// long-enough run and is asserted separately (see `no_starvation_over_long_run`).
fn check_invariants(trace: &Trace, nr_cpus: u32, span_ns: u64, ctx: &str) {
    // Invariant 3 (no starvation, watchdog form): the run must not end in an
    // engine/scheduler error — in particular `ErrorStall`, which the watchdog
    // raises when a runnable task is denied the CPU past the timeout.
    assert!(
        !trace.has_error(),
        "{ctx}: run ended with an error: {:?}",
        trace.exit_kind()
    );

    let intervals = reconstruct_intervals(trace);

    // Invariant 4a: no interval runs backward in time.
    for iv in &intervals {
        assert!(
            iv.end >= iv.start,
            "{ctx}: negative-length on-CPU interval pid={:?} cpu={:?} [{},{})",
            iv.pid,
            iv.cpu,
            iv.start,
            iv.end
        );
    }

    // Invariant 1: a task is never on two CPUs at once.
    let mut by_pid: HashMap<Pid, Vec<Interval>> = HashMap::new();
    for iv in &intervals {
        by_pid.entry(iv.pid).or_default().push(*iv);
    }
    for (_pid, group) in by_pid {
        assert_no_overlap(group, ctx, "task");
    }

    // Invariant 2: a CPU runs at most one task at a time.
    let mut by_cpu: HashMap<CpuId, Vec<Interval>> = HashMap::new();
    for iv in &intervals {
        by_cpu.entry(iv.cpu).or_default().push(*iv);
    }
    for (_cpu, group) in by_cpu {
        assert_no_overlap(group, ctx, "cpu");
    }

    // Invariant 4b: per-CPU timer time never runs backward. (Global cross-CPU
    // recording order is not time-sorted — near-simultaneous ticks on different
    // CPUs may be recorded in either order — so monotonicity is a *per-CPU*
    // property, which is the meaningful "a CPU's clock advances" invariant.)
    let mut tick_times: HashMap<CpuId, u64> = HashMap::new();
    for e in trace.events() {
        if matches!(e.kind, TraceKind::Tick { .. }) {
            let prev = tick_times.entry(e.cpu).or_insert(0);
            assert!(
                e.time_ns >= *prev,
                "{ctx}: cpu {:?} tick time went backward: {} < {}",
                e.cpu,
                e.time_ns,
                *prev
            );
            *prev = e.time_ns;
        }
    }

    // Invariant 5: bounded CPU utilization — total on-CPU time across all tasks
    // cannot exceed nr_cpus * span. A small tolerance covers tick/boundary
    // rounding; a real "phantom CPU time" bug (charging an idle CPU) blows past
    // this by a wide margin.
    let busy: u64 = intervals
        .iter()
        .map(|iv| iv.end.saturating_sub(iv.start))
        .sum();
    let capacity = nr_cpus as u64 * span_ns;
    let bound = capacity + capacity / 20 + 4_000_000; // +5% + one tick slack
    assert!(
        busy <= bound,
        "{ctx}: total on-CPU time {busy}ns exceeds capacity {capacity}ns (bound {bound}ns) \
         on {nr_cpus} CPUs over {span_ns}ns — phantom/over-charged CPU time",
        busy = busy
    );
}

/// Build a randomized-but-reproducible scenario from `rng`. Returns the built
/// scenario, the CPU count, the duration, and the task PIDs.
fn random_scenario(rng: &mut Rng) -> (Scenario, u32, u64, Vec<Pid>) {
    let nr_cpus = rng.range(1, 8) as u32;
    let nr_tasks = rng.range(1, 14) as u32;
    let duration_ms = rng.range(60, 120);
    let sim_seed = rng.next_u64() as u32;

    let mut b = Scenario::builder().cpus(nr_cpus).seed(sim_seed);
    let mut pids = Vec::new();
    for i in 0..nr_tasks {
        let pid = Pid(1 + i as i32);
        pids.push(pid);
        let nice = (rng.range(0, 9) as i16 - 4) as i8; // [-4, 5]

        // Mix of task shapes: pure CPU hog, or a run/sleep cycler.
        let phases = if rng.chance(1, 3) {
            // CPU-bound hog.
            vec![Phase::Run(rng.range(5_000_000, 40_000_000))]
        } else {
            // Interactive-ish cycler; sleep may be zero (yield-like).
            vec![
                Phase::Run(rng.range(500_000, 6_000_000)),
                Phase::Sleep(rng.range(0, 4_000_000)),
            ]
        };
        b = b.add_task(
            &format!("t{i}"),
            nice,
            TaskBehavior {
                phases,
                repeat: RepeatMode::Forever,
            },
        );
    }
    (
        b.duration_ms(duration_ms).build(),
        nr_cpus,
        duration_ms * 1_000_000,
        pids,
    )
}

/// The core property test: many random workloads × seeds × schedulers, each run
/// asserting every invariant. Reproducible: iteration `k` on scheduler `s`
/// always builds the same workload.
#[test]
fn scheduling_invariants_hold_under_random_workloads() {
    let _lock = common::setup_test();
    const ITERS: u64 = 24;

    for (name, make) in schedulers() {
        for k in 0..ITERS {
            // Distinct, reproducible RNG stream per (scheduler, iteration).
            let mut rng = Rng::new(
                0xC0FFEE ^ (k << 8) ^ (name.len() as u64) ^ (name.as_bytes()[0] as u64) << 16,
            );
            let (scenario, nr_cpus, span_ns, pids) = random_scenario(&mut rng);
            let ctx = format!("{name} iter={k} cpus={nr_cpus} tasks={}", pids.len());

            let trace = Simulator::new(make(nr_cpus)).run(scenario);
            check_invariants(&trace, nr_cpus, span_ns, &ctx);
        }
    }
}

/// Invariant 3 (no starvation), "eventually runs" form: with a modest load
/// (2 tasks per CPU) over a long run, EVERY task must get scheduled and accrue
/// runtime — no task is denied the CPU indefinitely. (Under heavy
/// oversubscription in a short window some tasks simply may not get a turn yet,
/// which is not starvation; hence the modest load + long duration here.)
#[test]
fn no_starvation_over_long_run() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let nr_tasks = nr_cpus * 2;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(7);
        let mut pids = Vec::new();
        for i in 0..nr_tasks {
            let pid = Pid(1 + i as i32);
            pids.push(pid);
            b = b.add_task(
                &format!("t{i}"),
                (i % 5) as i8 - 2,
                TaskBehavior {
                    phases: vec![Phase::Run(3_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let span_ms = 1_000u64;
        let trace = Simulator::new(make(nr_cpus)).run(b.duration_ms(span_ms).build());

        // Baseline invariants still hold on this run.
        check_invariants(
            &trace,
            nr_cpus,
            span_ms * 1_000_000,
            &format!("{name} long-run"),
        );

        // And every task made progress.
        for &pid in &pids {
            assert!(
                trace.schedule_count(pid) > 0 && trace.total_runtime(pid) > 0,
                "{name}: task {pid:?} starved over a {span_ms}ms run \
                 (scheduled {} times, {}ns runtime)",
                trace.schedule_count(pid),
                trace.total_runtime(pid)
            );
        }
    }
}

/// Targeted stress: heavy oversubscription (many tasks, few CPUs). The
/// mutual-exclusion and utilization-bound invariants are most likely to break
/// when the run queues are deep and migrations are frequent.
#[test]
fn invariants_under_heavy_oversubscription() {
    let _lock = common::setup_test();
    let nr_cpus = 2u32;
    let nr_tasks = 32u32;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(4242);
        for i in 0..nr_tasks {
            b = b.add_task(
                &format!("hog{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(20_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let span_ns = 200 * 1_000_000;
        let trace = Simulator::new(make(nr_cpus)).run(b.duration_ms(200).build());
        check_invariants(&trace, nr_cpus, span_ns, &format!("{name} oversub"));
    }
}
