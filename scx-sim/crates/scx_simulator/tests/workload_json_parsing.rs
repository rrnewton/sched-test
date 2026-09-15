//! Tests for rt-app workload JSON parsing and validation (`load_rtapp`).
//!
//! Complements the happy-path parse+simulate tests in `rtapp.rs` by focusing on
//! the parser's validation surface: invalid JSON, missing required fields,
//! optional-field defaults, nested task groups (`cpu.max` / `cpu.weight`),
//! inline comments, and suspend/resume (rt-app's external pause/resume).
//!
//! Error assertions match against the `RtAppError` Display text (the concrete
//! error type is not re-exported), which is the "clear error" contract.

use scx_simulator::*;

mod common;

const NR_CPUS: u32 = 4;

/// Parse and return the error's Display string (panics if it unexpectedly parsed).
fn parse_err(json: &str) -> String {
    match load_rtapp(json, NR_CPUS) {
        Ok(_) => panic!("expected a parse error but parsing succeeded"),
        Err(e) => format!("{e}"),
    }
}

fn parse_ok(json: &str) -> Scenario {
    load_rtapp(json, NR_CPUS).expect("expected the workload to parse")
}

// ---------------------------------------------------------------------------
// 1. Valid specs parse correctly.
// ---------------------------------------------------------------------------

#[test]
fn test_parse_minimal_valid() {
    let _lock = common::setup_test();
    let json = r#"{
        "global": { "duration": 1 },
        "tasks": { "worker": { "priority": 0, "loop": -1, "run": 1000, "sleep": 2000 } }
    }"#;
    let s = parse_ok(json);
    assert_eq!(s.nr_cpus, NR_CPUS);
    assert_eq!(s.duration_ns, 1_000_000_000, "duration 1s -> 1e9 ns");
    assert_eq!(s.tasks.len(), 1);
    let t = &s.tasks[0];
    assert_eq!(t.name, "worker");
    assert_eq!(t.nice, 0);
    assert_eq!(t.behavior.repeat, RepeatMode::Forever, "loop -1 -> Forever");
    // run 1000us -> Run(1_000_000ns), sleep 2000us -> Sleep(2_000_000ns).
    assert!(t
        .behavior
        .phases
        .iter()
        .any(|p| matches!(p, Phase::Run(1_000_000))));
    assert!(t
        .behavior
        .phases
        .iter()
        .any(|p| matches!(p, Phase::Sleep(2_000_000))));
}

#[test]
fn test_parse_priority_maps_to_nice() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": {
            "hi":  { "priority": -10, "run": 1000, "loop": 1 },
            "lo":  { "priority": 15,  "run": 1000, "loop": 1 }
        }
    }"#;
    let s = parse_ok(json);
    let nice = |name: &str| s.tasks.iter().find(|t| t.name == name).unwrap().nice;
    assert_eq!(nice("hi"), -10);
    assert_eq!(nice("lo"), 15);
}

