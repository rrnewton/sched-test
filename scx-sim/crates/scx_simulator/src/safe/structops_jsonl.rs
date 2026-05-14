//! JSONL emitter for direct comparison with bpftrace structops/helpers
//! traces.
//!
//! Emits one JSONL record per scxsim TraceEvent, in the canonical
//! schema shared with `scripts/probes/structops_full.bt` and
//! `scripts/probes/helpers_full.bt`:
//!
//! ```json
//! {"ts_ns":<u64>,"cpu":<i32>,"pid":<i32>,
//!  "kind":"<structop|helper>","name":"<op>","phase":"<entry|exit>",
//!  "args":{...},"ret":<val|null>}
//! ```
//!
//! `scripts/compare_live_vs_scxsim_calls.sh` consumes the bpftrace JSONL
//! and the scxsim JSONL produced here and runs a side-by-side
//! sequence-diff. The two sources MUST agree on every field name and
//! ordering for the diff to be meaningful — keep this emitter and the
//! probe generator in lockstep.
//!
//! ## Coverage gap
//!
//! scxsim's `TraceKind` enum currently emits ~12 of the ~75 unique
//! call sites the bpftrace probe set covers. Events for which scxsim
//! has no `TraceKind` variant simply don't appear in the JSONL — the
//! diff harness treats those as "live-only" calls, which is the
//! correct behavior under the `No-Stub` / `Don't Model the Scheduler`
//! rules: scxsim emits only what scxsim actually causes to happen.
//!
//! Filed gaps (file as separate tg tasks):
//!  - **Structops not emitted**: dequeue, runnable, quiescent, yield,
//!    core_sched_before, set_weight, set_cpumask, update_idle,
//!    cpu_acquire, cpu_release, init_task, exit_task, enable, disable,
//!    dump*, cgroup_init/exit/prep_move/move/cancel_move/set_weight/
//!    set_bandwidth, cpu_online/offline, init, exit.
//!  - **Helpers not emitted**: most non-DSQ-insert kfuncs (dispatch,
//!    consume, dsq_move{,_vtime}, dsq_move_set_*, create_dsq,
//!    destroy_dsq, select_cpu_dfl/and, pick_*_cpu*,
//!    test_and_clear_cpu_idle, all idle/cpumask helpers, cpu_*, now,
//!    nr_*, task_*, events, reenqueue_local, *_bstr).
//!
//! Many of these are scheduler-side decisions (BPF program internals)
//! which under the No-Fake-Approximation rule the scxsim engine does
//! NOT model — they will only appear after the corresponding scheduler
//! `.bpf.c` is fully exercised inside scxsim.

use crate::scenario::IrqType;
use crate::trace::{DispatchRejectReason, Trace, TraceEvent, TraceKind};

use std::io::{self, Write};

/// Emit the trace as JSONL into `writer`. One line per `TraceEvent`.
///
/// Lines are emitted in chronological order (matches the order in
/// `Trace::events()`).
pub fn write_jsonl(trace: &Trace, writer: &mut impl Write) -> io::Result<()> {
    for event in trace.events() {
        emit_event(event, writer)?;
    }
    Ok(())
}

