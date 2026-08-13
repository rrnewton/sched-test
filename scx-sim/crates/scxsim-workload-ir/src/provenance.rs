//! Mechanical provenance: does every value in the output appear in the input?
//!
//! That question is the instrument that found five defects in eight
//! exact-claiming lowering arms. This makes it a function instead of something
//! a reviewer has to remember to ask.
//!
//! # Why the fidelity report cannot be the check
//!
//! It was wrong in both directions, and each was demonstrated on real code:
//!
//! * **Green while fabricating.** `SpinWait` invented a 500us scheduling
//!   quantum and `is_exact()` returned true. Three tests asserted on that flag
//!   and passed; the result was a 68x slice-count divergence against a live
//!   kernel.
//! * **Non-green and still hiding something.** `PreemptStorm` recorded its
//!   iterations conversion and said nothing about inventing
//!   `Fifo { priority: 50 }` for a work type with no priority field. A
//!   non-green report is not a disclosed one.
//!
//! So the check ignores the flag entirely and looks at values.
//!
//! # The rule
//!
//! Every semantic number the lowering puts into the IR must be **explained**:
//!
//! 1. it appears verbatim in the serialised source scenario, or
//! 2. it is a declared iteration count converted at [`crate::lower::ITER_NS`],
//!    or
//! 3. it is the scenario duration (a continuously-running task), or
//! 4. it is zero — the absence of a value is not an invented one, or
//! 5. **it is named in a recorded approximation.**
//!
//! Rule 5 is the point. An unexplained value is permitted *only if the
//! lowering said out loud that it supplied it*. That is exactly the standard
//! the audit arrived at, and it is what makes the check compatible with arms
//! that legitimately must default something.
//!
//! # What it does not cover
//!
//! Stated plainly rather than implied, because a check whose limits are vague
//! invites false confidence:
//!
//! * **Structural integers** — task ids, task counts, CPU indices, topology
//!   fan-out. These are shape, not declared behaviour, and including them
//!   would drown the signal.
//! * **Arithmetic combinations.** `MutexContention` emits `hold + work`, a sum
//!   of two declared values. The sum is not in the input, so it must be
//!   disclosed like anything else. That is a deliberate strictness: a derived
//!   value is still a value the reader cannot find in the scenario.
//! * **Whether a disclosed value is a GOOD choice.** The check proves the
//!   lowering was honest, not that it was right. `SpinWait`'s 500us was wrong
//!   by 68x; a disclosed 500us would still have been wrong, just visible.
//! * **DROPPED values — structurally out of reach.** This walks the OUTPUT and
//!   asks where each value came from. A value the lowering discarded has no
//!   output to walk, so no amount of strengthening here will find it. The
//!   `Sequence` + `Yield(9ms)` silent drop is exactly that shape and is covered
//!   by a separate targeted test
//!   (`a_dropped_yield_duration_is_recorded_not_silent`). Anyone extending this
//!   check should know it is deaf to that whole class rather than assume the
//!   green tick covers it.
//!
//! # Verified against known bugs
//!
//! A check that does not catch a bug you already have is decoration. Each of
//! these was reintroduced into the lowering and the check was confirmed to
//! fail:
//!
//! | reintroduced | caught |
//! |---|---|
//! | `SpinWait` fabricating the 500us quantum (the 68x bug) | yes |
//! | `YieldHeavy` fabricating its quantum | yes |
//! | `EpollStorm` fabricating work-per-event | yes |
//! | `PreemptStorm` inventing `priority: 50` undisclosed | yes |
//! | `sched_class` inventing an RT priority undisclosed | yes |
//!
//! Two of those needed the check to be *strengthened* before they were caught,
//! and the strengthening is worth knowing about because it is the subtle part.
//! `PreemptStorm` declares `rt_burst_iters: 50`, so the digits "50" genuinely
//! appear in its scenario AND in an unrelated approximation's text — a plain
//! value match called the invented priority explained. Priorities are therefore
//! held to a stricter rule: they must come from a priority-shaped source field,
//! or be named in an approximation that is itself about a priority.
//!
//! That strengthening immediately found a SIXTH defect the hand audit had
//! missed: `sched_class`, a shared helper, supplies `priority: 50` for every
//! caller, and a fabrication inside a helper does not look like one at the call
//! site.

use std::collections::BTreeSet;

use crate::fidelity::FidelityReport;
use crate::ir::{Phase, SchedPolicy, WorkloadIr};
use crate::lower::ITER_NS;
use crate::source::SourceScenario;
use crate::units::DurationNs;

/// A value in the lowered IR that traces to nothing in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unexplained {
    /// Where in the IR it sits, e.g. `task[0].phases[1] Run`.
    pub what: String,
    /// The value itself.
    pub value: u64,
}

