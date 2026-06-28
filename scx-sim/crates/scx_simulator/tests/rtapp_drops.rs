//! Characterization of rt-app key/event DROPS in `load_rtapp`.
//!
//! `load_rtapp` SILENTLY skips SCHED_DEADLINE / uclamp / NUMA-bind / delay
//! keys (they sit in `TASK_PHASE_KEYS` and are `continue`d) and only
//! `warn!`-drops lock/wait/signal/etc. events — a No-Silent-Failures gap the
//! ktstr bridge would inherit (a SCHED_DEADLINE or uclamp workload silently
//! becomes a plain SCX task). These tests PIN the current drop behavior (load
//! succeeds; dropped keys produce no phases) so that the embed
//! degradation contract changing them to a hard error is a deliberate,
//! test-visible decision rather than silent drift.

use scx_simulator::load_rtapp;
use scx_simulator::*;

mod common;

#[test]
fn test_sched_deadline_and_uclamp_keys_silently_dropped() {
    let _lock = common::setup_test();
    // A SCHED_DEADLINE + uclamp + NUMA-bind + delay task. The sim IR has no
    // scheduling class, uclamp, NUMA, or start-delay, so today load_rtapp
    // accepts the keys and emits only the `run` phase.
    let json = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "w": {
                "run": 5000,
                "dl-runtime": 1000,
                "dl-period": 2000,
                "dl-deadline": 1500,
                "util_min": 100,
                "util_max": 900,
                "delay": 50,
                "nodes_membind": 0
            }
        }
    }"#;

    let scenario =
        load_rtapp(json, 2).expect("load_rtapp accepts (silently drops) the dl-*/uclamp keys");
    assert_eq!(scenario.tasks.len(), 1, "expected one task");
    let phases = &scenario.tasks[0].behavior.phases;
    assert_eq!(
        phases.len(),
        1,
        "dl-*/util_*/delay/nodes_membind must produce no phases; got {phases:?}"
    );
    assert!(
        matches!(phases[0], Phase::Run(5_000_000)),
        "expected only Run(5ms) from the `run` key; got {:?}",
        phases[0]
    );
}

#[test]
fn test_unsupported_events_warn_dropped() {
    let _lock = common::setup_test();
    // lock/wait have no sim analogue; today they are warn-dropped (no Phase
    // emitted), leaving only run + sleep.
    let json = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "w": {
                "run": 5000,
                "lock": 1,
                "sleep": 5000,
                "wait": 1
            }
        }
    }"#;

    let scenario = load_rtapp(json, 2).expect("load_rtapp accepts (warn-drops) unsupported events");
    assert_eq!(scenario.tasks.len(), 1);
    let phases = &scenario.tasks[0].behavior.phases;
    assert_eq!(
        phases.len(),
        2,
        "lock/wait must be dropped, leaving run+sleep; got {phases:?}"
    );
    assert!(
        matches!(phases[0], Phase::Run(5_000_000)),
        "phase0 = Run(5ms); got {:?}",
        phases[0]
    );
    assert!(
        matches!(phases[1], Phase::Sleep(5_000_000)),
        "phase1 = Sleep(5ms); got {:?}",
        phases[1]
    );
}
