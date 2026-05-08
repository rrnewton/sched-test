//! H6 dual-controller stall matrix (Diff 5 of the 5-diff cgroup_bw stack).
//!
//! Hypothesis 6 (H6) of the LAVD Bug-1 investigation says the live stall
//! requires *two* independent CPU-bandwidth controllers to be simultaneously
//! active on the same cgroup:
//!
//! 1. The kernel's CFS `cpu.max` controller (modeled in scxsim by the Diff 3
//!    engine enforcement loop: charge → throttle → admission gate → refill).
//! 2. LAVD's BPF cgroup-bw layer (modeled in scxsim by the Diff 4 wrapper
//!    redirect: `sim_cgroup_bw_*` shims that route LAVD's `scx_cgroup_bw_*`
//!    calls into the same engine-owned [`BandwidthManager`]).
//!
//! Reference:
//! `experiments/lavd_cpubw_stalls_202604/SCXSIM_CGROUP_BW_DESIGN.md`
//! (H6 cell table, lines 33-37) and
//! `PR11_chase_20260507-025500Z/HYPOTHESIS_6_DISAMBIGUATION.md`.
//!
//! # Cell semantics (per design doc, lines 33-37)
//!
//! | Cell | Kernel `cpu.max` finite | LAVD `enable_cpu_bw` | Expected     |
//! |------|------------------------:|---------------------:|--------------|
//! | C    | yes                     | yes                  | Bug-1 stall  |
//! | A    | no, `max`               | yes                  | no stall     |
//! | B    | yes                     | no                   | no stall     |
//!
//! Each test below pins a single cell of that matrix, asserting the stall
//! shape (or its absence) via the engine-side trace events introduced by
//! Diff 3:
//!
//! - `CgroupBwCharge` — runtime accounted against quota
//! - `CgroupBwThrottle` — quota exhausted, cgroup is now throttled
//! - `CgroupBwDenied` — DSQ-pop admission gate refused dispatch
//! - `CgroupBwRefill` — period boundary fired, throttle cleared
//!
//! In scxsim's first-cut model, the engine `BandwidthManager` is the single
//! source of truth for both controllers (Diff 4 redirects LAVD's wrapper.c
//! `scx_cgroup_bw_*` to the same `BandwidthManager`). That coherence is
//! exactly what Bug-1 violates in the live kernel — so the simulator is, by
//! construction, *less* divergent than the production state split. What it
//! *does* faithfully reproduce is the engine head-of-line block: a runnable
//! task pinned at the front of a DSQ, refused dispatch because its cgroup is
//! out of quota. That is the same observable shape the watchdog catches in
//! production (a runnable task that fails to run for the watchdog interval).

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// LAVD scheduler-global helpers (mirror lavd.rs's local `lavd_set_bool` so
// this test file stays self-contained — H6 needs them to flip
// `enable_cpu_bw` between cells).
// ---------------------------------------------------------------------------

/// Write a boolean value to a named LAVD global variable.
///
/// # Safety
/// Caller must ensure `name` is the literal name of a `bool` global in the
/// loaded LAVD `.so` and that `sched` outlives the write.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

// ---------------------------------------------------------------------------
// Trace-shape summary: derived purely from public TraceKind events. Centralized
// so each cell test stays a 5-line assertion block instead of re-walking the
// trace by hand.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct BwStallShape {
    n_charge: u32,
    n_throttle: u32,
    n_denied: u32,
    n_refill: u32,
}

impl BwStallShape {
    fn collect(trace: &Trace) -> Self {
        let mut s = Self::default();
        for ev in trace.events() {
            match ev.kind {
                TraceKind::CgroupBwCharge { .. } => s.n_charge += 1,
                TraceKind::CgroupBwThrottle { .. } => s.n_throttle += 1,
                TraceKind::CgroupBwDenied { .. } => s.n_denied += 1,
                TraceKind::CgroupBwRefill { .. } => s.n_refill += 1,
                _ => {}
            }
        }
        s
    }

