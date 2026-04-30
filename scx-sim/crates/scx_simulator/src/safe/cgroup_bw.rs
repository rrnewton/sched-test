//! Cgroup CPU bandwidth enforcement (cpu.max simulation).
//!
//! This module implements the kernel's CFS bandwidth controller logic for the
//! simulator. When a cgroup has `cpu.max` configured (quota/period), the engine
//! tracks CPU time consumed by tasks in that cgroup and throttles them when
//! the quota is exhausted for the current period.
//!
//! # Kernel behavior modeled
//!
//! - Per-cgroup runtime accounting: charged when tasks stop running
//! - Throttling: when `runtime_remaining_ns <= 0`, all tasks in the cgroup
//!   are prevented from being scheduled
//! - Period refill: a periodic timer restores the quota and unthrottles
//! - Hierarchical enforcement: a task is throttled if ANY ancestor is throttled
//!
//! # Not yet modeled
//!
//! - Burst: stored quota from unused periods (always 0 for now)
//! - Slack reclaim: returning unused runtime to the pool early

use std::collections::HashMap;

use tracing::debug;

use crate::cgroup::CgroupId;
use crate::types::{Pid, TimeNs};

/// Per-cgroup CPU bandwidth state for cpu.max enforcement.
///
/// Tracks the runtime budget, throttle state, and period timing for a single
/// cgroup. Only cgroups with a finite quota have an entry — unlimited cgroups
/// (`quota = "max"`) are not tracked.
#[derive(Debug, Clone)]
pub struct CgroupBandwidthState {
    /// Configured quota per period (ns). Stored in ns for precision even though
    /// cpu.max uses microseconds, because the simulator's clock is nanosecond-
    /// granularity and rounding errors accumulate over many periods.
    pub quota_ns: u64,
    /// Period length (ns).
    pub period_ns: u64,
    /// Remaining runtime in current period (ns). Can go negative when a task
    /// runs past the boundary — the deficit is carried as a charge against the
    /// next check but does not reduce future periods' quota.
    pub runtime_remaining_ns: i64,
    /// Whether tasks in this cgroup are currently throttled.
    pub throttled: bool,
    /// Start time of the current period (ns).
    pub period_start_ns: TimeNs,
    /// PIDs of tasks that were runnable when throttling kicked in.
    /// These tasks need to be re-enqueued when the cgroup is unthrottled.
    pub throttled_pids: Vec<Pid>,
}

impl CgroupBandwidthState {
    /// Create a new bandwidth state with a full quota.
    ///
    /// # Arguments
    /// * `quota_us` - Quota per period in microseconds
    /// * `period_us` - Period length in microseconds
    /// * `now_ns` - Current simulation time (start of first period)
    pub fn new(quota_us: u64, period_us: u64, now_ns: TimeNs) -> Self {
        let quota_ns = quota_us * 1_000;
        let period_ns = period_us * 1_000;
        Self {
            quota_ns,
            period_ns,
            runtime_remaining_ns: quota_ns as i64,
            throttled: false,
            period_start_ns: now_ns,
            throttled_pids: Vec::new(),
        }
    }

    /// Charge `delta_ns` of CPU time against this cgroup's quota.
    ///
    /// Returns `true` if the cgroup should be throttled (runtime exhausted).
    pub fn charge(&mut self, delta_ns: u64) -> bool {
        self.runtime_remaining_ns -= delta_ns as i64;
        self.runtime_remaining_ns <= 0 && !self.throttled
    }

    /// Refill the quota for a new period.
    ///
    /// Resets `runtime_remaining_ns` to the full quota. If the cgroup was
    /// throttled, clears the throttle flag and returns the list of PIDs
    /// that should be re-enqueued.
    ///
    /// # Arguments
    /// * `now_ns` - Current simulation time (new period start)
    pub fn refill(&mut self, now_ns: TimeNs) -> Vec<Pid> {
        self.period_start_ns = now_ns;
        self.runtime_remaining_ns = self.quota_ns as i64;

        if self.throttled {
            self.throttled = true; // will be cleared by caller after re-enqueue
            debug!(
                quota_ns = self.quota_ns,
                throttled_tasks = self.throttled_pids.len(),
                "cgroup bandwidth refill: unthrottling"
            );
            std::mem::take(&mut self.throttled_pids)
        } else {
            Vec::new()
        }
    }

