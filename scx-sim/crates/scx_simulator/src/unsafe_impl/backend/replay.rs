//! Hardware breakpoint replay preemption backend.
//!
//! Replays a recorded preemption trace using a hybrid PMU + hardware
//! breakpoint approach. The PMU timer fires when we're within
//! [`REPLAY_MARGIN`](crate::preempt::REPLAY_MARGIN) branches of the target,
//! then a hardware breakpoint catches the exact instruction pointer.
//!
//! Both PMU timer and HW breakpoint are REQUIRED. If either is unavailable
//! (VMs, containers, missing perf permissions), replay panics with a clear
//! error message rather than silently degrading to cooperative-only mode.

use std::os::unix::io::RawFd;

use tracing::debug;

use crate::backend::pmu::setup_pmu_timer;
use crate::backend::{
    AbsoluteRbc, PreemptTarget, PreemptionBackend, RbcTarget, RelativeRbc, StructopDelta,
};
use crate::engine_ring::EngineRing;
use crate::interleave::WorkerId;
use crate::perf;
use crate::preempt::trace::PreemptionTrace;
use crate::preempt::{self, PreemptRing, ReplayCursor, REPLAY_MARGIN};

/// Hardware breakpoint replay preemption backend.
///
/// Each worker follows its recorded trace of preemption points using the
/// hybrid PMU + hardware breakpoint approach: the PMU timer fires when
/// we're within REPLAY_MARGIN of the target RBC count, then a hardware
/// breakpoint catches the exact instruction pointer.
pub(crate) struct ReplayBackend {
    /// Per-worker cursors into the replay trace.
    cursors: Vec<ReplayCursor>,
    /// The PMU event type used during recording.
    break_on: perf::PmuEvent,
    /// Minimum timeslice from the recording scenario.
    ///
    /// Required for two PRNG sync points:
    /// 1. `rearm_timer()` at kfunc boundaries (cooperative yields consume
    ///    one PRNG via `roll_timeslice`; the result is discarded in replay
    ///    mode but the consumption must match recording).
    /// 2. `build_target()` consumes one PRNG to match recording's
    ///    `PmuBackend::build_target()` / `arm()` sequence.
    ///
    /// Cannot be removed: without matching PRNG consumption, `pick_next()`
    /// returns different worker IDs and replay diverges.
    timeslice_min: u64,
    /// Maximum timeslice from the recording scenario (see `timeslice_min`).
    timeslice_max: u64,
    /// Skip PMU timer and use hardware breakpoint stepping only.
    ///
    /// When true, the PMU timer is not armed and the hardware breakpoint
    /// fires on every execution of the target instruction. The RBC count
    /// is checked in the breakpoint handler to find the right dynamic
    /// instance. Slower but deterministic (no PMU skid).
    no_pmu_signal: bool,
}

/// Per-worker state for the replay backend.
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
pub(crate) struct ReplayWorkerCtx {
    timer: Option<perf::RbcTimer>,
    timer_fd: RawFd,
    bp_fd: RawFd,
    worker_idx: usize,
}

impl ReplayBackend {
    /// Create a new replay backend from a recorded preemption trace.
    ///
    /// Builds per-worker cursors from the trace, one per dispatch CPU.
    /// `timeslice_min` / `timeslice_max` must match the recording scenario's
    /// preemptive config to keep the PRNG sequence in sync.
    pub fn new(
        trace: &PreemptionTrace,
        num_workers: usize,
        timeslice_min: u64,
        timeslice_max: u64,
        no_pmu_signal: bool,
    ) -> Self {
        let cursors = (0..num_workers)
            .map(|i| {
                let targets = trace.worker_trace(WorkerId(i)).to_vec();
                ReplayCursor::new(targets)
            })
            .collect();
        ReplayBackend {
            cursors,
            break_on: trace.break_on(),
            timeslice_min,
            timeslice_max,
            no_pmu_signal,
        }
    }

    /// Whether this backend is in breakpoint-only mode.
    #[allow(dead_code)] // Callers temporarily removed during dispatch refactor
    pub fn no_pmu_signal(&self) -> bool {
        self.no_pmu_signal
    }

    /// Create a copy of this backend with `no_pmu_signal` forced on.
    ///
    /// Used by the retry logic when PMU signal attempts are exhausted
    /// and we fall back to breakpoint-only mode.
    #[allow(dead_code)] // Callers temporarily removed during dispatch refactor
    pub fn with_bp_only(&self) -> Self {
        ReplayBackend {
            cursors: self
                .cursors
                .iter()
                .map(|c| ReplayCursor::new(c.clone_targets()))
                .collect(),
            break_on: self.break_on,
            timeslice_min: self.timeslice_min,
            timeslice_max: self.timeslice_max,
            no_pmu_signal: true,
        }
    }