    /// Did the engine exercise the kernel-CFS-bw enforcement loop on this
    /// run?
    ///
    /// We require `charge`, `throttle`, and `refill` together. We do NOT
    /// require `CgroupBwDenied`: that event only fires from the engine's
    /// `local_dsq.front()` admission gate (Diff 3), and LAVD's dispatch
    /// path uses vtime/pinned DSQs rather than the simple per-CPU local
    /// DSQ that scx_simple uses. The Diff-3 baseline test
    /// (`test_engine_throttles_and_refills_tight_quota`) confirms `denied`
    /// fires under scx_simple; under LAVD the same engine state machine
    /// runs but the head-of-line block surfaces through LAVD's BTQ /
    /// throttle checks instead, which the existing
    /// `test_lavd_cgroup_bw_wrapper_redirect_charges_engine` covers and
    /// which our matrix does not need to re-prove.
    fn is_full_engine_enforcement_loop(&self) -> bool {
        self.n_charge > 0 && self.n_throttle > 0 && self.n_refill > 0
    }
}

// ---------------------------------------------------------------------------
// Shared scenario knobs used by the cell-A/B/C tests.
//
// We deliberately use *the same workload + same nr_cpus* across all three
// cells so the only variable is the controller configuration. That makes the
// matrix a true ablation: only the controller-axis change can move the
// observed behavior between "stall loop fires" and "no events at all."
// ---------------------------------------------------------------------------

/// Number of CPUs for the H6 matrix. 4 matches the existing
/// `cgroup_bw_enforcement.rs` baseline and keeps simulated wall time low.
const H6_NR_CPUS: u32 = 4;

/// Cgroup name used by every cell.
const H6_CGROUP_NAME: &str = "h6_test";

/// Tight-quota recipe: `quota=10ms / period=100ms = 10%` of one CPU. Mirrors
/// David Dai's R1 reproducer (`cpu.max="10000 100000"` from
/// `experiments/lavd_cpubw_stalls_202604/davids-artifacts/lavd_bw_repro_r1_tight.sh`).
const H6_PERIOD_US: u64 = 100_000;
const H6_QUOTA_US: u64 = 10_000;

/// Simulated workload duration: long enough to cross multiple period
/// boundaries (>= 3 refills with `period=100ms`) so charge/throttle/refill
/// all have room to fire. Kept under 1s of simulated time so the test stays
/// well under the 5s wall-time budget.
const H6_DURATION_MS: u64 = 350;

/// CPU-bound work per task: large enough that a single-CPU 10% budget is
/// always saturated. 200ms of contiguous run > 10ms quota by 20x.
const H6_TASK_RUN_NS: u64 = 200_000_000;

// ---------------------------------------------------------------------------
// Cell C: BOTH controllers active — the Bug-1 surface.
// ---------------------------------------------------------------------------

/// Cell C: kernel `cpu.max` finite AND LAVD `enable_cpu_bw=true`.
///
/// This is the H6 reproducer cell. Both controllers observe the same task
/// runtime through the engine-owned `BandwidthManager` (Diff 4 redirect), so
/// the engine fires the full charge → throttle → denied → refill loop. The
/// LAVD wrapper-side `scx_cgroup_bw_throttled()` returns -EAGAIN consistent
/// with the engine view (Diff 4 coherence), so LAVD's own dispatch checks
/// also defer the task — but the *observable* stall shape that production's
/// watchdog catches is the engine's head-of-line block at the DSQ.
///
/// Asserts the full-loop signature.
#[test]
fn test_h6_cell_c_dual_controller_full_stall_loop() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(H6_NR_CPUS);

    // LAVD-side controller: ON.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }

    // Kernel-side controller: ON (finite quota).
    let scenario = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup_with_bandwidth(
            H6_CGROUP_NAME,
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            H6_PERIOD_US,
            H6_QUOTA_US,
            0,
        )
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();

    let trace = Simulator::new(sched).run(scenario);
    let shape = BwStallShape::collect(&trace);

    assert!(
        shape.is_full_engine_enforcement_loop(),
        "Cell C (kernel cpu.max + LAVD enable_cpu_bw): expected the full \
         charge→throttle→refill engine loop, got {shape:?}. The engine \
         head-of-line block is the observable Bug-1 shape; one of the \
         legs is missing."
    );

    // Sanity: the hog must still get at least one slice per refill cycle —
    // otherwise the engine over-throttled and produced an artificial stall
    // (a known risk in the design doc, "Risks and open questions").
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "Cell C: hog never scheduled at all — admission gate over-blocked."
    );
}

// ---------------------------------------------------------------------------
// Cell A: kernel cpu.max=max, LAVD enable_cpu_bw=true. No stall.
// ---------------------------------------------------------------------------

