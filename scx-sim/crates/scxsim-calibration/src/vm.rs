//! Reading the live half: ktstr's stats sidecar.
//!
//! ktstr writes one JSON sidecar per test run, under
//! `$CARGO_TARGET_DIR/ktstr/<kernel>-<commit>/`. This module reads the subset a
//! calibration compares against and — the part that carries the weight —
//! distinguishes *not measured* from *measured as zero*.
//!
//! # Why the `*_measured` flags are the important field
//!
//! `CgroupStats` always contains `p99_wake_latency_us`, and on a run that never
//! sampled wake latency it contains `0.0`. A reader that takes the number at
//! face value compares the simulator against a fabricated zero and reports
//! whatever comes out. The flag beside it, `wake_measured: false`, is what says
//! the field is a placeholder.
//!
//! So every accessor here returns [`Option`], `None` meaning *the live run did
//! not capture this*, which [`crate::verdict::compare`] turns into
//! [`Verdict::NotMeasured`](crate::Verdict) rather than agreement. That is the
//! whole point: a metric nobody measured must not be able to pass.

use serde::Deserialize;

use crate::units::{DurationNs, Ratio};

/// One ktstr run's sidecar.
///
/// Deliberately a *subset*: unknown fields are ignored, so a ktstr-side schema
/// addition does not break parsing. The fields named here are the ones a
/// verdict depends on, and a rename of any of them SHOULD break the build
/// rather than silently read as absent — which is why they are not
/// `#[serde(default)]`.
#[derive(Debug, Clone, Deserialize)]
pub struct VmRun {
    pub test_name: String,
    /// e.g. `1n1l2c1t`.
    pub topology: String,
    /// The BPF scheduler the guest ran. NOT necessarily the one the simulator
    /// runs — see [`VmRun::scheduler`] callers.
    pub scheduler: String,
    /// ktstr commit the guest binary was built from.
    pub project_commit: String,
    pub passed: bool,
    /// Guest vCPU count. The workload's CPU budget, and the denominator of
    /// occupancy.
    pub vcpus: u32,
    pub stats: VmStats,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VmStats {
    pub cgroups: Vec<VmCgroup>,
    pub total_workers: u32,
    pub total_migrations: u64,
}

/// Per-cgroup live observations.
///
/// Only the quantities with a simulator counterpart are pulled out. Fields
/// ktstr emits that have no counterpart (page locality, iteration counts,
/// taobench) are deliberately absent: inventing a correspondence for them would
/// manufacture agreement instead of testing for it.
#[derive(Debug, Clone, Deserialize)]
pub struct VmCgroup {
    pub cgroup_name: String,
    pub num_workers: u32,
    /// Summed across this cgroup's workers.
    pub total_cpu_time_ns: u64,
    /// `off_cpu_ns / wall_time_ns * 100`, averaged over the cgroup's workers.
    /// A PERCENTAGE, not a fraction — `0.358` means 0.358%.
    pub avg_off_cpu_pct: f64,
    /// schedstat run_delay: runnable-but-not-running, in microseconds.
    ///
    /// Mean over the cgroup's WORKERS of each worker's whole-run delta in
    /// `task->sched_info.run_delay` — a per-worker TOTAL, not a per-dispatch
    /// mean. See [`Self::run_delay`] for the full definition and for the
    /// simulator quantity it is matched against.
    ///
    /// It is also the EVIDENCE for
    /// [`Metric::OffCpuTime`](crate::report::Metric::OffCpuTime) being ruled
    /// not-comparable: it isolates the scheduler-attributable share of off-CPU
    /// time, and on the one run we have that share is 8-17%. Without it the
    /// classification would be an assertion rather than a measurement.
    #[serde(default)]
    pub mean_run_delay_us: f64,
    /// Whether run_delay is a reading. Read via [`Self::run_delay`].
    ///
    /// A WEAKER predicate than `wake_measured`, and the difference matters.
    /// `wake_measured` is "did any wakeup get sampled". This is
    /// `!run_delays.is_empty()`, i.e. "did this cgroup have a worker at all"
    /// (ktstr `assert/reductions.rs`). That is the correct predicate for this
    /// field and not laxity: `sched_info.run_delay` accumulates whenever
    /// `CONFIG_SCHED_INFO` is compiled in — forced on in ktstr, and not gated by
    /// the runtime `kernel.sched_schedstats` key — so there is exactly one value
    /// per worker on every run, and a worker that never queued reads a genuine
    /// measured `0.0` rather than a placeholder. A cgroup with no workers is the
    /// only way to get a sentinel, and that is what this excludes.
    #[serde(default)]
    pub run_delay_measured: bool,
    /// Longest observed gap between the worker's own iterations, in ms.
    ///
    /// Also evidence, not a comparison quantity: it distinguishes "one long
    /// stall" (a scheduling event) from "finely distributed overhead", and the
    /// off-CPU classification depends on which of those the missing time is.
    #[serde(default)]
    pub max_gap_ms: u64,
    pub total_migrations: u64,
    /// Placeholder unless [`Self::wake_measured`]. Read via
    /// [`Self::wake_latency_p99`], never directly.
    pub p99_wake_latency_us: f64,
    /// Placeholder unless [`Self::wake_measured`].
    pub median_wake_latency_us: f64,
    /// Whether the wake-latency fields hold a measurement at all.
    pub wake_measured: bool,
}

impl VmCgroup {
    /// CPU time. Always measured.
    pub fn cpu_time(&self) -> DurationNs {
        DurationNs(self.total_cpu_time_ns)
    }

