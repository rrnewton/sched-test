//! Smoke + ingester-invariant tests for the wprof-compatible
//! perfetto protobuf trace writer (`Trace::write_perfetto_pb`).
//!
//! Two tests, layered:
//!
//! 1. [`test_perfetto_pb_roundtrip_wprof_compatible`] round-trips
//!    the bytes through `perfetto_protos::Trace::parse_from_bytes`
//!    and asserts the wprof event vocabulary is present plus the
//!    Perfetto wire-format invariants the ingester requires
//!    (`trusted_packet_sequence_id` non-None on every packet — the
//!    F1 invariant whose absence shipped in the original R3 PR
//!    and silently dropped 100 % of TrackEvents in
//!    `trace_processor` and downstream consumers — see
//!    `experiments/wprof_live_vs_scxsim_perfetto_20260513/REPORT.md`
//!    §6 F1).
//! 2. [`test_perfetto_pb_ingestible_by_trace_processor`] (gated on
//!    `trace_processor_shell` being on `$PATH` so it stays a no-op
//!    in stripped CI environments) writes the .pb to a tempfile,
//!    invokes `trace_processor_shell -q '<sql>'`, and asserts
//!    `SELECT COUNT(*) FROM slice` is non-zero and matches the
//!    in-memory ONCPU begin count. This is the hardening F1 needed:
//!    without it the original emitter regression escaped review
//!    (parse_from_bytes was happy; the actual ingester silently
//!    dropped everything).
//!
//! Mapping under test (from
//! `experiments/wprof_trace_baseline_20260513/REPORT.md` §5
//! and §6 F2 of the live-vs-sim report):
//!
//! | scxsim TraceKind | category       | TrackEvent type    |
//! |---|---|---|
//! | TaskScheduled    | `ONCPU`        | `TYPE_SLICE_BEGIN` |
//! | TaskSlept/etc.   | `ONCPU`        | `TYPE_SLICE_END`   |
//! | TaskWoke         | `WAKEE`        | `TYPE_INSTANT`     |
//! | DsqInsert*       | `SCX_DSQ`      | `TYPE_INSTANT`     |
//! | Tick             | `SOFTIRQ`      | `TYPE_INSTANT` (name `SOFTIRQ:timer`) |
//! | CpuIdle          | `IDLE`         | `TYPE_INSTANT`     |
//! | KickCpu          | `IPI_SEND`     | `TYPE_INSTANT` (name `IPI_SEND:single`) |

use std::io::Write as _;
use std::path::Path;

use perfetto_protos::{
    debug_annotation::debug_annotation, trace::Trace as TraceProto, trace_packet::trace_packet,
    track_event::track_event,
};
use protobuf::Message;

use scx_simulator::*;

#[macro_use]
mod common;

/// Build the same tiny scenario both tests use, run the simulator,
/// return the trace.
fn build_smoke_trace() -> Trace {
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
    Simulator::new(DynamicScheduler::simple()).run(scenario)
}