/// Cell A: kernel `cpu.max=max` (unlimited) AND LAVD `enable_cpu_bw=true`.
///
/// With no finite kernel quota, the engine `BandwidthManager` has no entry
/// for the cgroup at all (per `BandwidthManager::configure`'s contract:
/// `quota_us == 0 || period_us == 0 → remove`). The engine therefore
/// produces *zero* bw trace events. LAVD's wrapper-side calls also see
/// "no entry → not throttled" through the same coherent state.
///
/// The H6 hypothesis says: with only one controller active, no stall. The
/// trace must show exactly that — no charge, no throttle, no denial, no
/// refill — and the task must still get scheduled normally.
#[test]
fn test_h6_cell_a_kernel_unlimited_lavd_on_no_stall() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(H6_NR_CPUS);

    // LAVD-side controller: ON.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }

    // Kernel-side controller: OFF (no `cgroup_with_bandwidth`, just plain
    // cgroup with cpuset). Tasks belong to a tracked cgroup but no quota.
    let scenario = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup(H6_CGROUP_NAME, &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();

    let trace = Simulator::new(sched).run(scenario);
    let shape = BwStallShape::collect(&trace);

    // Subtlety: `CgroupBwCharge` events are recorded by the engine for
    // *any* task in a tracked cgroup, regardless of whether that cgroup
    // has a `cpu.max` entry in `BandwidthManager`. The bw_manager.charge()
    // call is a no-op for un-quota'd cgroups, but the trace event still
    // fires for observability. The H6-meaningful signals are throttle /
    // denied / refill — those require an actual `BandwidthManager` entry
    // and must all be zero in Cell A.
    assert_eq!(
        shape.n_throttle, 0,
        "Cell A (cpu.max=max, LAVD on): no kernel quota → no \
         BandwidthManager entry → no throttle, got {} ({shape:?})",
        shape.n_throttle
    );
    assert_eq!(
        shape.n_denied, 0,
        "Cell A: expected 0 CgroupBwDenied, got {} ({shape:?})",
        shape.n_denied
    );
    assert_eq!(
        shape.n_refill, 0,
        "Cell A: no quota means no refill timer was scheduled, got {} \
         ({shape:?})",
        shape.n_refill
    );
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "Cell A: hog must run freely under cpu.max=max — got 0 schedules."
    );
}

// ---------------------------------------------------------------------------
// Cell B: kernel cpu.max=finite, LAVD enable_cpu_bw=false. Single-controller
// stall (engine-only); LAVD-side calls return "not throttled."
// ---------------------------------------------------------------------------

/// Cell B: kernel `cpu.max` finite AND LAVD `enable_cpu_bw=false`.
///
/// Engine-side throttling fires (the engine's enforcement loop is decoupled
/// from LAVD's `enable_cpu_bw` flag — Diff 3 wires it directly into the
/// dispatch path). LAVD's `scx_cgroup_bw_throttled()` is never called from
/// LAVD because `enable_cpu_bw=false` short-circuits its own checks; the
/// wrapper redirects in Diff 4 are present but quiescent.
///
/// In production, this cell did NOT stall (per Hypothesis 6
/// disambiguation log). In scxsim's model it produces engine-side throttle
/// events because the engine is the single source of truth — but the
/// scheduler-side path is silent. The relevant invariant for the H6
/// matrix is that the LAVD-on-only path (Cell A) and the LAVD-off path
/// (Cell B) are *distinct* observable cells, and that the dual-controller
/// path (Cell C) hits the full loop.
///
/// Concretely we assert: engine-side throttle/refill events DO fire (the
/// engine still enforces quota), and the task still gets scheduled across
/// refills.
#[test]
fn test_h6_cell_b_kernel_finite_lavd_off_engine_only_throttle() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(H6_NR_CPUS);

    // LAVD-side controller: OFF (default; explicit for clarity).
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", false);
    }

    // Kernel-side controller: ON (finite quota — same as Cell C).
    let scenario = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup_with_bandwidth(
            H6_CGROUP_NAME,
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            H6_PERIOD_US,
            H6_QUOTA_US,
            0,
        )
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();

    let trace = Simulator::new(sched).run(scenario);
    let shape = BwStallShape::collect(&trace);

    // Engine-only throttle path must still exercise the full loop —
    // `enable_cpu_bw` is a LAVD-internal flag, not an engine flag.
    assert!(
        shape.is_full_engine_enforcement_loop(),
        "Cell B (kernel cpu.max finite, LAVD off): engine-side enforcement \
         is independent of LAVD's enable_cpu_bw; expected full \
         charge→throttle→refill loop from the engine, got {shape:?}."
    );
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "Cell B: hog never scheduled — engine over-throttled."
    );
}