fn emit_event(event: &TraceEvent, writer: &mut impl Write) -> io::Result<()> {
    let ts = event.time_ns;
    let cpu = event.cpu.0 as i32;

    match &event.kind {
        // ---- structops: scheduler callbacks the engine fires ------------
        TraceKind::SelectTaskRq {
            pid,
            prev_cpu,
            selected_cpu,
        } => {
            // entry record:
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "structop",
                "select_cpu",
                "entry",
                &format!(
                    r#""task_pid":{},"prev_cpu":{},"wake_flags":0"#,
                    pid.0, prev_cpu.0
                ),
                None,
            )?;
            // exit record carries the selected cpu in `ret`:
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "structop",
                "select_cpu",
                "exit",
                "",
                Some(&selected_cpu.0.to_string()),
            )?;
        }
        TraceKind::EnqueueTask { pid, enq_flags } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "enqueue",
            "entry",
            &format!(r#""task_pid":{},"enq_flags":{}"#, pid.0, enq_flags),
            None,
        )?,
        TraceKind::Balance { prev_pid } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "dispatch",
            "entry",
            &format!(
                r#""prev_cpu":{},"prev_pid":{}"#,
                cpu,
                prev_pid.map_or(-1, |p| p.0 as i64)
            ),
            None,
        )?,
        TraceKind::SetNextTask { pid } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "running",
            "entry",
            &format!(r#""task_pid":{}"#, pid.0),
            None,
        )?,
        TraceKind::PutPrevTask {
            pid,
            still_runnable,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "stopping",
            "entry",
            &format!(
                r#""task_pid":{},"still_runnable":{}"#,
                pid.0, *still_runnable as u8
            ),
            None,
        )?,
        TraceKind::Tick { pid } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "tick",
            "entry",
            &format!(r#""task_pid":{}"#, pid.0),
            None,
        )?,

        // ---- helpers: BPF kfuncs the scheduler invokes -----------------
        TraceKind::DsqInsert { pid, dsq_id, slice } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "helper",
            "dsq_insert",
            "entry",
            &format!(
                r#""task_pid":{},"dsq_id":{},"slice":{},"enq_flags":0"#,
                pid.0, dsq_id.0, slice
            ),
            None,
        )?,
        TraceKind::DsqInsertVtime {
            pid,
            dsq_id,
            slice,
            vtime,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "helper",
            "dsq_insert_vtime",
            "entry",
            &format!(
                r#""task_pid":{},"dsq_id":{},"slice":{},"vtime":{},"enq_flags":0"#,
                pid.0, dsq_id.0, slice, vtime.0
            ),
            None,
        )?,
        TraceKind::DsqMoveToLocal { dsq_id, success } => {
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "dsq_move_to_local",
                "entry",
                &format!(r#""dsq_id":{}"#, dsq_id.0),
                None,
            )?;
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "dsq_move_to_local",
                "exit",
                "",
                Some(&(*success as u8).to_string()),
            )?;
        }
        TraceKind::KickCpu { target_cpu } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "helper",
            "kick_cpu",
            "entry",
            &format!(r#""cpu_arg":{},"flags":0"#, target_cpu.0),
            None,
        )?,

        // ---- engine-internal events: NOT structops/helpers, no JSONL ---
        // These exist in scxsim because the engine models them; bpftrace
        // can't see scxsim-internal state. Rather than fake an emit name,
        // skip — the comparison harness will note the asymmetry as
        // "scxsim-only" without polluting the structops/helpers diff.
        TraceKind::TaskScheduled { .. }
        | TraceKind::TaskPreempted { .. }
        | TraceKind::TaskYielded { .. }
        | TraceKind::TaskSlept { .. }
        | TraceKind::TaskWoke { .. }
        | TraceKind::TaskCompleted { .. }
        | TraceKind::CpuIdle
        | TraceKind::SimulationEnd { .. }
        | TraceKind::PickTask { .. }
        | TraceKind::DispatchRejected { .. }
        | TraceKind::IrqStart { .. }
        | TraceKind::IrqEnd { .. }
        | TraceKind::CgroupBwCharge { .. }
        | TraceKind::CgroupBwDenied { .. }
        | TraceKind::CgroupBwDequeueOnThrottle { .. }
        | TraceKind::CgroupBwReenqueueOnReplenish { .. }
        | TraceKind::CgroupBwReplenish { .. }
        | TraceKind::LavdBailOnCgroupThrottle { .. }
        | TraceKind::LavdReenqueueViaBtqDrain { .. }
        | TraceKind::CgroupBwConsumeNs { .. } => {}
    }

    // Suppress unused-variable warnings for IrqType / DispatchRejectReason
    // brought in by the `use` statements (kept for forward compatibility
    // when those variants gain JSONL emit).
    let _ = (IrqType::HardIrq, DispatchRejectReason::CpumaskViolation);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_line(
    writer: &mut impl Write,
    ts_ns: u64,
    cpu: i32,
    pid: i32,
    kind: &str,
    name: &str,
    phase: &str,
    args_inner: &str,
    ret: Option<&str>,
) -> io::Result<()> {
    let args_blob = if args_inner.is_empty() {
        "{}".to_string()
    } else {
        format!("{{{}}}", args_inner)
    };
    let ret_blob = match ret {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    };
    writeln!(
        writer,
        r#"{{"ts_ns":{ts_ns},"cpu":{cpu},"pid":{pid},"kind":"{kind}","name":"{name}","phase":"{phase}","args":{args_blob},"ret":{ret_blob}}}"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::IrqType;
    use crate::trace::{Trace, TraceKind};
    use crate::types::{CpuId, DsqId, Pid, Vtime};

    /// Confirm the emitter produces well-formed JSONL: each line starts
    /// with `{"ts_ns":` and ends with `}\n`, with no trailing whitespace.
    #[test]
    fn emit_basic_events_jsonl() {
        let mut trace = Trace::with_warmup(2, &[], 0);
        trace.record(
            1_000,
            CpuId(0),
            TraceKind::EnqueueTask {
                pid: Pid(42),
                enq_flags: 0x10,
            },
        );
        trace.record(
            2_000,
            CpuId(1),
            TraceKind::DsqInsertVtime {
                pid: Pid(42),
                dsq_id: DsqId(0xff),
                slice: 5_000_000,
                vtime: Vtime(123456789),
            },
        );
        trace.record(
            3_000,
            CpuId(0),
            TraceKind::KickCpu {
                target_cpu: CpuId(3),
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.starts_with(r#"{"ts_ns":"#), "bad prefix: {line}");
            assert!(line.ends_with('}'), "bad suffix: {line}");
            // basic JSON validity check via serde_json round-trip
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(v.get("ts_ns").is_some());
            assert!(v.get("cpu").is_some());
            assert!(v.get("kind").is_some());
            assert!(v.get("name").is_some());
            assert!(v.get("phase").is_some());
            assert!(v.get("args").is_some());
            assert!(v.get("ret").is_some());
        }
        assert!(lines[0].contains(r#""name":"enqueue""#));
        assert!(lines[1].contains(r#""name":"dsq_insert_vtime""#));
        assert!(lines[2].contains(r#""name":"kick_cpu""#));

        // Suppress unused-import warning when running this test in isolation
        let _ = IrqType::HardIrq;
    }

    /// Engine-internal events are correctly skipped (no JSONL emitted).
    #[test]
    fn engine_internal_events_skipped() {
        let mut trace = Trace::with_warmup(1, &[], 0);
        trace.record(100, CpuId(0), TraceKind::TaskScheduled { pid: Pid(1) });
        trace.record(200, CpuId(0), TraceKind::CpuIdle);
        trace.record(
            300,
            CpuId(0),
            TraceKind::CgroupBwReplenish {
                cgid: crate::cgroup::CgroupId(7),
                runtime_total_last: 0,
                period_budget_in: 0,
                debt: 0,
                burst_credit: 0,
                period_budget_out: 0,
                keep_throttled: false,
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        assert!(buf.is_empty(), "engine-internal events should not emit");
    }

    /// SelectTaskRq emits BOTH entry and exit records, with the
    /// selected_cpu carried in `ret`.
    #[test]
    fn select_cpu_emits_entry_and_exit() {
        let mut trace = Trace::with_warmup(4, &[], 0);
        trace.record(
            500,
            CpuId(2),
            TraceKind::SelectTaskRq {
                pid: Pid(99),
                prev_cpu: CpuId(1),
                selected_cpu: CpuId(3),
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""phase":"entry""#));
        assert!(lines[0].contains(r#""ret":null"#));
        assert!(lines[1].contains(r#""phase":"exit""#));
        assert!(lines[1].contains(r#""ret":3"#));
    }
}