/// Round-trip a tiny scxsim trace through the same parsing primitive
/// scxtop's `load_perfetto_trace` uses, and verify the wprof event
/// vocabulary plus Perfetto wire-format invariants.
#[test]
fn test_perfetto_pb_roundtrip_wprof_compatible() {
    let _lock = common::setup_test();
    let trace = build_smoke_trace();

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
    let mut nr_softirq_timer_instant = 0usize;
    let mut nr_idle_instant = 0usize;
    let mut nr_with_scx_dsq_id_annotation = 0usize;
    let mut nr_with_cpu_annotation = 0usize;
    let mut nr_packets_missing_seq_id = 0usize;
    let mut packets_missing_seq_id_kinds: Vec<&'static str> = Vec::new();

    for packet in &proto.packet {
        // F1 invariant: every TracePacket must carry a
        // trusted_packet_sequence_id, otherwise trace_processor /
        // scxtop / ui.perfetto.dev silently drop the packet. This
        // is the regression class the live-vs-sim comparison
        // discovered post-merge.
        if packet.optional_trusted_packet_sequence_id.is_none() {
            nr_packets_missing_seq_id += 1;
            packets_missing_seq_id_kinds.push(match &packet.data {
                Some(trace_packet::Data::TrackDescriptor(_)) => "TrackDescriptor",
                Some(trace_packet::Data::TrackEvent(_)) => "TrackEvent",
                Some(_) => "OtherData",
                None => "NoData",
            });
        }

        match &packet.data {
            Some(trace_packet::Data::TrackDescriptor(_)) => {
                nr_track_descriptor += 1;
            }
            Some(trace_packet::Data::TrackEvent(ev)) => {
                nr_track_event += 1;
                let cat = ev.categories.first().map(String::as_str).unwrap_or("");
                let ev_name = ev.name_field.as_ref().and_then(|n| match n {
                    perfetto_protos::track_event::track_event::Name_field::Name(s) => {
                        Some(s.as_str())
                    }
                    _ => None,
                });
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
                    // tg `align-softirq-timer-category-naming`:
                    // post-fix the category is plain `"SOFTIRQ"` with
                    // the per-event subtype carried in the name field
                    // (matching live wprof's `SOFTIRQ:rcu` /
                    // `SOFTIRQ:hrtimer` / `SOFTIRQ:timer` /
                    // `SOFTIRQ:sched` naming convention). scxsim
                    // currently emits only the `:timer` subtype for
                    // `Tick` events, so match (category, name).
                    ("SOFTIRQ", track_event::Type::TYPE_INSTANT)
                        if ev_name == Some("SOFTIRQ:timer") =>
                    {
                        nr_softirq_timer_instant += 1
                    }
                    ("IDLE", track_event::Type::TYPE_INSTANT) => nr_idle_instant += 1,
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

    // F1 (CRITICAL): every packet — descriptors AND events — must
    // carry trusted_packet_sequence_id. The original R3 emitter
    // shipped with this field unset; trace_processor silently
    // dropped every TrackEvent and consumers saw an empty trace.
    assert_eq!(
        nr_packets_missing_seq_id,
        0,
        "{} of {} TracePackets are missing trusted_packet_sequence_id \
         (kinds: {:?}). Without this field trace_processor / scxtop / \
         ui.perfetto.dev silently drop every event — this is the F1 \
         regression class.",
        nr_packets_missing_seq_id,
        proto.packet.len(),
        packets_missing_seq_id_kinds
    );

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
    // SCX_DSQ instants and SOFTIRQ category + name `SOFTIRQ:timer`
    // instants fire on every dispatch cycle. WAKEE fires whenever
    // workload sleep/wake boundaries occur in the 50 ms window. IDLE
    // fires whenever a CPU goes idle.
    assert!(
        nr_scx_dsq_instant > 0,
        "no SCX_DSQ instant events emitted (DsqInsert mapping broken)"
    );
    assert!(
        nr_softirq_timer_instant > 0,
        "no SOFTIRQ category + name=SOFTIRQ:timer instant events emitted \
         (Tick mapping broken — see tg align-softirq-timer-category-naming)"
    );
    assert!(
        nr_wakee_instant > 0,
        "no WAKEE instant events emitted (TaskWoke mapping broken)"
    );
    assert!(
        nr_idle_instant > 0,
        "no IDLE instant events emitted (CpuIdle mapping broken)"
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

/// End-to-end ingestibility check: drive the actual Perfetto
/// `trace_processor_shell` against an emitted scxsim .pb and assert
/// `SELECT COUNT(*) FROM slice` returns a non-zero count matching
/// the in-memory ONCPU begin count.
///
/// **Why this test exists:** the original R3 emitter shipped with
/// `trusted_packet_sequence_id` unset on every TracePacket. The
/// existing wire-format round-trip test passed (parse_from_bytes
/// returns a populated `Trace` struct), but `trace_processor`
/// silently dropped all 883 k TrackEvents because they failed an
/// ingester-side schema invariant. This test catches that regression
/// class by going through the actual ingester.
///
/// **Skip behavior, and the asymmetry it creates — read before trusting a
/// green CI run of this test.** If `trace_processor_shell` is not on `$PATH`
/// or in `~/bin`, this test returns early and is reported as PASSED. The
/// GitHub Actions workflow installs only clang / llvm / libelf / xxd /
/// markdown / zlib, so **in CI this test has never actually executed** — it
/// contributes a pass to the suite total while asserting nothing. On a
/// developer machine that has the binary it does run, and provides the
/// strongest end-to-end guarantee available.
///
/// So the coverage runs BACKWARDS from the usual assumption: local runs more
/// than CI here, exactly like the ASLR gate that skipped whenever
/// `target/release/scxsim` was absent (CI never builds release, so CI never
/// ran it while developers with a stale binary did).
///
/// Two ways to close it, in preference order:
///  1. Install `trace_processor_shell` in the workflow and set
///     `SCXSIM_REQUIRE_TRACE_PROCESSOR=1` there, which turns the skip below
///     into a hard failure so the test can never silently vanish again.
///  2. Failing that, run it locally before trusting any Perfetto change.
#[test]
fn test_perfetto_pb_ingestible_by_trace_processor() {
    let _lock = common::setup_test();

    let require = std::env::var_os("SCXSIM_REQUIRE_TRACE_PROCESSOR").is_some();
    let tp = match find_trace_processor() {
        Some(p) => p,
        None => {
            assert!(
                !require,
                "SCXSIM_REQUIRE_TRACE_PROCESSOR is set but trace_processor_shell \
                 was not found on $PATH or in ~/bin — refusing to skip silently"
            );
            eprintln!(
                "SKIP: trace_processor_shell not found on $PATH or in ~/bin; \
                 install Perfetto's trace_processor_shell to enable this test. \
                 NOTE: this test is being counted as PASSED without running. \
                 Set SCXSIM_REQUIRE_TRACE_PROCESSOR=1 to make this a failure."
            );
            return;
        }
    };

    let trace = build_smoke_trace();

    // Compute the expected ONCPU-slice lower bound from the
    // in-memory trace. trace_processor's `slice` table in
    // TrackEvent mode also surfaces TYPE_INSTANT events as
    // zero-duration slices, so the actual `slice` count is greater
    // than just the BEGIN/END pair count — but it must AT LEAST
    // include every TaskScheduled ONCPU SLICE_BEGIN we emit.
    let expected_oncpu_slices = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .count();
    assert!(
        expected_oncpu_slices > 0,
        "smoke scenario produced no TaskScheduled events; test setup broken"
    );

    // Write the trace to a temp file (trace_processor_shell needs a
    // path).
    let dir = std::env::temp_dir();
    let pb_path = dir.join(format!(
        "scxsim-perfetto-pb-smoke-{}.pb",
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&pb_path)
            .unwrap_or_else(|e| panic!("create {} failed: {e}", pb_path.display()));
        trace
            .write_perfetto_pb(&mut file)
            .expect("write_perfetto_pb failed");
        file.flush().ok();
    }

    // The F1 invariant: trace_processor must see SOMETHING.
    let count_total = run_trace_processor_count(&tp, &pb_path, "SELECT COUNT(*) FROM slice")
        .unwrap_or_else(|e| {
            panic!(
                "trace_processor_shell SQL invocation failed against \
                 {}: {e}",
                pb_path.display()
            )
        });

    // The F2 invariant: filtering by category must work — each
    // TaskScheduled SLICE_BEGIN should appear under category =
    // 'ONCPU'.
    let count_oncpu = run_trace_processor_count(
        &tp,
        &pb_path,
        "SELECT COUNT(*) FROM slice WHERE category = 'ONCPU'",
    )
    .unwrap_or_else(|e| {
        panic!(
            "trace_processor_shell ONCPU-filter query failed against \
             {}: {e}",
            pb_path.display()
        )
    });

    let _ = std::fs::remove_file(&pb_path);

    assert!(
        count_total > 0,
        "trace_processor sees ZERO slices in scxsim .pb. \
         This is the F1 regression class — every packet is probably \
         missing trusted_packet_sequence_id again."
    );
    assert!(
        count_oncpu >= expected_oncpu_slices as i64,
        "trace_processor ONCPU-filter count ({count_oncpu}) is below \
         the in-memory TaskScheduled count ({expected_oncpu_slices}). \
         Either the emitter is dropping ONCPU begins or the F2 \
         category vocabulary regressed."
    );
}

/// Locate `trace_processor_shell`: prefer `$PATH`, fall back to
/// `~/bin/trace_processor_shell` (the dev-machine convention per
/// `scx-sim/CLAUDE.md` "Common tools already available").
fn find_trace_processor() -> Option<std::path::PathBuf> {
    if let Ok(path) = which("trace_processor_shell") {
        return Some(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home).join("bin/trace_processor_shell");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Lightweight `which` (avoid pulling a crate dep just for this).
fn which(bin: &str) -> std::io::Result<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PATH unset"))?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{bin} not on PATH"),
    ))
}

/// Run `trace_processor_shell -q <sql_file> <trace_path>` (the only
/// invocation form that works in stripped sandboxes — `-Q <sql>`
/// inline doesn't exist on every build) and parse the single
/// scalar count from the CSV-ish output.
fn run_trace_processor_count(tp: &Path, trace_path: &Path, sql: &str) -> Result<i64, String> {
    use std::process::Command;

    // Write the SQL to a temp file. `-q file` is the documented
    // batch form.
    let sql_path = std::env::temp_dir().join(format!("scxsim-tp-q-{}.sql", std::process::id()));
    std::fs::write(&sql_path, sql)
        .map_err(|e| format!("write {} failed: {e}", sql_path.display()))?;

    let output = Command::new(tp)
        .arg("-q")
        .arg(&sql_path)
        .arg(trace_path)
        .output()
        .map_err(|e| format!("spawn trace_processor_shell failed: {e}"))?;

    let _ = std::fs::remove_file(&sql_path);

    if !output.status.success() {
        return Err(format!(
            "trace_processor_shell exited with status {} (stderr: {})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // The CSV-ish output is:
    //   "COUNT(*)"
    //   "<integer>"
    // Find the first line that parses as a signed integer.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim().trim_matches('"');
        if let Ok(n) = trimmed.parse::<i64>() {
            return Ok(n);
        }
    }
    Err(format!(
        "trace_processor_shell returned no integer in stdout: {stdout:?}"
    ))
}