    /// Mark this cgroup as throttled and record a task PID.
    pub fn throttle(&mut self, pid: Pid) {
        self.throttled = true;
        if !self.throttled_pids.contains(&pid) {
            self.throttled_pids.push(pid);
        }
    }

    /// Unthrottle this cgroup (called after refill + re-enqueue).
    pub fn unthrottle(&mut self) {
        self.throttled = false;
        self.throttled_pids.clear();
    }

    /// Time remaining until the next period boundary (ns).
    pub fn time_until_refill(&self, now_ns: TimeNs) -> TimeNs {
        let period_end = self.period_start_ns + self.period_ns;
        period_end.saturating_sub(now_ns)
    }

    /// How much CPU time (ns) this cgroup can still use before hitting the
    /// quota limit. Returns 0 if already exhausted.
    pub fn budget_remaining_ns(&self) -> u64 {
        self.runtime_remaining_ns.max(0) as u64
    }

    /// Utilization: fraction of quota consumed in the current period.
    pub fn utilization(&self) -> f64 {
        let consumed = self.quota_ns as i64 - self.runtime_remaining_ns;
        consumed.max(0) as f64 / self.quota_ns as f64
    }
}

/// Manager for all cgroup bandwidth states.
///
/// Provides a central place for the engine to:
/// - Initialize bandwidth when `cgroup_set_bandwidth` is called
/// - Charge runtime when tasks stop
/// - Check throttle state before scheduling
/// - Schedule and process refill timers
pub struct BandwidthManager {
    /// Per-cgroup bandwidth state. Only cgroups with finite quota have entries.
    states: HashMap<CgroupId, CgroupBandwidthState>,
}

impl BandwidthManager {
    /// Create an empty bandwidth manager (no cgroups tracked).
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Configure bandwidth for a cgroup.
    ///
    /// If `quota_us` is 0 or the period is 0, the entry is removed (unlimited).
    /// Called when the engine processes `cgroup_set_bandwidth`.
    pub fn configure(&mut self, cgid: CgroupId, period_us: u64, quota_us: u64, now_ns: TimeNs) {
        if quota_us == 0 || period_us == 0 {
            self.states.remove(&cgid);
            return;
        }
        debug!(
            cgid = cgid.0,
            quota_us, period_us, "cgroup bandwidth: configured"
        );
        self.states
            .insert(cgid, CgroupBandwidthState::new(quota_us, period_us, now_ns));
    }

    /// Remove bandwidth tracking for a cgroup (e.g., on destroy).
    pub fn remove(&mut self, cgid: CgroupId) {
        self.states.remove(&cgid);
    }

    /// Check if a cgroup (or any of its ancestors) is currently throttled.
    ///
    /// # Arguments
    /// * `cgid` - The cgroup to check
    /// * `ancestor_lookup` - Closure that returns the parent cgroup ID, or `None` for root
    pub fn is_throttled(
        &self,
        cgid: CgroupId,
        ancestor_lookup: impl Fn(CgroupId) -> Option<CgroupId>,
    ) -> bool {
        let mut current = Some(cgid);
        while let Some(cg) = current {
            if let Some(state) = self.states.get(&cg) {
                if state.throttled {
                    return true;
                }
            }
            current = ancestor_lookup(cg);
        }
        false
    }

    /// Charge CPU time to a cgroup and all its ancestors.
    ///
    /// Returns the ID of the first cgroup whose quota was exhausted (if any),
    /// along with whether it was newly exhausted (not already throttled).
    ///
    /// # Arguments
    /// * `cgid` - The cgroup the task belongs to
    /// * `delta_ns` - CPU time consumed (ns)
    /// * `ancestor_lookup` - Closure returning parent cgroup ID
    pub fn charge(
        &mut self,
        cgid: CgroupId,
        delta_ns: u64,
        ancestor_lookup: impl Fn(CgroupId) -> Option<CgroupId>,
    ) -> Option<CgroupId> {
        if delta_ns == 0 {
            return None;
        }

        let mut newly_exhausted = None;
        let mut current = Some(cgid);
        while let Some(cg) = current {
            if let Some(state) = self.states.get_mut(&cg) {
                if state.charge(delta_ns) && newly_exhausted.is_none() {
                    newly_exhausted = Some(cg);
                }
            }
            current = ancestor_lookup(cg);
        }
        newly_exhausted
    }