    /// Reset all cursors to the beginning for a retry attempt.
    #[allow(dead_code)] // Callers temporarily removed during dispatch refactor
    pub fn reset_cursors(&self) {
        for c in &self.cursors {
            c.reset();
        }
    }
}

impl PreemptionBackend for ReplayBackend {
    type WorkerCtx = ReplayWorkerCtx;

    fn global_setup(&self) {
        if self.no_pmu_signal {
            preempt::install_replay_bp_only_handlers();
        } else {
            preempt::install_replay_signal_handlers();
        }
    }

    fn global_teardown(&self) {
        if self.no_pmu_signal {
            preempt::uninstall_replay_bp_only_handlers();
        } else {
            preempt::uninstall_replay_signal_handlers();
        }
    }

    fn worker_setup(
        &self,
        ring: &PreemptRing,
        engine: &EngineRing,
        worker_id: WorkerId,
    ) -> ReplayWorkerCtx {
        let i = worker_id.0;
        let cursor = &self.cursors[i];

        // Create per-thread PMU timer.
        let (timer, timer_fd) = setup_pmu_timer(false, self.break_on);

        // In normal mode, the PMU timer is required to approach the
        // target RBC count before arming the hardware breakpoint.
        // In breakpoint-only mode, we still create the timer for RBC
        // measurement (read_rbc_count) but do not arm it for signals.
        if timer_fd < 0 && !self.no_pmu_signal {
            panic!(
                "replay: PMU timer unavailable on worker {i}. \
                 Replay mode requires PMU counters to reproduce preemption \
                 points. This environment (VM, container, or missing perf \
                 permissions) cannot support replay. Use --preemptive \
                 (non-replay) mode instead, or use --no-pmu-signal for \
                 breakpoint-only replay."
            );
        }

        // Create per-thread hardware breakpoint (at dummy addr 0x1).
        let bp_fd = {
            let bp = perf::try_create_hw_breakpoint(0x1);
            match bp {
                Some(b) => {
                    let fd = b.raw_fd();
                    // Leak the bp so the fd stays open; we manage
                    // the fd lifetime manually via raw ioctls.
                    std::mem::forget(b);
                    fd
                }
                None => {
                    // Fatal: replay requires HW breakpoints to catch the
                    // exact instruction pointer at the recorded preemption
                    // point. Without breakpoints, replay CANNOT reproduce
                    // preemption points and will produce wrong results.
                    panic!(
                        "replay: HW breakpoint unavailable on worker {i}. \
                         Replay mode requires hardware breakpoints (perf \
                         hw_breakpoint) to catch exact preemption instruction \
                         pointers. This environment (VM, container, or \
                         missing perf permissions) cannot support replay."
                    );
                }
            }
        };

        // Both timer_fd and bp_fd are guaranteed valid at this point
        // (we panic above if either is unavailable).
        debug!(
            worker = i,
            targets = cursor.len(),
            "replay: PMU + breakpoint armed"
        );

        // Install PREEMPT_CTX for cooperative yields at kfunc boundaries.
        // This enables pause_timer/resume_timer and maybe_yield_preemptive
        // which are essential for reproducing the same interleaving pattern.
        // The replay_mode flag prevents rearm_timer from overwriting the
        // replay timer period with random timeslices.
        preempt::install_replay_preempt(
            ring,
            engine,
            worker_id,
            timer_fd,
            self.timeslice_min,
            self.timeslice_max,
        );

        // Install replay context (replaces normal preempt context).
        preempt::install_replay(
            ring,
            engine,
            worker_id,
            timer_fd,
            bp_fd,
            cursor,
            self.no_pmu_signal,
        );

        ReplayWorkerCtx {
            timer,
            timer_fd,
            bp_fd,
            worker_idx: i,
        }
    }