#[test]
fn test_parse_loop_repeat_mapping() {
    let _lock = common::setup_test();
    let mk = |loop_val: &str| {
        format!(r#"{{ "tasks": {{ "t": {{ "run": 1000, "loop": {loop_val} }} }} }}"#)
    };
    assert_eq!(
        parse_ok(&mk("-1")).tasks[0].behavior.repeat,
        RepeatMode::Forever
    );
    assert_eq!(
        parse_ok(&mk("1")).tasks[0].behavior.repeat,
        RepeatMode::Once
    );
    assert_eq!(
        parse_ok(&mk("3")).tasks[0].behavior.repeat,
        RepeatMode::Count(3)
    );
}

// ---------------------------------------------------------------------------
// 2. Invalid JSON is rejected with clear errors.
// ---------------------------------------------------------------------------

#[test]
fn test_reject_malformed_json() {
    let _lock = common::setup_test();
    let msg = parse_err(r#"{ "tasks": { "t": { "run": 1000 } "#); // truncated / unbalanced
    assert!(msg.contains("JSON parse error"), "unclear error: {msg}");
}

#[test]
fn test_reject_non_object_root() {
    let _lock = common::setup_test();
    let msg = parse_err("[1, 2, 3]");
    assert!(msg.contains("root object"), "unclear error: {msg}");
}

// ---------------------------------------------------------------------------
// 3. Missing required fields produce errors.
// ---------------------------------------------------------------------------

#[test]
fn test_reject_missing_tasks() {
    let _lock = common::setup_test();
    let msg = parse_err(r#"{ "global": { "duration": 1 } }"#);
    assert!(
        msg.contains("missing required field: tasks"),
        "unclear error: {msg}"
    );
}

#[test]
fn test_reject_task_not_object() {
    let _lock = common::setup_test();
    let msg = parse_err(r#"{ "tasks": { "t": 42 } }"#);
    assert!(msg.contains("expected object"), "unclear error: {msg}");
}

#[test]
fn test_reject_nr_cpus_zero() {
    let _lock = common::setup_test();
    let json = r#"{ "tasks": { "t": { "run": 1000 } } }"#;
    match load_rtapp(json, 0) {
        Ok(_) => panic!("expected nr_cpus=0 to be rejected"),
        Err(e) => assert!(format!("{e}").contains("nr_cpus"), "unclear error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// 4. Optional fields have correct defaults.
// ---------------------------------------------------------------------------

#[test]
fn test_optional_field_defaults() {
    let _lock = common::setup_test();
    // No global, no priority, no loop.
    let json = r#"{ "tasks": { "t": { "run": 1000, "sleep": 1000 } } }"#;
    let s = parse_ok(json);
    assert_eq!(
        s.duration_ns, 10_000_000_000,
        "default duration is 10s when global absent"
    );
    assert_eq!(s.tasks[0].nice, 0, "default priority -> nice 0");
    assert_eq!(
        s.tasks[0].behavior.repeat,
        RepeatMode::Forever,
        "default loop -1 -> Forever"
    );
    assert!(
        s.tasks[0].allowed_cpus.is_none(),
        "no cpus -> unrestricted affinity"
    );
}

#[test]
fn test_default_duration_on_nonpositive() {
    let _lock = common::setup_test();
    // duration 0 / negative should fall back to the 10s default.
    let json = r#"{ "global": { "duration": 0 }, "tasks": { "t": { "run": 1000 } } }"#;
    assert_eq!(parse_ok(json).duration_ns, 10_000_000_000);
}

// ---------------------------------------------------------------------------
// 5. Complex specs with nested / configured task groups.
// ---------------------------------------------------------------------------

#[test]
fn test_parse_taskgroup_string_form() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": { "t": { "run": 1000, "loop": 1, "taskgroup": "/grp1" } }
    }"#;
    let s = parse_ok(json);
    assert!(
        s.tasks[0].cgroup_name.as_deref() == Some("/grp1"),
        "task cgroup_name should be /grp1, got {:?}",
        s.tasks[0].cgroup_name
    );
    assert!(
        s.cgroups.iter().any(|c| c.name == "/grp1"),
        "cgroup /grp1 should be registered, got {:?}",
        s.cgroups.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

#[test]
fn test_parse_taskgroup_object_with_cpu_max() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": {
            "t": {
                "run": 1000, "loop": 1,
                "taskgroup": { "path": "/limited", "cpu.max": "20000 100000" }
            }
        }
    }"#;
    let s = parse_ok(json);
    let cg = s
        .cgroups
        .iter()
        .find(|c| c.name == "/limited")
        .expect("/limited cgroup should exist");
    let bw = cg
        .bandwidth
        .as_ref()
        .expect("cpu.max should produce a bandwidth limit");
    assert_eq!(bw.quota_us, 20_000, "quota");
    assert_eq!(bw.period_us, 100_000, "period");
}

#[test]
fn test_parse_taskgroup_cpu_max_unlimited_form() {
    let _lock = common::setup_test();
    // "max PERIOD" is the cgroup-v2 unlimited form and must be accepted.
    let json = r#"{
        "tasks": {
            "t": { "run": 1000, "loop": 1, "taskgroup": { "path": "/free", "cpu.max": "max 100000" } }
        }
    }"#;
    let s = parse_ok(json);
    assert!(s.cgroups.iter().any(|c| c.name == "/free"));
}

#[test]
fn test_reject_bad_cpu_max() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": { "t": { "run": 1000, "taskgroup": { "path": "/g", "cpu.max": "not-a-quota" } } }
    }"#;
    assert!(
        parse_err(json).contains("cpu.max"),
        "should reject malformed cpu.max"
    );
}

#[test]
fn test_reject_cpu_weight_out_of_range() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": { "t": { "run": 1000, "taskgroup": { "path": "/g", "cpu.weight": 99999 } } }
    }"#;
    assert!(
        parse_err(json).contains("cpu.weight"),
        "should reject out-of-range cpu.weight"
    );
}

#[test]
fn test_parse_multiple_taskgroups() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": {
            "a": { "run": 1000, "loop": 1, "taskgroup": "/groupA" },
            "b": { "run": 1000, "loop": 1, "taskgroup": "/groupB" }
        }
    }"#;
    let s = parse_ok(json);
    assert!(s.cgroups.iter().any(|c| c.name == "/groupA"));
    assert!(s.cgroups.iter().any(|c| c.name == "/groupB"));
}

// ---------------------------------------------------------------------------
// 6. External pause/resume: rt-app suspend/resume (there is no `external_pause`
//    field; see the note test below).
// ---------------------------------------------------------------------------

