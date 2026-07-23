//! cgroup CPU-bandwidth THROTTLE ↔ UNTHROTTLE cycle tests under LAVD.
//!
//! LAVD is the scheduler that compiles in the real `scx/lib/cgroup_bw.bpf.c`
//! library (No-Stub / model-the-kernel: throttle/replenish decisions run in the
//! linked-in BPF library, never a Rust approximation). With `enable_cpu_bw=true`
//! LAVD consults `cgroup_throttled()` inside `lavd_enqueue` and puts a throttled
//! cgroup's tasks aside in the per-LLC BTQ, draining them back on the library's
//! `replenish_timerfn` (period reset). That full lifecycle is what these tests
//! exercise.
//!
//! ## Observable throttle/unthrottle vocabulary (all real TraceKinds)
//!
//! Throttle (quota exhausted → tasks withheld):
//!   * `CgroupBwReplenish{ keep_throttled=true, .. }` — the library decided the
//!     cgroup stays throttled into the next period (`period_budget_out <= 0`).
//!   * `LavdBailOnCgroupThrottle{pid,cgid}` / `CgroupBwDequeueOnThrottle` /
//!     `CgroupBwDenied` — a task was withheld from dispatch.
//!   * `CbwPutAside{cgid,count,btq_len_after}` — tasks parked in the BTQ.
//!
//! Unthrottle (period reset → tasks re-admitted):
//!   * `CgroupBwReplenish{ keep_throttled=false, .. }` — throttle cleared.
//!   * `LavdReenqueueViaBtqDrain{cgid}` / `CgroupBwReenqueueOnReplenish` — a
//!     withheld task was re-enqueued.
//!   * `CbwDrainBtqBatch{cgid,count,btq_len_after}` — BTQ drained.
//!
//! ## Relationship to existing cgroup-bw tests (no duplication)
//!
//!  * `cgroup_bw_replenish_smoking_gun.rs` asserts replenish events FIRE and
//!    that post-V4-C oscillation is HEALTHY (`max_consecutive_kt <= 1`, one
//!    clean recovery). It does not assert the *causal* quota→throttle link, the
//!    explicit true→false unthrottle transition count, MULTIPLE full cycles, or
//!    the task-withheld→task-redispatched lifecycle.
//!  * `cgroup_hierarchy.rs` asserts hierarchy bandwidth *differentiation*
//!    (runtime comparisons) — not the throttle/unthrottle event cycle.
//!  * `stress.rs` / `bug1_canonical_repro.rs` cover flat lifecycle / the H6
//!    stall fingerprint via stderr printk.
//!
//! This file targets the throttle↔unthrottle CYCLE directly, on observable
//! trace events, for each of the six task goals.
//!
//! ## Substrate limitation surfaced by this work (nested enforcement)
//!
//! The sim enforces cgroup CPU-bandwidth throttle only for ROOT-level (level-1)
//! cgroups. NESTED cgroups (level>1) are never throttled because the substrate
//! stubs `bpf_cgroup_ancestor()` to NULL (`csrc/sim_bpf_stubs.c`), so
//! `cgroup_bw.bpf.c` sets `parent_id=0` for every cgroup and an ancestor's quota
//! never propagates down ("Fail to lookup parent ctx"). Tracked in mb sim-4e4c0a.
//! Consequently item 5 (nested-hierarchy throttle) is covered here by a passing
//! throttle-SCOPING/isolation test plus an `#[ignore]`d executable spec for the
//! ancestor-propagation case that flips green once the substrate is fixed.
//!
//! Deterministic serial engine, fixed seed + `instant_timing()`.

use std::collections::BTreeMap;

use scx_simulator::*;

mod common;

// ---------------------------------------------------------------------------
// Scheduler / workload helpers
// ---------------------------------------------------------------------------

/// Set a `bool` global in the loaded LAVD `.so` (mirrors the smoking-gun test).
///
/// # Safety
/// `name` must be the NUL-terminated literal name of a `bool` global present in
/// the loaded LAVD `.so`; the scheduler must outlive the write.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// LAVD with the real cgroup_bw library actively enforcing throttle
/// (`enable_cpu_bw=true`) and a comfortable cgroup-tracking capacity.
fn lavd_cpu_bw(nr_cpus: u32) -> DynamicScheduler {
    let sched = DynamicScheduler::lavd(nr_cpus);
    sched.lavd_set_cgroup_bw_max(64);
    // SAFETY: `enable_cpu_bw` is a bool global in LAVD's main.bpf.c; the sched
    // outlives this write.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    sched
}

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

