//! Smoking-gun observer for cgroup_bw library replenishment.
//!
//! tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun` (R2 HIGH
//! from wprof-trace-baseline 2026-05-13).
//!
//! The cpu-bw-stall-bug's CAUSE lives in `lib/cgroup_bw.bpf.c:1679` --
//! the `period_budget` debt accounting inside `cbw_replenish_cgroup`.
//! That state has NO kernel tracepoint and is INVISIBLE to wprof.
//! scxsim is the only place we can observe it.
//!
//! # Architecture (post-fire snapshot diff)
//!
//! The first attempt was a function-like macro wrapping
//! `cbw_replenish_cgroup` BEFORE the lib include in
//! `schedulers/lavd/wrapper.c`. That fails because the C preprocessor
//! expands the macro at the FUNCTION DEFINITION site inside the lib
//! too (the lib has `bool cbw_replenish_cgroup(struct scx_cgroup_ctx
//! *cgx, u64 now) { ... }` -- two-arg form matches the macro),
//! producing syntactic garbage. Splitting the lib include is not
//! possible with standard `#include`.
//!
//! Workaround: snapshot/diff. The wrapper exposes
//! `scxsim_cbw_snapshot_all_cgroups(out, max)` (default visibility,
//! dlsym'd) which fills `out` with `(cgid, runtime_total_last,
//! period_budget, burst_remaining, nquota, nquota_ub, is_throttled)`
//! for every finite-quota cgroup in `cbw_cgroup_ids[]`. The engine
//! calls this BEFORE every `fire_timer` and AGAIN AFTER. For each
//! cgroup whose `runtime_total_last` OR `period_budget` changed --
//! the two fields the lib's `replenish_timerfn` writes -- the engine
//! emits a `TraceKind::CgroupBwReplenish` event with `debt`,
//! `burst_credit`, `keep_throttled` computed in Rust using the same
//! formulas the lib uses (lib lines 1679-1706).
//!
//! Why the diff is faithful:
//!   * `runtime_total_last` is set by `replenish_timerfn` at lib line
//!     1894 BEFORE the per-cgroup `cbw_replenish_cgroup` call. The
//!     "AFTER" snapshot reads exactly the value the lib used as
//!     debt/burst-credit input.
//!   * `period_budget` is the OUTPUT of `cbw_replenish_cgroup` (lib
//!     line 1697). The "AFTER" snapshot reads it.
//!   * `burst_remaining`, `nquota`, `nquota_ub` only mutate at
//!     period boundaries / config changes; the BEFORE snapshot is
//!     correct as the lib's input.
//!   * `debt` and `burst_credit` are computed from the BEFORE/AFTER
//!     deltas using the formulas at lib lines 1679-1680.
//!   * `keep_throttled` = `period_budget_out <= 0` (lib line 1706).
//!
//! Coverage caveat: the engine fires a snapshot diff after EVERY
//! `fire_timer` invocation, but only emits events for cgroups whose
//! state actually changed. So an `accounting_timer` fire (which
//! does not write `period_budget` or `runtime_total_last`) produces
//! no events; only `replenish_timer` fires generate events. No
//! scheduler-internal slot-id knowledge is needed engine-side.

use crate::cgroup::CgroupId;
use crate::ffi::CbwCgroupSnapshot;
use crate::trace::TraceKind;

/// Maximum cgroups per snapshot. Matches the lib's `CBW_NR_CGRP_MAX`
/// (lib/cgroup_bw.bpf.c:271) so the snapshot can never miss a cgroup
/// the lib tracks. 2048 * 56 bytes = 112 KiB per snapshot.
pub const SNAPSHOT_CAPACITY: usize = 2048;