#[test]
fn test_parse_suspend_resume() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": {
            "producer": { "loop": -1, "run": 5000, "resume": "consumer", "sleep": 5000 },
            "consumer": { "loop": -1, "suspend": "consumer", "run": 10000 }
        }
    }"#;
    let s = parse_ok(json);
    let producer = s.tasks.iter().find(|t| t.name == "producer").unwrap();
    let consumer = s.tasks.iter().find(|t| t.name == "consumer").unwrap();
    // resume -> a Wake phase; suspend -> Sleep(u64::MAX).
    assert!(
        producer
            .behavior
            .phases
            .iter()
            .any(|p| matches!(p, Phase::Wake(_))),
        "producer should have a Wake phase from resume"
    );
    assert!(
        consumer
            .behavior
            .phases
            .iter()
            .any(|p| matches!(p, Phase::Sleep(u64::MAX))),
        "consumer should have a suspend (Sleep MAX) phase"
    );
}

#[test]
fn test_reject_unresolved_resume() {
    let _lock = common::setup_test();
    let json = r#"{
        "tasks": { "p": { "loop": 1, "run": 1000, "resume": "ghost" } }
    }"#;
    let msg = parse_err(json);
    assert!(
        msg.contains("unresolved resume") || msg.contains("ghost"),
        "should report the unresolved resume target: {msg}"
    );
}

/// `external_pause` is NOT a recognized rt-app field in this parser (rt-app's
/// pause semantics are expressed via `suspend`/`resume`). Unknown task fields
/// are ignored rather than rejected, so a spec carrying `external_pause` parses
/// identically to one without it.
#[test]
fn test_external_pause_field_is_ignored() {
    let _lock = common::setup_test();
    let with = r#"{ "tasks": { "t": { "run": 1000, "sleep": 1000, "loop": 1, "external_pause": true } } }"#;
    let without = r#"{ "tasks": { "t": { "run": 1000, "sleep": 1000, "loop": 1 } } }"#;
    let s_with = parse_ok(with);
    let s_without = parse_ok(without);
    // Unknown field ignored: same task shape either way.
    assert_eq!(s_with.tasks.len(), 1);
    assert_eq!(
        s_with.tasks[0].behavior.phases.len(),
        s_without.tasks[0].behavior.phases.len()
    );
    assert_eq!(
        s_with.tasks[0].behavior.repeat,
        s_without.tasks[0].behavior.repeat
    );
}

// ---------------------------------------------------------------------------
// Extras: comment stripping, instances, cpu affinity, end-to-end simulate.
// ---------------------------------------------------------------------------

/// `_comment`/`_*` idiom keys are accepted and ignored (documented in
/// examples/README.md).
#[test]
fn test_underscore_comment_keys_ignored() {
    let _lock = common::setup_test();
    let json = r#"{
        "_comment": "top-level note",
        "global": { "duration": 1, "_note": "hi" },
        "tasks": { "t": { "_why": "demo", "run": 1000, "loop": 1 } }
    }"#;
    let s = parse_ok(json);
    assert_eq!(s.tasks.len(), 1);
    assert_eq!(s.tasks[0].name, "t");
}

#[test]
fn test_instance_expands_to_multiple_tasks() {
    let _lock = common::setup_test();
    let json = r#"{ "tasks": { "w": { "run": 1000, "loop": 1, "instance": 3 } } }"#;
    let s = parse_ok(json);
    assert_eq!(
        s.tasks.len(),
        3,
        "instance:3 should yield 3 tasks, got {}",
        s.tasks.len()
    );
    // Distinct PIDs.
    let pids: std::collections::HashSet<_> = s.tasks.iter().map(|t| t.pid).collect();
    assert_eq!(pids.len(), 3, "instances must have distinct PIDs");
}

#[test]
fn test_cpus_affinity_parsing() {
    let _lock = common::setup_test();
    let json = r#"{ "tasks": { "t": { "run": 1000, "loop": 1, "cpus": "0-1" } } }"#;
    let s = parse_ok(json);
    let allowed = s.tasks[0]
        .allowed_cpus
        .as_ref()
        .expect("cpus should set affinity");
    assert!(allowed.contains(&CpuId(0)) && allowed.contains(&CpuId(1)));
    assert!(
        !allowed.contains(&CpuId(3)),
        "cpus 0-1 must not include CPU 3"
    );
}

/// A parsed spec must actually be runnable end-to-end (parse feeds a valid
/// Scenario to the engine).
#[test]
fn test_parsed_spec_simulates() {
    let _lock = common::setup_test();
    let json = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "a": { "priority": 0, "loop": -1, "run": 2000, "sleep": 2000 },
            "b": { "priority": -3, "loop": -1, "run": 3000, "sleep": 1000 }
        }
    }"#;
    let scenario = parse_ok(json);
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(trace.schedule_count(Pid(1)) > 0 && trace.schedule_count(Pid(2)) > 0);
}