// ---------------------------------------------------------------------------
// Trace-derivation helpers
// ---------------------------------------------------------------------------

/// One replenish record: `(keep_throttled, runtime_total_last, period_budget_in,
/// period_budget_out)`.
type ReplenishRec = (bool, i64, i64, i64);

/// Per-cgid replenish history in time order.
fn replenish_by_cgid(trace: &Trace) -> BTreeMap<u64, Vec<ReplenishRec>> {
    let mut per_cg: BTreeMap<u64, Vec<ReplenishRec>> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::CgroupBwReplenish {
            cgid,
            runtime_total_last,
            period_budget_in,
            period_budget_out,
            keep_throttled,
            ..
        } = &ev.kind
        {
            per_cg.entry(cgid.0).or_default().push((
                *keep_throttled,
                *runtime_total_last,
                *period_budget_in,
                *period_budget_out,
            ));
        }
    }
    per_cg
}

/// A trace event that WITHHOLDS a task because its cgroup is throttled.
fn is_throttle_withhold(k: &TraceKind) -> bool {
    matches!(
        k,
        TraceKind::LavdBailOnCgroupThrottle { .. }
            | TraceKind::CgroupBwDequeueOnThrottle { .. }
            | TraceKind::CgroupBwDenied { .. }
            | TraceKind::CbwPutAside { .. }
    )
}

/// A trace event that RE-ADMITS a withheld task on unthrottle/replenish.
fn is_unthrottle_readmit(k: &TraceKind) -> bool {
    matches!(
        k,
        TraceKind::LavdReenqueueViaBtqDrain { .. }
            | TraceKind::CgroupBwReenqueueOnReplenish { .. }
            | TraceKind::CbwDrainBtqBatch { .. }
    )
}

