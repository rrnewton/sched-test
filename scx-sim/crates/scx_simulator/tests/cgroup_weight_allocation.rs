//! cgroup `cpu.weight` — parse/validation guard + substrate-gap characterization.
//!
//! # Why this file is not a proportional-allocation test
//!
//! The tg task `test-cgroup-weight-allocation` asked for cgroup weight-based
//! *proportional CPU allocation* tests (equal weights → equal CPU, 2:1 ratio →
//! ~2:1 split, mid-sim weight change, zero-task cgroup, nested weight
//! inheritance). Investigation found that cgroup-v2 `cpu.weight` proportional
//! allocation is **not modeled or runnable in scxsim** — only cgroup
//! *bandwidth* (`cpu.max` quota/period throttling, via `cgroup_bw.bpf.c`) is a
//! real executed path. Concretely:
//!
//!   * `CgroupDef` (`safe/scenario.rs`) has no weight field — only `name`,
//!     `parent_name`, `cpuset`, and `bandwidth`.
//!   * No `ScenarioBuilder` API sets a cgroup weight (only cpuset/bandwidth).
//!   * rt-app `cpu.weight` is parsed and range-validated, then **discarded** —
//!     it is never stored into `RtTaskgroupSpec` or `CgroupDef`
//!     (`safe/rtapp.rs`).
//!   * The scheduler FFI surface has `cgroup_init`/`cgroup_exit`/`cgroup_move`/
//!     `cgroup_set_bandwidth` but **no** `cgroup_set_weight`
//!     (`unsafe_impl/ffi.rs`); LAVD registers only the four bandwidth cgroup
//!     ops and COSMOS registers none.
//!
//! Writing a "proportional CPU by cgroup weight" test would therefore assert on
//! behavior that no BPF scheduler code produces under scxsim — a violation of
//! the project's No-Stub Rule. The substrate work needed to make it real
//! (weight field + builder + rt-app storage + `cgroup_set_weight` FFI op +
//! engine dispatch + a scheduler that consumes it) is tracked in **minibeads
//! issue sim-77e5fb**.
//!
//! The real, *runnable* proportional path in scxsim is per-**task** weight
//! (`nice` → `p->scx.weight` vtime), which is already covered by
//! `tests/simple.rs::{test_weighted_fairness, test_three_way_weighted_fairness}`
//! and `tests/lavd.rs::test_lavd_varied_nice_values`.
//!
//! # What this file DOES test (100% real, no stubs)
//!
//! The only `cpu.weight` code that exists today: the rt-app parser's accept /
//! range-validate path, plus a characterization that a weight-only taskgroup
//! yields a cgroup carrying no scheduling-affecting state (the gap itself).
//! These are regression breadcrumbs — when sim-77e5fb lands the weight
//! substrate, `test_cpu_weight_parsed_then_discarded` should be replaced by
//! real proportional-allocation tests.

use scx_simulator::*;

mod common;

/// rt-app JSON with a single task in a taskgroup that carries only `cpu.weight`
/// (no `cpu.max`, no `cpus`). `weight` is interpolated verbatim so callers can
/// exercise both valid and invalid values.
fn json_with_weight(weight: &str) -> String {
    format!(
        r#"{{
            "global": {{ "duration": 1 }},
            "tasks": {{
                "runner": {{
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {{
                        "path": "/tg_weighted",
                        "cpu.weight": {weight}
                    }}
                }}
            }}
        }}"#
    )
}

/// A valid `cpu.weight` parses, the cgroup is created, and the task joins it —
/// but the weight produces NO scheduling-affecting state (the parsed value is
/// discarded today; the resulting `CgroupDef` has no weight field and, for a
/// weight-only taskgroup, no bandwidth). This pins the current behavior; it
/// must be revisited when sim-77e5fb implements the weight substrate.
#[test]
fn test_cpu_weight_parsed_then_discarded() {
    let _lock = common::setup_test();

    let scenario =
        load_rtapp(&json_with_weight("250"), 2).expect("valid cpu.weight taskgroup should parse");

    // The cgroup is created and the task is assigned to it.
    assert_eq!(scenario.cgroups.len(), 1, "expected exactly one cgroup");
    assert_eq!(scenario.cgroups[0].name, "/tg_weighted");
    assert_eq!(
        scenario.tasks[0].cgroup_name.as_deref(),
        Some("/tg_weighted"),
        "task should be assigned to its taskgroup cgroup"
    );

    // ...but the weight yields no actionable state: a weight-only taskgroup has
    // no bandwidth, and `CgroupDef` has no weight field at all. cpu.weight is
    // inert under scxsim today (sim-77e5fb).
    assert!(
        scenario.cgroups[0].bandwidth.is_none(),
        "weight-only taskgroup unexpectedly produced bandwidth state \
         (has cpu.weight started affecting the model? update this test + sim-77e5fb)"
    );
}

/// `cpu.weight` below the valid `1..=10000` range is rejected by the parser.
#[test]
fn test_cpu_weight_below_min_rejected() {
    let _lock = common::setup_test();
    let err = load_rtapp(&json_with_weight("0"), 2)
        .expect_err("cpu.weight = 0 should be rejected")
        .to_string();
    assert!(
        err.contains("cpu.weight") && err.contains("1..=10000"),
        "expected a cpu.weight range error, got: {err}"
    );
}

/// `cpu.weight` above the valid `1..=10000` range is rejected by the parser.
#[test]
fn test_cpu_weight_above_max_rejected() {
    let _lock = common::setup_test();
    let err = load_rtapp(&json_with_weight("10001"), 2)
        .expect_err("cpu.weight = 10001 should be rejected")
        .to_string();
    assert!(
        err.contains("cpu.weight") && err.contains("1..=10000"),
        "expected a cpu.weight range error, got: {err}"
    );
}

/// A non-integer `cpu.weight` is rejected with an "expected integer" error.
#[test]
fn test_cpu_weight_non_integer_rejected() {
    let _lock = common::setup_test();
    let err = load_rtapp(&json_with_weight(r#""heavy""#), 2)
        .expect_err("non-integer cpu.weight should be rejected")
        .to_string();
    assert!(
        err.contains("cpu.weight") && err.contains("expected integer"),
        "expected a cpu.weight integer-type error, got: {err}"
    );
}

/// The minimum and maximum valid `cpu.weight` values (`1` and `10000`) are
/// both accepted — boundary guard for the range check.
#[test]
fn test_cpu_weight_range_boundaries_accepted() {
    let _lock = common::setup_test();
    for w in ["1", "10000"] {
        load_rtapp(&json_with_weight(w), 2)
            .unwrap_or_else(|e| panic!("cpu.weight = {w} should be accepted, got: {e}"));
    }
}