    /// Get the bandwidth state for a cgroup (if tracked).
    pub fn get(&self, cgid: CgroupId) -> Option<&CgroupBandwidthState> {
        self.states.get(&cgid)
    }

    /// Get mutable bandwidth state for a cgroup (if tracked).
    pub fn get_mut(&mut self, cgid: CgroupId) -> Option<&mut CgroupBandwidthState> {
        self.states.get_mut(&cgid)
    }

    /// Process a period refill for a specific cgroup.
    ///
    /// Returns the list of PIDs that should be re-enqueued (were throttled).
    pub fn refill(&mut self, cgid: CgroupId, now_ns: TimeNs) -> Vec<Pid> {
        if let Some(state) = self.states.get_mut(&cgid) {
            let pids = state.refill(now_ns);
            state.unthrottle();
            pids
        } else {
            Vec::new()
        }
    }

    /// Compute the maximum run time (ns) a task can execute before any of its
    /// cgroup's bandwidth limits are hit.
    ///
    /// Returns `None` if no bandwidth limit applies (unlimited run time).
    /// Returns `Some(0)` if the cgroup is already throttled.
    pub fn max_run_ns(
        &self,
        cgid: CgroupId,
        ancestor_lookup: impl Fn(CgroupId) -> Option<CgroupId>,
    ) -> Option<u64> {
        let mut min_budget: Option<u64> = None;
        let mut current = Some(cgid);
        while let Some(cg) = current {
            if let Some(state) = self.states.get(&cg) {
                if state.throttled {
                    return Some(0);
                }
                let budget = state.budget_remaining_ns();
                min_budget = Some(match min_budget {
                    Some(prev) => prev.min(budget),
                    None => budget,
                });
            }
            current = ancestor_lookup(cg);
        }
        min_budget
    }

    /// Iterator over all tracked cgroup IDs and their states.
    pub fn iter(&self) -> impl Iterator<Item = (&CgroupId, &CgroupBandwidthState)> {
        self.states.iter()
    }

    /// Number of cgroups with bandwidth tracking.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether any cgroups have bandwidth tracking.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

impl Default for BandwidthManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_parent(_: CgroupId) -> Option<CgroupId> {
        None
    }

    #[test]
    fn test_new_state_has_full_quota() {
        let state = CgroupBandwidthState::new(200_000, 100_000, 0);
        assert_eq!(state.quota_ns, 200_000_000); // 200ms in ns
        assert_eq!(state.period_ns, 100_000_000); // 100ms in ns
        assert_eq!(state.runtime_remaining_ns, 200_000_000);
        assert!(!state.throttled);
        assert!(state.throttled_pids.is_empty());
    }

    #[test]
    fn test_charge_within_quota() {
        let mut state = CgroupBandwidthState::new(200_000, 100_000, 0);
        // Charge 50ms — should not exhaust
        let exhausted = state.charge(50_000_000);
        assert!(!exhausted);
        assert_eq!(state.runtime_remaining_ns, 150_000_000);
        assert!(!state.throttled);
    }

    #[test]
    fn test_charge_exhausts_quota() {
        let mut state = CgroupBandwidthState::new(200_000, 100_000, 0);
        // Charge 250ms — exceeds 200ms quota
        let exhausted = state.charge(250_000_000);
        assert!(exhausted);
        assert_eq!(state.runtime_remaining_ns, -50_000_000);
    }

    #[test]
    fn test_refill_restores_quota() {
        let mut state = CgroupBandwidthState::new(200_000, 100_000, 0);
        state.charge(250_000_000);
        state.throttle(Pid(1));
        state.throttle(Pid(2));
        assert!(state.throttled);

        let pids = state.refill(100_000_000);
        assert_eq!(pids.len(), 2);
        assert!(pids.contains(&Pid(1)));
        assert!(pids.contains(&Pid(2)));

        state.unthrottle();
        assert!(!state.throttled);
        assert_eq!(state.runtime_remaining_ns, 200_000_000);
        assert!(state.throttled_pids.is_empty());
    }

