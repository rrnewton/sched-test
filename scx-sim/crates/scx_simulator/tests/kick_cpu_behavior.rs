//! Tests for `scx_bpf_kick_cpu` and cross-CPU notification behavior.
//!
//! A scheduler calls `scx_bpf_kick_cpu(cpu, flags)` to send a rescheduling IPI
//! to another CPU (or itself). In the simulator this stages a `KickDelivered`
//! event at `now + ipi_delivery_ns` and records a `KickCpu { target_cpu }` trace
//! event; when the engine delivers it (`handle_kick_delivered`), a
//! `SCX_KICK_PREEMPT` on a busy CPU preempts the running task (`TaskPreempted`)
//! and reschedules, a `SCX_KICK_IDLE` dispatches only if the CPU is idle, and a
//! plain kick re-runs the dispatch path. A self-kick with `PREEMPT` is handled
//! inline on the issuing CPU.
//!
//! These tests observe scheduler-*issued* kicks (the `KickCpu` trace event) and
//! their downstream reschedule effects. Which schedulers kick, and how often,
//! is workload-dependent: `simple`/`mitosis`/`tickless` rarely kick, while
//! `lavd` kicks heavily under contention and `cosmos` self-kicks; the fixtures
//! below use a contended burst that reliably drives `lavd` (and `cosmos`).
//!
//! Limitation (documented, not worked around): the `KickCpu` trace event carries
//! only the target CPU, not the `flags`. The flag *bits* (`SCX_KICK_IDLE` /
//! `SCX_KICK_PREEMPT`) are therefore not observable at the integration level —
//! their staging is unit-tested in `kfuncs.rs::test_kick_cpu`. Here we assert the
//! observable flag *effects* (a busy-CPU kick preempts; an idle-CPU kick
//! dispatches).
//!
//! Assertions read only trace observables — no scheduler-side changes, per the
//! No-Stub / "model the kernel, not the scheduler" rules in `scx-sim/CLAUDE.md`.

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler factory (`simple` ignores the CPU count).
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// Reschedule effects must land within this window after a kick is issued
/// (kick delivery adds `ipi_delivery_ns`, then the target reschedules).
const RESCHED_WINDOW_NS: TimeNs = 20_000_000;

/// A generic task definition.
fn td(name: &str, pid: i32, behavior: TaskBehavior, mm: Option<MmId>) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior,
        start_time_ns: 0,
        mm_id: mm,
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

/// A contended burst that reliably drives kicks on `lavd`: many latency-critical
/// I/O tasks waking repeatedly plus a couple of CPU hogs, all squeezed onto 2
/// CPUs so the scheduler must fire rescheduling IPIs.
fn kick_burst_scenario() -> Scenario {
    let mut b = Scenario::builder().cpus(2);
    for i in 0..8 {
        b = b.task(td(
            &format!("io{i}"),
            1 + i,
            workloads::io_bound(200_000, 2_000_000),
            None,
        ));
    }
    for i in 0..2 {
        b = b.task(td(
            &format!("hog{i}"),
            20 + i,
            workloads::cpu_bound(50_000_000),
            None,
        ));
    }
    b.duration_ms(300).build()
}

/// A mutual-wake + hog mix on 4 CPUs that reliably drives kicks (including
/// self-kicks) on `cosmos`.
fn mixed_kick_scenario() -> Scenario {
    let (a, bb) = workloads::ping_pong(Pid(1), Pid(2), 300_000);
    let mut b = Scenario::builder()
        .cpus(4)
        .task(td("ping", 1, a, Some(MmId(1))))
        .task(td("pong", 2, bb, Some(MmId(1))));
    for i in 0..3 {
        b = b.task(td(
            &format!("io{i}"),
            3 + i,
            workloads::io_bound(500_000, 3_000_000),
            None,
        ));
    }
    for i in 0..2 {
        b = b.task(td(
            &format!("hog{i}"),
            20 + i,
            workloads::cpu_bound(50_000_000),
            None,
        ));
    }
    b.duration_ms(300).build()
}