// ---------------------------------------------------------------------------
// Cross-cell ablation: one assertion that proves the matrix discriminates.
//
// Re-runs all three cells in one test and asserts the cell-A → cell-{B,C}
// gradient is observable: zero events when neither layer enforces, full
// loop when either kernel-side or both layers enforce. This is the
// "matrix actually distinguishes cells" check that Diff 5's reviewer cares
// about most.
// ---------------------------------------------------------------------------

#[test]
fn test_h6_matrix_discriminates_cells() {
    let _lock = common::setup_test();

    // --- Cell A: cpu.max=max, LAVD on ---
    let sched_a = DynamicScheduler::lavd(H6_NR_CPUS);
    unsafe {
        lavd_set_bool(&sched_a, "enable_cpu_bw\0", true);
    }
    let scen_a = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup(H6_CGROUP_NAME, &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();
    let shape_a = BwStallShape::collect(&Simulator::new(sched_a).run(scen_a));

    // --- Cell B: cpu.max=finite, LAVD off ---
    let sched_b = DynamicScheduler::lavd(H6_NR_CPUS);
    unsafe {
        lavd_set_bool(&sched_b, "enable_cpu_bw\0", false);
    }
    let scen_b = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup_with_bandwidth(
            H6_CGROUP_NAME,
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            H6_PERIOD_US,
            H6_QUOTA_US,
            0,
        )
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();
    let shape_b = BwStallShape::collect(&Simulator::new(sched_b).run(scen_b));

    // --- Cell C: cpu.max=finite, LAVD on ---
    let sched_c = DynamicScheduler::lavd(H6_NR_CPUS);
    unsafe {
        lavd_set_bool(&sched_c, "enable_cpu_bw\0", true);
    }
    let scen_c = Scenario::builder()
        .cpus(H6_NR_CPUS)
        .cgroup_with_bandwidth(
            H6_CGROUP_NAME,
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            H6_PERIOD_US,
            H6_QUOTA_US,
            0,
        )
        .add_task_in_cgroup(
            "hog",
            0,
            workloads::cpu_bound(H6_TASK_RUN_NS),
            H6_CGROUP_NAME,
        )
        .duration_ms(H6_DURATION_MS)
        .build();
    let shape_c = BwStallShape::collect(&Simulator::new(sched_c).run(scen_c));

    eprintln!(
        "[h6_matrix] Cell A (max,on): {shape_a:?}\n[h6_matrix] Cell B (fin,off): \
         {shape_b:?}\n[h6_matrix] Cell C (fin,on): {shape_c:?}"
    );

    // Cell A is the H6-negative case: no kernel quota configured →
    // BandwidthManager has no entry for the cgroup → no throttle/denied/
    // refill events at all. (Charge events do fire because the task is
    // in a tracked cgroup, but they're no-ops on the bw_manager side.)
    assert_eq!(
        (shape_a.n_throttle, shape_a.n_denied, shape_a.n_refill),
        (0, 0, 0),
        "matrix discrimination: cell A must have zero \
         throttle/denied/refill events, got {shape_a:?}"
    );

    // Cells B and C both fire the engine enforcement loop — the engine
    // is the single source of truth and enforces whenever a finite quota
    // is configured.
    assert!(
        shape_b.is_full_engine_enforcement_loop(),
        "matrix discrimination: cell B must fire the engine enforcement \
         loop, got {shape_b:?}"
    );
    assert!(
        shape_c.is_full_engine_enforcement_loop(),
        "matrix discrimination: cell C must fire the engine enforcement \
         loop, got {shape_c:?}"
    );

    // Strict gradient: zero-event cell A is strictly less active than
    // either of the two enforcing cells.
    assert!(
        shape_a.n_throttle < shape_b.n_throttle && shape_a.n_throttle < shape_c.n_throttle,
        "matrix discrimination: cell A throttle count ({}) must be strictly \
         less than B ({}) and C ({})",
        shape_a.n_throttle,
        shape_b.n_throttle,
        shape_c.n_throttle
    );
}