fn count_kind(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Count throttle→unthrottle cycles on a single cgroup's replenish history: a
/// cycle completes on each `keep_throttled` true→false transition.
fn count_unthrottle_transitions(recs: &[ReplenishRec]) -> usize {
    recs.windows(2)
        .filter(|w| w[0].0 /* was throttled */ && !w[1].0 /* now not */)
        .count()
}

/// Count throttle onsets: `keep_throttled` false→true transitions (plus a
/// leading `true` if the history starts already throttled).
fn count_throttle_onsets(recs: &[ReplenishRec]) -> usize {
    let mut onsets = 0;
    let mut prev = false;
    for (kt, ..) in recs {
        if *kt && !prev {
            onsets += 1;
        }
        prev = *kt;
    }
    onsets
}

fn assert_identical(t1: &Trace, t2: &Trace, ctx: &str) {
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "{ctx}: trace lengths differ"
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

/// A tight-quota cgroup (10% of one CPU over a 100ms period) full of CPU-bound
/// tasks that blow through the quota every period, guaranteeing throttle. Long
/// enough (`dur_ms`) to cross several 100ms replenish periods.
fn throttle_scenario(nr_cpus: u32, nr_tasks: u32, dur_ms: u64) -> Scenario {
    let cpus: Vec<CpuId> = (0..nr_cpus).map(CpuId).collect();
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .seed(42)
        .instant_timing()
        .cgroup_with_bandwidth("tight", &cpus, 100_000, 10_000, 0);
    for i in 0..nr_tasks {
        b = b.add_task_in_cgroup(&format!("hog{i}"), 0, forever_run(2_000_000_000), "tight");
    }
    b.duration_ms(dur_ms).build()
}

// ---------------------------------------------------------------------------
// 1. Throttle triggers when quota is exhausted.
// ---------------------------------------------------------------------------

/// A CPU-bound task in a 10ms/100ms cgroup exhausts its quota every period. The
/// library must throttle it: at least one `CgroupBwReplenish{keep_throttled=
/// true}` whose just-ended period consumed at least its budget
/// (`runtime_total_last >= period_budget_in`) — i.e. throttle is *caused* by
/// quota exhaustion, not spurious.
#[test]
fn test_throttle_triggers_on_quota_exhaustion() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(throttle_scenario(4, 1, 600));
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    let per_cg = replenish_by_cgid(&trace);
    assert!(
        !per_cg.is_empty(),
        "no CgroupBwReplenish events — cgroup_bw not enforcing"
    );

    // Some throttled period must have been *caused* by consuming >= budget.
    let caused: bool = per_cg
        .values()
        .flatten()
        .any(|(kt, rtl, pb_in, _pbo)| *kt && *rtl >= *pb_in && *pb_in > 0);
    let n_throttled: usize = per_cg.values().flatten().filter(|(kt, ..)| *kt).count();
    eprintln!(
        "throttle-on-exhaustion: {n_throttled} throttled periods; caused-by-exhaustion={caused}"
    );
    assert!(n_throttled > 0, "expected at least one throttled period");
    assert!(
        caused,
        "expected a throttle caused by quota exhaustion (rtl >= budget); per_cg={per_cg:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Unthrottle when the period resets.
// ---------------------------------------------------------------------------

/// After throttling, the periodic `replenish_timerfn` must clear the throttle:
/// at least one `keep_throttled` true→false transition (an unthrottle), and the
/// cgroup ends the run un-throttled (recovers, does not stall forever).
#[test]
fn test_unthrottle_on_period_reset() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(throttle_scenario(4, 1, 800));
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let per_cg = replenish_by_cgid(&trace);
    let unthrottles: usize = per_cg
        .values()
        .map(|r| count_unthrottle_transitions(r))
        .sum();
    let throttles: usize = per_cg.values().map(|r| count_throttle_onsets(r)).sum();
    eprintln!(
        "unthrottle-on-reset: {throttles} throttle onsets, {unthrottles} unthrottle transitions"
    );

    assert!(throttles > 0, "expected at least one throttle onset");
    assert!(
        unthrottles > 0,
        "expected at least one unthrottle (keep_throttled true->false) on period reset; per_cg={per_cg:?}"
    );
    // The cgroup that throttled must not be left permanently throttled.
    for (cgid, recs) in &per_cg {
        if recs.iter().any(|(kt, ..)| *kt) {
            let last_kt = recs.last().map(|(kt, ..)| *kt).unwrap_or(false);
            assert!(
                !last_kt,
                "cgid={cgid} ended permanently throttled (stall); history={recs:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Multiple throttle/unthrottle cycles.
// ---------------------------------------------------------------------------

/// Over many replenish periods a continuously-demanding cgroup oscillates:
/// throttle → recover → throttle → recover ... Assert at least TWO complete
/// unthrottle transitions (i.e. ≥2 cycles) on the same cgroup.
#[test]
fn test_multiple_throttle_unthrottle_cycles() {
    let _lock = common::setup_test();
    // 1 second → ~10 replenish periods, plenty for multiple cycles.
    let trace = Simulator::new(lavd_cpu_bw(4)).run(throttle_scenario(4, 2, 1000));
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let per_cg = replenish_by_cgid(&trace);
    let max_cycles = per_cg
        .values()
        .map(|r| count_unthrottle_transitions(r))
        .max()
        .unwrap_or(0);
    eprintln!("multi-cycle: max unthrottle transitions on any cgroup = {max_cycles}");
    assert!(
        max_cycles >= 2,
        "expected >= 2 throttle/unthrottle cycles on a cgroup, got {max_cycles}; per_cg={per_cg:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Tasks queued during throttle are dispatched on unthrottle.
// ---------------------------------------------------------------------------

/// With several CPU-bound tasks packed into a tight cgroup, throttle withholds
/// runnable tasks (BTQ put-aside / LAVD bail) and the subsequent replenish
/// re-admits them (BTQ drain / re-enqueue). Assert BOTH sides of the lifecycle
/// fire, and that a withheld task actually resumes running afterwards.
#[test]
fn test_tasks_queued_during_throttle_dispatched_on_unthrottle() {
    let _lock = common::setup_test();
    // Few CPUs, many tasks → strong contention inside the throttled cgroup.
    let trace = Simulator::new(lavd_cpu_bw(2)).run(throttle_scenario(2, 6, 800));
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let withheld = count_kind(&trace, is_throttle_withhold);
    let readmit = count_kind(&trace, is_unthrottle_readmit);
    eprintln!("queue-during-throttle: {withheld} withhold events, {readmit} re-admit events");

    assert!(
        withheld > 0,
        "expected tasks to be withheld during throttle (BTQ put-aside / LAVD bail / dequeue)"
    );
    assert!(
        readmit > 0,
        "expected withheld tasks to be re-admitted on unthrottle (BTQ drain / re-enqueue)"
    );

    // Every task made progress across the run (withheld tasks were eventually
    // dispatched — no permanent starvation from throttle).
    for p in 1..=6i32 {
        assert!(
            trace.schedule_count(Pid(p)) > 0,
            "task {p} was never dispatched (starved by throttle)"
        );
    }

    // A re-admit must be followed by the cgroup's tasks running again: find the
    // first re-admit time, assert some task is scheduled at/after it.
    if let Some(readmit_t) = trace
        .events()
        .iter()
        .find(|e| is_unthrottle_readmit(&e.kind))
        .map(|e| e.time_ns)
    {
        let ran_after = trace
            .events()
            .iter()
            .any(|e| e.time_ns >= readmit_t && matches!(e.kind, TraceKind::TaskScheduled { .. }));
        assert!(
            ran_after,
            "no task ran after the unthrottle re-admit at {readmit_t}ns"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Throttle interaction with nested cgroup hierarchies.
// ---------------------------------------------------------------------------

/// Throttle interaction with a system that CONTAINS a nested hierarchy: a
/// root-level bandwidth-limited cgroup (which throttles correctly — level 1)
/// coexists with a separate nested, *unlimited* hierarchy. The throttle must be
/// correctly SCOPED — it fires on the limited cgroup while the nested unlimited
/// hierarchy's task keeps running unthrottled and out-runs the throttled tasks.
///
/// This is the honest, working slice of "throttle × nested hierarchy": the sim
/// enforces bandwidth only for level-1 cgroups (see the ignored spec below and
/// mb sim-4e4c0a for why ancestor-limit propagation to nested tasks does not yet
/// work), so item 5 is covered here via throttle SCOPING/ISOLATION rather than
/// ancestor propagation.
#[test]
fn test_throttle_isolation_with_nested_hierarchy() {
    let _lock = common::setup_test();
    let cpus = [CpuId(0), CpuId(1)];
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        // Limited root cgroup with its own tasks (throttles — level 1).
        .cgroup_with_bandwidth("tight", &cpus, 100_000, 10_000, 0)
        .add_task_in_cgroup("t1", 0, forever_run(2_000_000_000), "tight")
        .add_task_in_cgroup("t2", 0, forever_run(2_000_000_000), "tight")
        // Separate UNLIMITED nested hierarchy with a task in the leaf.
        .cgroup("free_parent", &cpus)
        .cgroup_nested("free_child", "free_parent", None)
        .add_task_in_cgroup("f1", 0, forever_run(2_000_000_000), "free_child")
        .duration_ms(800)
        .build();

    let trace = Simulator::new(lavd_cpu_bw(2)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    // The limited (level-1) cgroup throttled and recovered.
    let per_cg = replenish_by_cgid(&trace);
    let throttles: usize = per_cg.values().map(|r| count_throttle_onsets(r)).sum();
    let unthrottles: usize = per_cg
        .values()
        .map(|r| count_unthrottle_transitions(r))
        .sum();
    eprintln!("throttle-isolation: {throttles} throttle onsets, {unthrottles} unthrottles");
    assert!(
        throttles > 0,
        "limited cgroup never throttled; per_cg={per_cg:?}"
    );
    assert!(
        unthrottles > 0,
        "limited cgroup never unthrottled; per_cg={per_cg:?}"
    );

    // Throttle is correctly SCOPED: the unlimited nested task (pid 3) runs a lot
    // and out-runs each throttled task (pids 1,2) — the limit did not leak to
    // the nested hierarchy.
    let free_rt = trace.total_runtime(Pid(3));
    let tight_each = trace.total_runtime(Pid(1)).max(trace.total_runtime(Pid(2)));
    eprintln!("throttle-isolation: free(nested,unlimited)={free_rt} tight_each={tight_each}");
    assert!(free_rt > 0 && tight_each > 0, "all tasks should run");
    assert!(
        free_rt > tight_each,
        "unlimited nested task should out-run each throttled task: free={free_rt} tight_each={tight_each}"
    );
}

/// Executable SPEC (currently blocked): ancestor bandwidth limit propagating to
/// a nested child's tasks. The limit sits on a root parent; the CPU-bound tasks
/// live in a nested child. In a correct hierarchy the child inherits the
/// ancestor's quota and throttles.
///
/// This does NOT pass today: the sim substrate stubs `bpf_cgroup_ancestor()` to
/// NULL (`csrc/sim_bpf_stubs.c`), so `cgroup_bw.bpf.c` sets `parent_id=0` for
/// every cgroup and `cbw_update_nquota_ub` fails to resolve the parent ("Fail to
/// lookup parent ctx: 0") — the nested child is treated as unlimited and never
/// throttles. Tracked in mb sim-4e4c0a. Remove `#[ignore]` once that lands.
#[test]
#[ignore = "blocked on mb sim-4e4c0a: sim bpf_cgroup_ancestor NULL stub -> nested cgroup parent_id=0, ancestor bw limit not propagated"]
fn test_nested_ancestor_limit_throttles_child() {
    let _lock = common::setup_test();
    let cpus = [CpuId(0), CpuId(1)];
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        .cgroup_with_bandwidth("parent", &cpus, 100_000, 10_000, 0)
        .cgroup_nested("child", "parent", None)
        .add_task_in_cgroup("d1", 0, forever_run(2_000_000_000), "child")
        .add_task_in_cgroup("d2", 0, forever_run(2_000_000_000), "child")
        .duration_ms(800)
        .build();

    let trace = Simulator::new(lavd_cpu_bw(2)).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let per_cg = replenish_by_cgid(&trace);
    let throttles: usize = per_cg.values().map(|r| count_throttle_onsets(r)).sum();
    let unthrottles: usize = per_cg
        .values()
        .map(|r| count_unthrottle_transitions(r))
        .sum();
    assert!(
        throttles > 0,
        "nested child never throttled; per_cg={per_cg:?}"
    );
    assert!(
        unthrottles > 0,
        "nested child never unthrottled; per_cg={per_cg:?}"
    );
    assert!(trace.total_runtime(Pid(1)) > 0 && trace.total_runtime(Pid(2)) > 0);
}

// ---------------------------------------------------------------------------
// 6. Throttle state is correct/consistent in trace output.
// ---------------------------------------------------------------------------

/// The library's throttle bookkeeping must be internally consistent in the
/// trace. `keep_throttled == (period_budget_out <= 0)` field-sanity is shared
/// with the smoking-gun test; the DISTINCT contribution here is the direct
/// throttle-state-transition stream (`CbwThrottleCgroups`), which no other test
/// asserts on: both a 0→1 (throttle) and a 1→0 (unthrottle) transition must
/// appear, and any cgroup that became throttled must also become un-throttled
/// (no stuck-throttle / stall).
#[test]
fn test_throttle_state_consistent_in_trace() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(2)).run(throttle_scenario(2, 4, 800));
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    // (a) keep_throttled is exactly the sign of period_budget_out in every record.
    for ev in trace.events() {
        if let TraceKind::CgroupBwReplenish {
            cgid,
            keep_throttled,
            period_budget_out,
            ..
        } = &ev.kind
        {
            assert_eq!(
                *keep_throttled,
                *period_budget_out <= 0,
                "cgid={} keep_throttled={keep_throttled} inconsistent with period_budget_out={period_budget_out}",
                cgid.0
            );
        }
    }

    // (b) Direct throttle-state-transition stream (CbwThrottleCgroups): both a
    // throttle (0->1) and an unthrottle (1->0) transition must appear, and any
    // cgroup that became throttled must also become un-throttled (no stuck
    // throttle — the stall fingerprint).
    let mut became_throttled: BTreeMap<u64, bool> = BTreeMap::new();
    let mut became_unthrottled: BTreeMap<u64, bool> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::CbwThrottleCgroups { cgid, throttled } = &ev.kind {
            if *throttled {
                became_throttled.insert(cgid.0, true);
            } else {
                became_unthrottled.insert(cgid.0, true);
            }
        }
    }
    eprintln!(
        "state-consistency: became_throttled={:?} became_unthrottled={:?}",
        became_throttled.keys().collect::<Vec<_>>(),
        became_unthrottled.keys().collect::<Vec<_>>()
    );
    assert!(
        !became_throttled.is_empty(),
        "expected at least one CbwThrottleCgroups throttle (0->1) transition"
    );
    assert!(
        !became_unthrottled.is_empty(),
        "expected at least one CbwThrottleCgroups unthrottle (1->0) transition"
    );
    for cgid in became_throttled.keys() {
        assert!(
            became_unthrottled.contains_key(cgid),
            "cgid={cgid} became throttled but never un-throttled (stuck-throttle / stall)"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Determinism guard (no flakes across the throttle/unthrottle scenarios).
// ---------------------------------------------------------------------------

#[test]
fn test_throttle_cycle_determinism() {
    let _lock = common::setup_test();
    let t1 = Simulator::new(lavd_cpu_bw(4)).run(throttle_scenario(4, 2, 600));
    let t2 = Simulator::new(lavd_cpu_bw(4)).run(throttle_scenario(4, 2, 600));
    assert_identical(&t1, &t2, "throttle cycle");
    assert_eq!(t1.exit_kind(), &ExitKind::Normal);
    assert!(
        replenish_by_cgid(&t1)
            .values()
            .flatten()
            .any(|(kt, ..)| *kt),
        "determinism scenario should still throttle"
    );
}