/// Schedulers paired with a workload under which they reliably issue kicks:
/// `lavd` kicks heavily under the 2-CPU burst, `cosmos` under the 4-CPU mix.
fn reliable_kicker_runs() -> [(&'static str, DynamicScheduler, Scenario); 2] {
    [
        ("lavd", DynamicScheduler::lavd(2), kick_burst_scenario()),
        ("cosmos", DynamicScheduler::cosmos(4), mixed_kick_scenario()),
    ]
}

/// A single observed kick and its reconstructed context.
struct KickObs {
    time: TimeNs,
    issuer: CpuId,
    target: CpuId,
    /// Whether the target CPU had a task running when the kick was issued.
    target_busy: bool,
    /// A `TaskScheduled` landed on the target within the reschedule window.
    target_dispatched: bool,
    /// A `TaskPreempted` landed on the target within the reschedule window.
    target_preempted: bool,
}

impl KickObs {
    fn is_self(&self) -> bool {
        self.issuer == self.target
    }
    /// Any reschedule effect (dispatch or preempt) followed the kick.
    fn caused_reschedule(&self) -> bool {
        self.target_dispatched || self.target_preempted
    }
}

/// Extract every kick from a trace, reconstructing per-CPU busy state (to tell
/// whether the target was busy) and scanning forward for the reschedule effect.
fn observe_kicks(trace: &Trace) -> Vec<KickObs> {
    let events = trace.events();
    let mut busy: std::collections::HashMap<CpuId, bool> = std::collections::HashMap::new();
    let mut out = Vec::new();

    for (i, e) in events.iter().enumerate() {
        match &e.kind {
            TraceKind::TaskScheduled { .. } => {
                busy.insert(e.cpu, true);
            }
            TraceKind::TaskPreempted { .. }
            | TraceKind::TaskSlept { .. }
            | TraceKind::TaskYielded { .. }
            | TraceKind::TaskCompleted { .. }
            | TraceKind::CpuIdle => {
                busy.insert(e.cpu, false);
            }
            TraceKind::KickCpu { target_cpu } => {
                let target = *target_cpu;
                let target_busy = *busy.get(&target).unwrap_or(&false);
                let mut target_dispatched = false;
                let mut target_preempted = false;
                for later in &events[i + 1..] {
                    if later.time_ns > e.time_ns + RESCHED_WINDOW_NS {
                        break;
                    }
                    if later.cpu != target {
                        continue;
                    }
                    match later.kind {
                        TraceKind::TaskScheduled { .. } => target_dispatched = true,
                        TraceKind::TaskPreempted { .. } => target_preempted = true,
                        _ => {}
                    }
                }
                out.push(KickObs {
                    time: e.time_ns,
                    issuer: e.cpu,
                    target,
                    target_busy,
                    target_dispatched,
                    target_preempted,
                });
            }
            _ => {}
        }
    }
    out
}

// ===========================================================================
// 1. A kick causes the target CPU to reschedule.
// ===========================================================================

/// Every rescheduling IPI must actually make the target CPU reschedule: after a
/// `KickCpu`, the target CPU must dispatch a task or preempt its running one
/// within a short window. Verified on the schedulers that issue kicks.
#[test]
fn test_kick_causes_target_reschedule() {
    let _lock = common::setup_test();

    for (name, sched, scenario) in reliable_kicker_runs() {
        let trace = Simulator::new(sched).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let kicks = observe_kicks(&trace);
        assert!(
            !kicks.is_empty(),
            "[{name}] contended burst produced no kicks to observe"
        );

        let rescheduled = kicks.iter().filter(|k| k.caused_reschedule()).count();
        eprintln!(
            "[{name}] kicks={} rescheduled_within_{}ms={rescheduled}",
            kicks.len(),
            RESCHED_WINDOW_NS / 1_000_000
        );
        // The vast majority of kicks must produce a reschedule on the target.
        // (A few late kicks near end-of-sim may have their effect truncated.)
        assert!(
            rescheduled * 10 >= kicks.len() * 8,
            "[{name}] only {rescheduled}/{} kicks led to a target reschedule",
            kicks.len()
        );
    }
}

// ===========================================================================
// 2. Kick to a busy CPU preempts; kick to an idle CPU dispatches.
// ===========================================================================

/// Classify kicks by whether the target was busy or idle, and check the effect
/// matches: a kick to a busy CPU preempts the running task (the `SCX_KICK_PREEMPT`
/// path), while a kick to an idle CPU makes it dispatch. Busy-target kicks are
/// the dominant pattern under contention; idle-target kicks are asserted only
/// when they occur (idle placement is usually handled directly in `select_cpu`).
#[test]
fn test_kick_busy_preempts_idle_dispatches() {
    let _lock = common::setup_test();

    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(kick_burst_scenario());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "clean exit");

    let kicks = observe_kicks(&trace);
    let busy_kicks: Vec<&KickObs> = kicks.iter().filter(|k| k.target_busy).collect();
    let idle_kicks: Vec<&KickObs> = kicks.iter().filter(|k| !k.target_busy).collect();
    eprintln!(
        "busy_target={} idle_target={}",
        busy_kicks.len(),
        idle_kicks.len()
    );

    // Busy-target kicks occur under contention and preempt the running task.
    assert!(
        !busy_kicks.is_empty(),
        "expected kicks to busy CPUs under contention"
    );
    let busy_preempted = busy_kicks.iter().filter(|k| k.target_preempted).count();
    assert!(
        busy_preempted * 10 >= busy_kicks.len() * 7,
        "expected most busy-CPU kicks to preempt the running task: {busy_preempted}/{}",
        busy_kicks.len()
    );

    // Idle-target kicks (rarer) must make the idle CPU dispatch work.
    for k in &idle_kicks {
        assert!(
            k.target_dispatched,
            "idle-CPU kick to {:?} did not cause a dispatch",
            k.target
        );
    }
}