impl std::fmt::Display for Unexplained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} = {} — appears nowhere in the source scenario and is not named \
             in any recorded approximation",
            self.what, self.value
        )
    }
}

/// Numbers a reader could find in, or derive directly from, the source.
struct Provenance {
    declared: BTreeSet<u64>,
    /// Declared values that came from a field whose NAME looks like a
    /// scheduling priority or nice value.
    ///
    /// Bare value-matching cannot tell "the number 50 came from
    /// `rt_burst_iters`" from "the number 50 was invented as an RT priority",
    /// and `PreemptStorm`'s sample really does declare `rt_burst_iters: 50` —
    /// so the undisclosed `priority: 50` slipped through the first version of
    /// this check. Keying priorities to priority-shaped fields closes that.
    declared_priorities: BTreeSet<u64>,
    disclosed: String,
    /// Each approximation rendered separately, so a value can be required to
    /// appear in a record that is ABOUT the thing being explained.
    segments: Vec<String>,
}

impl Provenance {
    fn build(source: &SourceScenario, report: &FidelityReport) -> Self {
        let mut declared = BTreeSet::new();
        declared.insert(0);
        declared.insert(source.duration.as_nanos());

        // Serialising the source and walking every number is what makes this
        // generic: a new field on a work type is covered without touching this
        // file, which is the opposite of the per-arm table it replaces.
        let mut declared_priorities = BTreeSet::new();
        if let Ok(v) = serde_json::to_value(source) {
            collect_numbers(&v, &mut declared);
            collect_named(&v, "", &mut declared_priorities);
        }

        // Iteration counts are the one conversion the IR performs on a declared
        // value, so their product is still traceable to the input.
        let iters: Vec<u64> = declared
            .iter()
            .filter_map(|n| n.checked_mul(ITER_NS))
            .collect();
        declared.extend(iters);

        // ktstr declares durations in us, ms and s depending on the field; the
        // IR is uniformly ns. A unit conversion does not change WHERE the value
        // came from, so the converted forms are still traceable to the input.
        // Without this, `ThunderingHerd`'s declared `inter_batch_ms: 10` looked
        // fabricated at 10_000_000 ns — a false positive, and false positives
        // are how a check like this gets deleted.
        for scale in [1_000u64, 1_000_000, 1_000_000_000] {
            let scaled: Vec<u64> = declared
                .iter()
                .filter_map(|n| n.checked_mul(scale))
                .collect();
            declared.extend(scaled);
        }

        let disclosed = report
            .approximations()
            .iter()
            .map(|a| format!("{} {} {}", a.source, a.lowered_to, a.dropped))
            .collect::<Vec<_>>()
            .join(" | ");

        let segments = report
            .approximations()
            .iter()
            .map(|a| format!("{} {} {}", a.source, a.lowered_to, a.dropped))
            .collect();

        Provenance {
            declared,
            declared_priorities,
            disclosed,
            segments,
        }
    }

    /// Is a PRIORITY-like value traceable, or admitted to?
    ///
    /// Stricter than [`Self::explains`]: a priority must come from a
    /// priority-shaped source field, not merely coincide with some unrelated
    /// number in the scenario.
    fn explains_priority(&self, v: u64) -> bool {
        if self.declared_priorities.contains(&v) {
            return true;
        }
        // The disclosure must be ABOUT a priority, not merely contain the
        // digits somewhere. `PreemptStorm` declares `rt_burst_iters: 50`, so
        // its iterations approximation renders "rt_burst_iters=50" — which a
        // whole-report substring match happily accepted as an explanation for
        // an entirely unrelated invented `priority: 50`. Requiring the same
        // record to say "priority" is what separates the two.
        self.segments.iter().any(|seg| {
            let lower = seg.to_ascii_lowercase();
            (lower.contains("prio") || lower.contains("nice") || lower.contains("weight"))
                && contains_integer(seg, v)
        })
    }

    /// Is this value traceable, or at least admitted to?
    fn explains(&self, v: u64) -> bool {
        if self.declared.contains(&v) {
            return true;
        }
        // Named in an approximation. Durations are matched on their rendered
        // form ("500.000us"), which is distinctive; bare integers are matched
        // on a digit boundary so that 50 does not match 500.
        if self.disclosed.contains(&DurationNs(v).to_string()) {
            return true;
        }
        contains_integer(&self.disclosed, v)
    }
}