/// Snapshot the cgroup_bw lib state for every (cgid, raw cgrp ptr)
/// pair in `pairs`, invoking the supplied `query` per pair (typically
/// a `Scheduler::snapshot_by_raw_cgrp` dlsym call).
///
/// Returns:
///   * `None` -- the loaded scheduler does NOT link the cgroup_bw
///     library (the very first `query` returns `None`). The engine
///     interprets this as "observer inactive" and skips the AFTER
///     snapshot too. Cheap.
///   * `Some(vec)` -- the lib is active. `vec` contains one entry
///     per pair for which the lib has finite-quota state (`query`
///     returned `Some(0)`). Cgroups absent from the lib (`Some(-1)`,
///     `Some(-2)`) and unlimited-quota cgroups (`Some(-3)`) are
///     filtered out.
///
/// The "first query None → return None" early-exit means schedulers
/// without cgroup_bw pay only one dlsym-Option check per fire_timer.
pub fn snapshot_via<F>(
    pairs: &[(u64, *mut std::ffi::c_void)],
    mut query: F,
) -> Option<Vec<CbwCgroupSnapshot>>
where
    F: FnMut(u64, *mut std::ffi::c_void, &mut CbwCgroupSnapshot) -> Option<i32>,
{
    let mut out = Vec::with_capacity(pairs.len().min(SNAPSHOT_CAPACITY));
    let mut snap = CbwCgroupSnapshot::default();
    let mut first = true;
    for &(cgid, raw) in pairs {
        let rc = query(cgid, raw, &mut snap);
        if first {
            // Probe the very first query: if the dlsym slot is missing
            // (None), the scheduler doesn't link cgroup_bw at all and
            // the observer is inactive. Bail out early to avoid the
            // per-cgid loop overhead.
            first = false;
            rc?;
        }
        // rc == Some(0): success, snapshot filled. Anything else
        // (-1, -2, -3) means "skip this cgroup" per the wrapper.c
        // contract documented on snapshot_by_raw_cgrp.
        if rc == Some(0) {
            out.push(snap);
        }
    }
    Some(out)
}

