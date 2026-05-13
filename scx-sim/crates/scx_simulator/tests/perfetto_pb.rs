//! Smoke test for the wprof-compatible perfetto protobuf trace
//! writer (`Trace::write_perfetto_pb`).
//!
//! Round-trips a tiny scxsim trace through `perfetto_protos::Trace::
//! parse_from_bytes` (the same parser scxtop's `load_perfetto_trace`
//! uses) and asserts the wprof event vocabulary is present so that
//! scxtop can ingest a scxsim `.pb` side-by-side with a wprof `.pb`.
//!
//! Mapping under test (from
//! `experiments/wprof_trace_baseline_20260513/REPORT.md` §5):
//!
//! | scxsim TraceKind | wprof category | TrackEvent type |
//! |---|---|---|
//! | TaskScheduled    | `ONCPU`        | `TYPE_SLICE_BEGIN` |
//! | TaskSlept/etc.   | `ONCPU`        | `TYPE_SLICE_END`   |
//! | TaskWoke         | `WAKEE`        | `TYPE_INSTANT`     |
//! | DsqInsert*       | `SCX_DSQ`      | `TYPE_INSTANT`     |
//! | Tick             | `TIMER`        | `TYPE_INSTANT`     |
//!
//! The test does NOT depend on scxtop directly (scxtop is a separate
//! crate in the scx submodule and brings in libbpf-rs etc.); it uses
//! `perfetto_protos` directly, which IS the parsing primitive scxtop's
//! loader is built on (see
//! `scx/tools/scxtop/src/mcp/perfetto_track_event_types.rs::parse_track_event`).

use perfetto_protos::{
    debug_annotation::debug_annotation, trace::Trace as TraceProto, trace_packet::trace_packet,
    track_event::track_event,
};
use protobuf::Message;

use scx_simulator::*;

#[macro_use]
mod common;

/// Assemble a tiny multi-CPU multi-task scenario, run it, write the
/// trace as a wprof-compatible perfetto protobuf, and parse it back
/// via `perfetto_protos::Trace::parse_from_bytes`. Then assert the
/// wprof event vocabulary is present.
#[test]
fn test_perfetto_pb_roundtrip_wprof_compatible() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .instant_timing()
        .add_task(
            "worker-0",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(5_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .add_task(
            "worker-1",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    // Write to an in-memory buffer.
    let mut buf = Vec::new();
    trace
        .write_perfetto_pb(&mut buf)
        .expect("write_perfetto_pb failed");

    // Round-trip through the SAME parser scxtop's loader uses.
    let proto: TraceProto =
        TraceProto::parse_from_bytes(&buf).expect("parse_from_bytes failed on scxsim .pb");
    assert!(!proto.packet.is_empty(), "perfetto trace has no packets");

    // Categorize packets.
    let mut nr_track_descriptor = 0usize;
    let mut nr_track_event = 0usize;
    let mut nr_oncpu_begin = 0usize;
    let mut nr_oncpu_end = 0usize;
    let mut nr_wakee_instant = 0usize;
    let mut nr_scx_dsq_instant = 0usize;
    let mut nr_timer_instant = 0usize;
    let mut nr_with_scx_dsq_id_annotation = 0usize;
    let mut nr_with_cpu_annotation = 0usize;

    for packet in &proto.packet {
        match &packet.data {
            Some(trace_packet::Data::TrackDescriptor(_)) => {
                nr_track_descriptor += 1;
            }
            Some(trace_packet::Data::TrackEvent(ev)) => {
                nr_track_event += 1;
                let cat = ev.categories.first().map(String::as_str).unwrap_or("");
                let ty = ev
                    .type_
                    .as_ref()
                    .map(|t| t.enum_value_or_default())
                    .unwrap_or(track_event::Type::TYPE_UNSPECIFIED);

                match (cat, ty) {
                    ("ONCPU", track_event::Type::TYPE_SLICE_BEGIN) => nr_oncpu_begin += 1,
                    ("ONCPU", track_event::Type::TYPE_SLICE_END) => nr_oncpu_end += 1,
                    ("WAKEE", track_event::Type::TYPE_INSTANT) => nr_wakee_instant += 1,
                    ("SCX_DSQ", track_event::Type::TYPE_INSTANT) => nr_scx_dsq_instant += 1,
                    ("TIMER", track_event::Type::TYPE_INSTANT) => nr_timer_instant += 1,
                    _ => {}
                }

                // Inspect debug_annotations: confirm the wprof keys we
                // promise (cpu, scx_dsq_id) are actually emitted.
                for ann in &ev.debug_annotations {
                    let name = match &ann.name_field {
                        Some(debug_annotation::Name_field::Name(n)) => n.as_str(),
                        _ => "",
                    };
                    if name == "cpu" {
                        nr_with_cpu_annotation += 1;
                    }
                    if name == "scx_dsq_id" {
                        nr_with_scx_dsq_id_annotation += 1;
                    }
                }
            }
            _ => {}
        }
    }

    // Track descriptors: at minimum 1 (process) + nr_cpus + nr_tasks.
    assert!(
        nr_track_descriptor >= 1 + 2 + 2,
        "expected ≥ {} TrackDescriptors (1 process + 2 CPUs + 2 tasks), got {}",
        1 + 2 + 2,
        nr_track_descriptor
    );

    // Some TrackEvents must exist.
    assert!(nr_track_event > 0, "no TrackEvents emitted");

    // ONCPU slices must have matched begin/end pairs (the simulator
    // closes every running slice via SimulationEnd if nothing else
    // closes it first).
    assert!(nr_oncpu_begin > 0, "no ONCPU slice begin events emitted");
    assert_eq!(
        nr_oncpu_begin, nr_oncpu_end,
        "ONCPU begin/end mismatch: {nr_oncpu_begin} begins vs {nr_oncpu_end} ends"
    );

    // Categories that this trace MUST surface for wprof parity:
    // SCX_DSQ instants and TIMER instants are emitted by the simple
    // scheduler on every dispatch cycle. WAKEE may or may not fire
    // depending on whether the workload includes any sleep/wake
    // boundary in 50 ms.
    assert!(
        nr_scx_dsq_instant > 0,
        "no SCX_DSQ instant events emitted (DsqInsert mapping broken)"
    );
    assert!(
        nr_timer_instant > 0,
        "no TIMER instant events emitted (Tick mapping broken)"
    );
    assert!(
        nr_wakee_instant > 0,
        "no WAKEE instant events emitted (TaskWoke mapping broken)"
    );

    // Annotation keys: each SCX_DSQ instant carries scx_dsq_id; each
    // ONCPU slice carries cpu.
    assert!(
        nr_with_scx_dsq_id_annotation >= nr_scx_dsq_instant,
        "fewer scx_dsq_id annotations ({nr_with_scx_dsq_id_annotation}) than \
         SCX_DSQ instants ({nr_scx_dsq_instant})"
    );
    assert!(
        nr_with_cpu_annotation >= nr_oncpu_begin,
        "fewer cpu annotations ({nr_with_cpu_annotation}) than \
         ONCPU begins ({nr_oncpu_begin})"
    );

    // Every packet that carries a TrackEvent must also carry a
    // timestamp (scxtop's parser drops events with no timestamp).
    let untimed_events = proto
        .packet
        .iter()
        .filter(|p| {
            matches!(p.data, Some(trace_packet::Data::TrackEvent(_))) && p.timestamp.is_none()
        })
        .count();
    assert_eq!(
        untimed_events, 0,
        "{untimed_events} TrackEvent packets are missing the required timestamp field"
    );
}