/// Substring match on `v` that will not accept it as part of a longer number.
fn contains_integer(haystack: &str, v: u64) -> bool {
    let needle = v.to_string();
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_digit();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Collect numbers reached through a field whose name looks like a priority.
fn collect_named(v: &serde_json::Value, key: &str, out: &mut BTreeSet<u64>) {
    let priorityish = |k: &str| {
        let k = k.to_ascii_lowercase();
        k.contains("prio") || k.contains("nice") || k.contains("weight")
    };
    match v {
        serde_json::Value::Number(n) if priorityish(key) => {
            if let Some(i) = n.as_i64() {
                out.insert(i.unsigned_abs());
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_named(x, key, out)),
        serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| collect_named(x, k, out)),
        _ => {}
    }
}

fn collect_numbers(v: &serde_json::Value, out: &mut BTreeSet<u64>) {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                out.insert(u);
            }
            if let Some(i) = n.as_i64() {
                if i >= 0 {
                    out.insert(i as u64);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_numbers(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_numbers(x, out)),
        _ => {}
    }
}

/// How strictly a value must trace back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Any declared number, in any unit. Durations are compared this way
    /// because ktstr spreads them across us/ms/s fields and a stricter rule
    /// produced false positives.
    Loose,
    /// Must come from a priority-shaped field, or be disclosed.
    Priority,
}

/// Every semantic value the lowering emitted, labelled.
///
/// Deliberately a hand-written list of the fields that carry DECLARED
/// BEHAVIOUR, rather than a generic walk of the whole IR. A generic walk would
/// also pick up task ids, task counts and CPU indices — structure rather than
/// behaviour — and the noise would make the check unusable, which is the same
/// as not having it.
fn emitted_values(ir: &WorkloadIr) -> Vec<(String, u64, Kind)> {
    let mut out = Vec::new();
    for (i, t) in ir.tasks.iter().enumerate() {
        if t.start != DurationNs::ZERO {
            out.push((format!("task[{i}].start"), t.start.as_nanos(), Kind::Loose));
        }
        match t.policy {
            SchedPolicy::Fifo { priority } | SchedPolicy::RoundRobin { priority } => {
                out.push((
                    format!("task[{i}].policy.priority"),
                    priority.max(0) as u64,
                    Kind::Priority,
                ));
            }
            _ => {}
        }
        let n = t.nice.0;
        if n != 0 {
            out.push((
                format!("task[{i}].nice"),
                n.unsigned_abs() as u64,
                Kind::Priority,
            ));
        }
        for (j, p) in t.phases.iter().enumerate() {
            match p {
                Phase::Run(d) => out.push((
                    format!("task[{i}].phases[{j}] Run"),
                    d.as_nanos(),
                    Kind::Loose,
                )),
                Phase::Sleep(d) => out.push((
                    format!("task[{i}].phases[{j}] Sleep"),
                    d.as_nanos(),
                    Kind::Loose,
                )),
                Phase::Yield | Phase::Wake(_) => {}
            }
        }
    }
    for (i, cg) in ir.cgroups.iter().enumerate() {
        if let Some(w) = cg.weight {
            out.push((format!("cgroup[{i}].weight"), w as u64, Kind::Priority));
        }
        if let Some(b) = cg.bandwidth.as_ref() {
            out.push((
                format!("cgroup[{i}].bandwidth.quota"),
                b.quota.as_nanos(),
                Kind::Loose,
            ));
            out.push((
                format!("cgroup[{i}].bandwidth.period"),
                b.period.as_nanos(),
                Kind::Loose,
            ));
        }
    }
    for (i, tm) in ir.timeline.iter().enumerate() {
        if tm.at != DurationNs::ZERO {
            out.push((format!("timeline[{i}].at"), tm.at.as_nanos(), Kind::Loose));
        }
    }
    out
}

/// Check a lowered IR against the scenario it came from.
///
/// Returns every value that traces to nothing. An empty result means the
/// lowering either carried declared values through or admitted to supplying
/// them — it does NOT mean the lowering is correct, only that it is honest.
///
/// ```
/// # use scxsim_workload_ir::*;
/// let src = SourceScenario::new("s").step(SourceStep::new(
///     vec![SourceCgroupDef::named("cg_0")], SourceHold::FULL));
/// let ir = lower(&src).expect("lowers");
/// assert!(provenance::check(&src, &ir).is_empty());
/// ```
pub fn check(source: &SourceScenario, ir: &WorkloadIr) -> Vec<Unexplained> {
    let p = Provenance::build(source, &ir.fidelity);
    emitted_values(ir)
        .into_iter()
        .filter(|(_, v, kind)| match kind {
            Kind::Loose => !p.explains(*v),
            Kind::Priority => !p.explains_priority(*v),
        })
        .map(|(what, value, _)| Unexplained { what, value })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_match_respects_digit_boundaries() {
        assert!(contains_integer("priority: 50", 50));
        assert!(!contains_integer("Run(500.000us)", 50), "50 is not 500");
        assert!(contains_integer("a 50, b", 50));
        assert!(!contains_integer("1500", 50));
        assert!(contains_integer("50", 50));
    }
}