    #[test]
    fn test_manager_configure_and_charge() {
        let mut mgr = BandwidthManager::new();
        let cg = CgroupId(10);
        mgr.configure(cg, 100_000, 200_000, 0);
        assert_eq!(mgr.len(), 1);

        // Charge 100ms — within quota
        let exhausted = mgr.charge(cg, 100_000_000, no_parent);
        assert!(exhausted.is_none());

        // Charge another 150ms — exceeds quota
        let exhausted = mgr.charge(cg, 150_000_000, no_parent);
        assert_eq!(exhausted, Some(cg));
    }

    #[test]
    fn test_manager_hierarchical_throttle() {
        let mut mgr = BandwidthManager::new();
        let parent = CgroupId(10);
        let child = CgroupId(20);

        // Parent: 100ms quota, child: 50ms quota
        mgr.configure(parent, 100_000, 100_000, 0);
        mgr.configure(child, 100_000, 50_000, 0);

        let ancestor = |cg: CgroupId| -> Option<CgroupId> {
            if cg == child {
                Some(parent)
            } else {
                None
            }
        };

        // Charge 60ms to child — child exhausted (50ms quota), parent still has 40ms
        let exhausted = mgr.charge(child, 60_000_000, ancestor);
        assert_eq!(exhausted, Some(child));

        // Child should be throttleable
        assert!(!mgr.is_throttled(child, ancestor)); // not marked yet, just exhausted
    }

    #[test]
    fn test_manager_max_run_ns() {
        let mut mgr = BandwidthManager::new();
        let parent = CgroupId(10);
        let child = CgroupId(20);

        mgr.configure(parent, 100_000, 300_000, 0); // 300ms quota
        mgr.configure(child, 100_000, 100_000, 0); // 100ms quota

        let ancestor = |cg: CgroupId| -> Option<CgroupId> {
            if cg == child {
                Some(parent)
            } else {
                None
            }
        };

        // Max run = min(child's 100ms, parent's 300ms) = 100ms
        let max = mgr.max_run_ns(child, ancestor);
        assert_eq!(max, Some(100_000_000));

        // After charging 80ms, max = min(20ms, 220ms) = 20ms
        mgr.charge(child, 80_000_000, ancestor);
        let max = mgr.max_run_ns(child, ancestor);
        assert_eq!(max, Some(20_000_000));
    }

    #[test]
    fn test_manager_unlimited_cgroup() {
        let mgr = BandwidthManager::new();
        let cg = CgroupId(10);

        // Not configured = unlimited
        assert!(!mgr.is_throttled(cg, no_parent));
        assert_eq!(mgr.max_run_ns(cg, no_parent), None);
    }

    #[test]
    fn test_manager_remove() {
        let mut mgr = BandwidthManager::new();
        let cg = CgroupId(10);
        mgr.configure(cg, 100_000, 200_000, 0);
        assert_eq!(mgr.len(), 1);

        mgr.remove(cg);
        assert_eq!(mgr.len(), 0);
    }

    #[test]
    fn test_time_until_refill() {
        let state = CgroupBandwidthState::new(200_000, 100_000, 50_000_000);
        // Period started at 50ms, period is 100ms, so refill at 150ms
        assert_eq!(state.time_until_refill(50_000_000), 100_000_000);
        assert_eq!(state.time_until_refill(100_000_000), 50_000_000);
        assert_eq!(state.time_until_refill(150_000_000), 0);
    }

    #[test]
    fn test_utilization() {
        let mut state = CgroupBandwidthState::new(100_000, 100_000, 0);
        assert!((state.utilization() - 0.0).abs() < f64::EPSILON);

        state.charge(50_000_000); // 50% of 100ms quota
        assert!((state.utilization() - 0.5).abs() < 0.001);

        state.charge(50_000_000); // 100%
        assert!((state.utilization() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_configure_zero_removes() {
        let mut mgr = BandwidthManager::new();
        let cg = CgroupId(10);
        mgr.configure(cg, 100_000, 200_000, 0);
        assert_eq!(mgr.len(), 1);

        // Zero quota = unlimited = remove
        mgr.configure(cg, 100_000, 0, 0);
        assert_eq!(mgr.len(), 0);
    }
}