    /// Off-CPU time as a FRACTION of wall, converted from ktstr's percentage.
    ///
    /// The conversion is here, once, rather than at each call site: a stray
    /// factor of 100 would put the simulator ~100x off and read as a
    /// spectacular fidelity gap instead of as the unit bug it is.
    pub fn off_cpu_fraction(&self) -> Ratio {
        Ratio(self.avg_off_cpu_pct / 100.0)
    }

    /// Off-CPU time in ns, derived against a wall duration.
    pub fn off_cpu_time(&self, wall: DurationNs) -> DurationNs {
        DurationNs((wall.as_nanos() as f64 * self.off_cpu_fraction().get()) as u64)
    }

    /// Scheduling delay: mean over the cgroup's workers of each worker's TOTAL
    /// runnable-but-not-running time. `None` when the cgroup had no worker.
    ///
    /// The kernel's `task->sched_info.run_delay`, read per worker from
    /// `/proc/self/task/<tid>/schedstat` field 2 and differenced across the run.
    /// It accumulates in `sched_info_arrive()` at each dispatch as
    /// `now - last_queued`, with `last_queued` stamped by `sched_info_enqueue()`
    /// on every enqueue including the re-enqueue of a preempted task — so it is
    /// the summed length of every interval the worker spent on a runqueue in
    /// TASK_RUNNING state without the CPU.
    ///
    /// Matched against [`SimRun::run_delay`](crate::sim::SimRun::run_delay),
    /// which sets out the two remaining definitional differences and measures
    /// the size of the one that can be measured. Note that this side, being a
    /// real machine, includes waiting caused by everything else on it: kernel
    /// threads, IRQ work, the test harness. The simulator has none of those.
    pub fn run_delay(&self) -> Option<DurationNs> {
        self.run_delay_measured
            .then_some(DurationNs((self.mean_run_delay_us * 1_000.0) as u64))
    }

    /// p99 wake latency, or `None` when the run did not measure it.
    pub fn wake_latency_p99(&self) -> Option<DurationNs> {
        self.wake_measured
            .then_some(DurationNs((self.p99_wake_latency_us * 1_000.0) as u64))
    }

    /// Median wake latency, or `None` when the run did not measure it.
    pub fn wake_latency_median(&self) -> Option<DurationNs> {
        self.wake_measured
            .then_some(DurationNs((self.median_wake_latency_us * 1_000.0) as u64))
    }
}

impl VmRun {
    /// Parse a sidecar.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Total CPU time across all cgroups.
    pub fn total_cpu_time(&self) -> DurationNs {
        DurationNs(self.stats.cgroups.iter().map(|c| c.total_cpu_time_ns).sum())
    }

    /// Occupancy: busy CPU time over available CPU time.
    ///
    /// Dimensionless, so it is the one metric immune to a unit-conversion
    /// mistake on either side — which is why it carries the tightest
    /// pre-registered bound.
    pub fn occupancy(&self, wall: DurationNs) -> Ratio {
        let capacity = wall.as_nanos() as f64 * self.vcpus as f64;
        Ratio(self.total_cpu_time().as_nanos() as f64 / capacity)
    }

    /// Whether ANY cgroup measured wake latency.
    ///
    /// False here means the metric is [`NotMeasured`](crate::Verdict) for the
    /// whole run, however many cgroups it has.
    pub fn wake_latency_measured(&self) -> bool {
        self.stats.cgroups.iter().any(|c| c.wake_measured)
    }