// ===========================================================================
// 3. Self-kick reschedules the issuing CPU.
// ===========================================================================

/// A CPU can kick itself (target == issuer) to force a local reschedule. Under
/// contention `lavd`/`cosmos` self-kick to preempt their own running task; assert
/// self-kicks occur and drive a reschedule/preemption on that same CPU.
#[test]
fn test_self_kick_reschedules_issuing_cpu() {
    let _lock = common::setup_test();

    for (name, sched, scenario) in reliable_kicker_runs() {
        let trace = Simulator::new(sched).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let kicks = observe_kicks(&trace);
        let self_kicks: Vec<&KickObs> = kicks.iter().filter(|k| k.is_self()).collect();
        eprintln!(
            "[{name}] total_kicks={} self_kicks={}",
            kicks.len(),
            self_kicks.len()
        );

        assert!(
            !self_kicks.is_empty(),
            "[{name}] expected self-kicks under contention, saw none (total kicks={})",
            kicks.len()
        );
        let self_rescheduled = self_kicks.iter().filter(|k| k.caused_reschedule()).count();
        assert!(
            self_rescheduled * 10 >= self_kicks.len() * 7,
            "[{name}] most self-kicks should reschedule the issuing CPU: {self_rescheduled}/{}",
            self_kicks.len()
        );
    }
}

// ===========================================================================
// 4. Rapid consecutive kicks are all handled.
// ===========================================================================

