//! Schema characterization for the public structops JSONL renderer.
//!
//! ktstr's report synthesis / live-vs-sim diff consumes the public
//! `structops_jsonl::write_jsonl`. This pins the canonical record schema
//! (`ts_ns`/`cpu`/`pid`/`kind`/`name`/`phase`/`args`/`ret`) over a REAL
//! simulation through the public re-export, so a refactor that perturbs the
//! renderer or the `TraceEvent`->JSONL mapping fails loudly. (The in-module
//! unit test only checks well-formedness of hand-crafted events; this pins
//! the public surface + real-engine output the embed will consume.)

use scx_simulator::structops_jsonl::write_jsonl;
use scx_simulator::*;
use serde_json::Value;

mod common;

#[test]
fn test_structops_jsonl_schema() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .instant_timing()
        .add_task(
            "a",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(5_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .add_task(
            "b",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .duration_ms(50)
        .build();
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    let mut buf = Vec::new();
    write_jsonl(&trace, &mut buf).expect("write_jsonl failed");
    let text = String::from_utf8(buf).expect("structops JSONL is not valid UTF-8");
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "no structops JSONL emitted for a real simulation"
    );

    for (i, line) in lines.iter().enumerate() {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON ({e}): {line}"));
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("line {i} is not a JSON object: {line}"));

        for field in [
            "ts_ns", "cpu", "pid", "kind", "name", "phase", "args", "ret",
        ] {
            assert!(
                obj.contains_key(field),
                "line {i} missing required field `{field}`: {line}"
            );
        }
        assert!(obj["ts_ns"].is_u64(), "line {i}: ts_ns is not u64: {line}");
        assert!(
            obj["cpu"].is_i64(),
            "line {i}: cpu is not an integer: {line}"
        );
        assert!(
            obj["pid"].is_i64(),
            "line {i}: pid is not an integer: {line}"
        );
        assert!(
            obj["args"].is_object(),
            "line {i}: args is not an object: {line}"
        );

        let kind = obj["kind"].as_str().unwrap_or("");
        assert!(
            kind == "structop" || kind == "helper",
            "line {i}: unexpected kind `{kind}`: {line}"
        );
        let phase = obj["phase"].as_str().unwrap_or("");
        assert!(
            phase == "entry" || phase == "exit",
            "line {i}: unexpected phase `{phase}`: {line}"
        );
        assert!(
            !obj["name"].as_str().unwrap_or("").is_empty(),
            "line {i}: empty name: {line}"
        );
    }
}