    pub fn cgroup(&self, name: &str) -> Option<&VmCgroup> {
        self.stats.cgroups.iter().find(|c| c.cgroup_name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed fixture, so these tests exercise the real schema rather
    /// than a hand-written approximation of it.
    const FIXTURE: &str =
        include_str!("../vm_runs/sched_basic_proportional-6.14.11-85c72e1.ktstr.json");

    fn fixture() -> VmRun {
        VmRun::from_json(FIXTURE).expect("the committed sidecar must parse")
    }

    #[test]
    fn parses_the_real_sidecar() {
        let run = fixture();
        assert_eq!(run.test_name, "sched_basic_proportional");
        assert_eq!(run.topology, "1n1l2c1t");
        assert_eq!(run.scheduler, "ktstr_sched");
        assert_eq!(run.project_commit, "85c72e1");
        assert!(run.passed);
        assert_eq!(run.vcpus, 2);
        assert_eq!(run.stats.cgroups.len(), 2);
        assert_eq!(run.stats.total_workers, 2);
    }

    /// Run delay reads through, in ns, from a µs field.
    ///
    /// A missed factor of 1000 here would put the live side three orders of
    /// magnitude off and read as a spectacular fidelity gap rather than as the
    /// unit bug it is — the same trap `off_cpu_fraction` exists to close.
    #[test]
    fn run_delay_converts_microseconds_to_nanoseconds() {
        let run = fixture();
        let cg = run.cgroup("cg_0").expect("cg_0 is in the fixture");
        assert!(cg.run_delay_measured, "fixture premise: run delay measured");
        assert_eq!(cg.mean_run_delay_us, 3694.068);
        assert_eq!(cg.run_delay(), Some(DurationNs(3_694_068)));
    }

    /// A cgroup with no worker yields no run-delay reading.
    ///
    /// Weaker than the wake-latency guard below, and deliberately so: unlike
    /// wake latency, `sched_info.run_delay` is populated on every ktstr run, so
    /// a real measured `0.0` is a legitimate value and must NOT be suppressed.
    /// The only sentinel is the worker-less cohort, and that is what this pins.
    /// Constructed rather than taken from the fixture because the fixture has no
    /// empty cgroup — which is also why this cannot be an assertion about it.
    #[test]
    fn run_delay_of_a_workerless_cgroup_is_none_but_a_measured_zero_is_some() {
        let mut cg = fixture().stats.cgroups.into_iter().next().unwrap();

        cg.run_delay_measured = false;
        cg.mean_run_delay_us = 0.0;
        assert_eq!(cg.run_delay(), None, "no worker: not a reading");

        cg.run_delay_measured = true;
        assert_eq!(
            cg.run_delay(),
            Some(DurationNs(0)),
            "a worker that never queued measured zero, and zero is the answer"
        );
    }

    /// THE guard this module exists for. The fixture has
    /// `p99_wake_latency_us: 0.0` sitting next to `wake_measured: false`; if
    /// the accessor ever returns `Some(0)` there, every wake-latency
    /// calibration silently compares against a number nobody measured.
    #[test]
    fn unmeasured_wake_latency_is_none_not_zero() {
        let run = fixture();
        for cg in &run.stats.cgroups {
            assert!(!cg.wake_measured, "fixture premise: wake was not measured");
            assert_eq!(cg.p99_wake_latency_us, 0.0, "the placeholder IS zero");
            assert_eq!(
                cg.wake_latency_p99(),
                None,
                "but it must not be readable as a measured zero"
            );
            assert_eq!(cg.wake_latency_median(), None);
        }
        assert!(!run.wake_latency_measured());
    }

    /// And the flag must actually gate — a run that DID measure must read
    /// through, or the guard above would be satisfied by an accessor that
    /// always returns None.
    #[test]
    fn measured_wake_latency_reads_through() {
        let mut run = fixture();
        run.stats.cgroups[0].wake_measured = true;
        run.stats.cgroups[0].p99_wake_latency_us = 250.0;
        assert_eq!(
            run.stats.cgroups[0].wake_latency_p99(),
            Some(DurationNs(250_000)),
            "250us is 250_000ns"
        );
        assert!(run.wake_latency_measured());
    }

    #[test]
    fn off_cpu_is_converted_from_percent_to_fraction() {
        let run = fixture();
        let cg = run.cgroup("cg_0").expect("cg_0");
        assert!((cg.avg_off_cpu_pct - 0.3582538).abs() < 1e-6, "raw is a %");
        assert!(
            (cg.off_cpu_fraction().get() - 0.003582538).abs() < 1e-9,
            "accessor yields a fraction"
        );
    }

    #[test]
    fn occupancy_of_two_saturated_spinners_on_two_cpus_is_about_one() {
        let run = fixture();
        let occ = run.occupancy(DurationNs::from_secs(12));
        assert!(
            (occ.get() - 1.0).abs() < 0.01,
            "two spinners on 2 cpus for 12s should be ~100% occupied, got {occ}"
        );
    }

    #[test]
    fn per_cgroup_lookup_and_totals() {
        let run = fixture();
        assert_eq!(
            run.cgroup("cg_0").unwrap().total_cpu_time_ns,
            12_010_668_525
        );
        assert_eq!(
            run.cgroup("cg_1").unwrap().total_cpu_time_ns,
            12_002_274_341
        );
        assert_eq!(run.cgroup("nope").map(|c| c.cgroup_name.as_str()), None);
        assert_eq!(run.total_cpu_time(), DurationNs(24_012_942_866));
        assert_eq!(run.stats.total_migrations, 16);
    }
}