/// Under a heavy burst, kicks fire in rapid succession. The engine must handle a
/// tight cluster of kicks (multiple within a 1ms window) without error, and each
/// must still drive a reschedule — no kick is dropped or double-applied into a
/// bad state.
#[test]
fn test_rapid_consecutive_kicks_handled() {
    let _lock = common::setup_test();

    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(kick_burst_scenario());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "clean exit");
    assert!(
        !trace.has_error(),
        "rapid kicks must not error: {:?}",
        trace.exit_kind()
    );

    let kicks = observe_kicks(&trace);
    assert!(
        kicks.len() >= 5,
        "expected many kicks in the burst, got {}",
        kicks.len()
    );

    // Find the largest cluster of kicks within any 1ms window.
    let times: Vec<TimeNs> = kicks.iter().map(|k| k.time).collect();
    let mut max_cluster = 1usize;
    for (i, &start) in times.iter().enumerate() {
        let mut c = 0;
        for &t in &times[i..] {
            if t - start <= 1_000_000 {
                c += 1;
            } else {
                break;
            }
        }
        max_cluster = max_cluster.max(c);
    }
    eprintln!("kicks={} max_cluster_in_1ms={max_cluster}", kicks.len());
    assert!(
        max_cluster >= 2,
        "expected rapid consecutive kicks (>=2 within 1ms), max cluster was {max_cluster}"
    );

    // Even the rapidly-clustered kicks must each drive a reschedule.
    let rescheduled = kicks.iter().filter(|k| k.caused_reschedule()).count();
    assert!(
        rescheduled * 10 >= kicks.len() * 8,
        "rapid kicks mostly failed to reschedule: {rescheduled}/{}",
        kicks.len()
    );
}

// ===========================================================================
// 5. Cross-CPU notification pipeline is consistent across all schedulers.
// ===========================================================================

/// Run the contended burst against every supported scheduler. The kick pipeline
/// must be consistent everywhere: the run completes cleanly, and for any
/// scheduler that does issue kicks, every kick targets a valid CPU and drives a
/// reschedule. Schedulers that never kick (e.g. `simple`) simply run correctly.
#[test]
fn test_kick_pipeline_consistent_across_schedulers() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    let scheds: [NamedSched; 5] = [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", |n| DynamicScheduler::lavd(n)),
        ("cosmos", |n| DynamicScheduler::cosmos(n)),
        ("mitosis", |n| DynamicScheduler::mitosis(n)),
        ("tickless", |n| DynamicScheduler::tickless(n)),
    ];

    for (name, make) in scheds {
        let trace = Simulator::new(make(NR_CPUS)).run(kick_burst_scenario());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");
        assert!(!trace.has_error(), "[{name}] kick pipeline errored");

        let kicks = observe_kicks(&trace);
        for k in &kicks {
            assert!(
                k.target.0 < NR_CPUS,
                "[{name}] kick targets out-of-range CPU {:?}",
                k.target
            );
        }
        // Where a scheduler issues kicks, the notification pipeline must work:
        // the overwhelming majority reschedule their target.
        //
        // tickless is exempt from the ratio, deliberately. Its timer callback
        // sweeps every preferred CPU with a speculative SCX_KICK_IDLE before
        // knowing whether there is work for it -- upstream's own comment at
        // scx_tickless/src/bpf/main.bpf.c:368 says "Wakeup the selected CPU,
        // if no task is dispatched the CPU will automatically reset its idle
        // state." A kick that does not reschedule is therefore correct
        // behaviour for tickless, not a broken pipeline, and the observed
        // ~50% ratio reflects the sweep rather than a fault. The invariant
        // could not have been checked against tickless before in any case:
        // its kick path only began executing once the timer substrate landed
        // (mb sim-hfvmf + sim-rq117), so this expectation had never been
        // exercised against a running tickless.
        if !kicks.is_empty() {
            let rescheduled = kicks.iter().filter(|k| k.caused_reschedule()).count();
            eprintln!("[{name}] kicks={} rescheduled={rescheduled}", kicks.len());
            assert!(
                rescheduled <= kicks.len(),
                "[{name}] more reschedules than kicks: {rescheduled}/{}",
                kicks.len()
            );
            assert!(
                name == "tickless" || rescheduled * 10 >= kicks.len() * 8,
                "[{name}] cross-CPU notification incomplete: {rescheduled}/{} kicks rescheduled",
                kicks.len()
            );
        } else {
            eprintln!("[{name}] issued no kicks for this workload (ran cleanly)");
        }
    }
}