/// Diff two snapshots and emit `TraceKind::CgroupBwReplenish` events
/// for every cgroup whose state shows the lib performed a
/// replenishment.
///
/// "Replenishment happened" predicate (mirrors the writes in
/// `replenish_timerfn` -> `cbw_replenish_cgroup`):
///   * `runtime_total_last` changed (set at lib line 1894), OR
///   * `period_budget` changed (set at lib line 1697).
///
/// Either change indicates the lib touched this cgroup's
/// replenishment state during the just-fired timer. (`accounting_timer`
/// fires, by contrast, do not write either field, so cgroups show
/// no change after one and emit no events.)
///
/// Computed fields (mirror lib lines 1679-1706):
///   * `debt = max(rtl_after - pb_before, 0)`. Note that `rtl_after`
///     is the lib's input (set just before `cbw_replenish_cgroup`),
///     and `pb_before` is the previous period's budget -- exactly
///     what the lib uses for the debt formula.
///   * `burst_credit = clamp(nquota - rtl_after, 0, br_before)`.
///   * `period_budget_out = pb_after`.
///   * `keep_throttled = (pb_after <= 0)`.
///
/// Returns the events ready to be appended to the trace. The caller
/// is responsible for the `Trace::record(time_ns, cpu, kind)` calls
/// (the diff is decoupled from the timestamp / CPU choice).
pub fn diff_snapshots(before: &[CbwCgroupSnapshot], after: &[CbwCgroupSnapshot]) -> Vec<TraceKind> {
    // Build a small lookup map for `before` keyed by cgid. Snapshot
    // sizes are bounded by SNAPSHOT_CAPACITY (= 2048) so a linear
    // scan would also work, but a hashmap keeps the diff predictably
    // O(N+M) regardless of size.
    let mut before_idx: std::collections::HashMap<u64, &CbwCgroupSnapshot> =
        std::collections::HashMap::with_capacity(before.len());
    for s in before {
        before_idx.insert(s.cgid, s);
    }

    let mut events = Vec::new();
    for after_s in after {
        let before_s = match before_idx.get(&after_s.cgid) {
            Some(b) => *b,
            None => {
                // Cgroup appeared between snapshots (cgroup_init
                // happened during this fire_timer). Treat as no-op:
                // we have no BEFORE state to compute debt against.
                continue;
            }
        };

        let rtl_changed = before_s.runtime_total_last != after_s.runtime_total_last;
        let pb_changed = before_s.period_budget != after_s.period_budget;
        if !rtl_changed && !pb_changed {
            // No replenishment happened for this cgroup during the
            // just-fired timer.
            continue;
        }

        let rtl_after = after_s.runtime_total_last;
        let pb_before = before_s.period_budget;
        let pb_after = after_s.period_budget;
        let br_before = before_s.burst_remaining;
        let nquota_before = before_s.nquota;

        let debt = (rtl_after - pb_before).max(0);
        let bc_unclamped = nquota_before - rtl_after;
        let burst_credit = bc_unclamped.clamp(0, br_before);
        let keep_throttled = pb_after <= 0;

        events.push(TraceKind::CgroupBwReplenish {
            cgid: CgroupId(after_s.cgid),
            runtime_total_last: rtl_after,
            period_budget_in: pb_before,
            debt,
            burst_credit,
            period_budget_out: pb_after,
            keep_throttled,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        cgid: u64,
        rtl: i64,
        pb: i64,
        br: i64,
        nq: i64,
        nqub: i64,
        thr: i32,
    ) -> CbwCgroupSnapshot {
        CbwCgroupSnapshot {
            cgid,
            runtime_total_last: rtl,
            period_budget: pb,
            burst_remaining: br,
            nquota: nq,
            nquota_ub: nqub,
            is_throttled: thr,
            _pad: 0,
        }
    }

    #[test]
    fn no_change_no_events() {
        let before = vec![snap(1, 0, 1000, 0, 1000, 1000, 0)];
        let after = before.clone();
        let events = diff_snapshots(&before, &after);
        assert!(events.is_empty());
    }

    #[test]
    fn pb_only_change_emits() {
        // period_budget changed (lib wrote it), runtime_total_last
        // unchanged (rare but legal). Should still emit.
        let before = vec![snap(1, 0, 1000, 0, 1000, 1000, 0)];
        let after = vec![snap(1, 0, 500, 0, 1000, 1000, 0)];
        let events = diff_snapshots(&before, &after);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn rtl_only_change_emits_with_debt_zero_when_pb_was_huge() {
        // runtime_total_last increased to 800, but pb was 1000, so no
        // debt accumulated. period_budget unchanged means lib early-
        // returned (out_no_replenish). With nquota_ub = INF the
        // snapshotter would skip; here we use finite quotas to keep
        // the unit test focused on the formulas.
        let before = vec![snap(1, 0, 1000, 100, 1000, 1000, 0)];
        let after = vec![snap(1, 800, 1000, 100, 1000, 1000, 0)];
        let events = diff_snapshots(&before, &after);
        assert_eq!(events.len(), 1);
        if let TraceKind::CgroupBwReplenish {
            debt, burst_credit, ..
        } = &events[0]
        {
            assert_eq!(*debt, 0);
            // burst_credit = clamp(1000 - 800, 0, 100) = 100.
            assert_eq!(*burst_credit, 100);
        } else {
            panic!("wrong event kind");
        }
    }

    #[test]
    fn smoking_gun_keep_throttled_with_zero_runtime() {
        // The bug signature: pb went non-positive AND runtime_total_last
        // is 0 (no work done in the period). Lib carries debt forward.
        let before = vec![snap(1, 0, -500, 0, 1000, 1000, 1)];
        let after = vec![snap(1, 0, -1500, 0, 1000, 1000, 1)];
        let events = diff_snapshots(&before, &after);
        assert_eq!(events.len(), 1);
        if let TraceKind::CgroupBwReplenish {
            keep_throttled,
            runtime_total_last,
            period_budget_out,
            ..
        } = &events[0]
        {
            assert!(*keep_throttled);
            assert_eq!(*runtime_total_last, 0);
            assert!(*period_budget_out <= 0);
        } else {
            panic!("wrong event kind");
        }
    }

    #[test]
    fn cgroup_appears_after_emits_nothing() {
        // Cgroup with cgid=2 only present in `after` -- engine has no
        // BEFORE state for it. Skip.
        let before = vec![snap(1, 0, 1000, 0, 1000, 1000, 0)];
        let after = vec![
            snap(1, 0, 1000, 0, 1000, 1000, 0),
            snap(2, 100, 500, 0, 1000, 1000, 0),
        ];
        let events = diff_snapshots(&before, &after);
        assert!(events.is_empty());
    }
}