    fn build_target(&self, ctx: &ReplayWorkerCtx, ring: &PreemptRing) -> Option<PreemptTarget> {
        // Consume the PRNG to match the recording's PmuBackend::arm() which
        // calls roll_timeslice. Without this, the PRNG sequences diverge
        // and pick_next returns different worker IDs.
        let _timeslice = ring.roll_timeslice(self.timeslice_min, self.timeslice_max);

        let cursor = &self.cursors[ctx.worker_idx];
        let first = cursor.current_target()?;

        if self.no_pmu_signal || first.structop_rbc < REPLAY_MARGIN {
            // Breakpoint-only mode, or target is too close for PMU to fire
            // before overshooting: use Relative(0) so the breakpoint handler
            // checks RBC on each hit to find the right dynamic instance.
            Some(PreemptTarget {
                count_rbc: RbcTarget::Relative(RelativeRbc(0)),
                target_rip: Some(first.instruction_pointer),
            })
        } else {
            // Normal mode: use the absolute structop RBC count from
            // the recorded trace. The PMU timer fires near this count,
            // then the breakpoint catches the exact RIP.
            Some(PreemptTarget {
                count_rbc: RbcTarget::Absolute(AbsoluteRbc(first.structop_rbc)),
                target_rip: Some(first.instruction_pointer),
            })
        }
    }

    fn arm(&self, ctx: &mut ReplayWorkerCtx, target: PreemptTarget) {
        match target.count_rbc {
            RbcTarget::Absolute(AbsoluteRbc(structop_rbc)) => {
                // Normal mode: arm the PMU timer to fire near the
                // target's cumulative RBC (structop_rbc).
                if ctx.timer_fd >= 0 && ctx.bp_fd >= 0 {
                    preempt::arm_replay_timer_pub(ctx.timer_fd, structop_rbc);
                }
            }
            RbcTarget::Relative(RelativeRbc(0)) => {
                // Breakpoint-only mode: arm the HW breakpoint directly
                // at the target RIP. The breakpoint handler checks the
                // RBC count on each hit to find the right instance.
                if let Some(rip) = target.target_rip {
                    if ctx.bp_fd >= 0 {
                        preempt::arm_replay_breakpoint_pub(ctx.bp_fd, rip);
                    }
                }
            }
            RbcTarget::Relative(RelativeRbc(n)) => {
                panic!(
                    "ReplayBackend::arm() received unexpected \
                     RbcTarget::Relative({n}) -- expected Absolute or Relative(0)"
                );
            }
        }
    }

    fn disarm(&self, ctx: &mut ReplayWorkerCtx) -> StructopDelta {
        // Disable timer.
        if let Some(ref t) = ctx.timer {
            let _ = t.disable();
        }
        // Disable and close breakpoint fd.
        if ctx.bp_fd >= 0 {
            // SAFETY: `bp_fd` is a valid perf_event fd obtained from
            // `perf_event_open`. PERF_IOC_DISABLE is a valid ioctl.
            unsafe {
                libc::ioctl(ctx.bp_fd, scx_perf::PERF_IOC_DISABLE, 0 as libc::c_ulong);
            }
        }

        StructopDelta {
            rbc_total: 0,
            interleave_count: preempt::structop_info().interleave_count,
        }
    }

    fn worker_teardown(&self, ctx: ReplayWorkerCtx) {
        // Close breakpoint fd (was leaked from HwBreakpoint via forget).
        if ctx.bp_fd >= 0 {
            // SAFETY: `bp_fd` is a valid fd; closed exactly once here.
            unsafe { libc::close(ctx.bp_fd) };
        }
        preempt::uninstall_replay();
        preempt::uninstall();
        // timer dropped here — closes the perf fd
    }

    fn log_completion(&self, ring: &PreemptRing) {
        let mut total_targets: usize = 0;
        let mut total_consumed: usize = 0;
        for (i, cursor) in self.cursors.iter().enumerate() {
            let consumed = cursor.consumed();
            let len = cursor.len();
            total_targets += len;
            total_consumed += consumed;
            if consumed < len {
                tracing::warn!(
                    worker = i,
                    consumed,
                    total = len,
                    skipped = len - consumed,
                    "replay: worker did not consume all targets"
                );
            }
        }
        debug!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            consumed = total_consumed,
            total = total_targets,
            "replay interleave: complete — consumed {}/{} targets",
            total_consumed,
            total_targets,
        );
    }

    fn is_precise(&self) -> bool {
        true
    }

    fn read_count(&self, ctx: &ReplayWorkerCtx) -> u64 {
        ctx.timer.as_ref().and_then(|t| t.read().ok()).unwrap_or(0)
    }

    fn reset_count(&self, ctx: &mut ReplayWorkerCtx) {
        if let Some(ref t) = ctx.timer {
            let _ = t.reset();
        }
    }
}
