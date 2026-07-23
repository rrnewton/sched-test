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

        // ---- task-state-transition structops (TOP-4 cluster) ------------
        TraceKind::Runnable { pid, enq_flags } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "runnable",
            "entry",
            &format!(r#""task_pid":{},"enq_flags":{}"#, pid.0, enq_flags),
            None,
        )?,
        TraceKind::Dequeue { pid, deq_flags } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "dequeue",
            "entry",
            &format!(r#""task_pid":{},"deq_flags":{}"#, pid.0, deq_flags),
            None,
        )?,
        TraceKind::Quiescent { pid, deq_flags } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "quiescent",
            "entry",
            &format!(r#""task_pid":{},"deq_flags":{}"#, pid.0, deq_flags),
            None,
        )?,

        // ---- CPU idle-tracking structop (TOP-3) -------------------------
        TraceKind::UpdateIdle {
            cpu: idle_cpu,
            idle,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "update_idle",
            "entry",
            &format!(
                r#""cpu_arg":{},"idle":{}"#,
                idle_cpu.0,
                if *idle { "true" } else { "false" },
            ),
            None,
        )?,

        // ---- cgroup-lifecycle structops (TOP-1 / TOP-2 / TOP-6) ---------
        TraceKind::CgroupInit { cgid, rc } => {
            // entry record carries the cgid:
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "structop",
                "cgroup_init",
                "entry",
                &format!(r#""cgid":{}"#, cgid.0),
                None,
            )?;
            // exit record carries rc in `ret`:
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "structop",
                "cgroup_init",
                "exit",
                "",
                Some(&rc.to_string()),
            )?;
        }
        TraceKind::CgroupExit { cgid } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "cgroup_exit",
            "entry",
            &format!(r#""cgid":{}"#, cgid.0),
            None,
        )?,
        TraceKind::CgroupSetBandwidth {
            cgid,
            period_us,
            quota_us,
            burst_us,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "cgroup_set_bandwidth",
            "entry",
            &format!(
                r#""cgid":{},"period_us":{},"quota_us":{},"burst_us":{}"#,
                cgid.0, period_us, quota_us, burst_us
            ),
            None,
        )?,
        TraceKind::CgroupMove {
            pid,
            from_cgid,
            to_cgid,
        } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "cgroup_move",
            "entry",
            &format!(
                r#""task_pid":{},"from_cgid":{},"to_cgid":{}"#,
                pid.0, from_cgid.0, to_cgid.0
            ),
            None,
        )?,

        // ---- task-lifecycle structops (TOP-5: fixture-load determinism) -
        TraceKind::InitTask { pid, rc } => {
            // entry record carries the pid:
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "structop",
                "init_task",
                "entry",
                &format!(r#""task_pid":{}"#, pid.0),
                None,
            )?;
            // exit record carries the callback rc in `ret`:
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "structop",
                "init_task",
                "exit",
                "",
                Some(&rc.to_string()),
            )?;
        }
        TraceKind::ExitTask { pid } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "exit_task",
            "entry",
            &format!(r#""task_pid":{}"#, pid.0),
            None,
        )?,
        TraceKind::Enable { pid } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "enable",
            "entry",
            &format!(r#""task_pid":{}"#, pid.0),
            None,
        )?,

        // ---- task affinity structop (TOP-7: affinity parity) -----------
        TraceKind::SetCpumask { pid, cpumask_hex } => emit_line(
            writer,
            ts,
            cpu,
            pid.0,
            "structop",
            "set_cpumask",
            "entry",
            &format!(r#""task_pid":{},"cpumask_hex":"{}""#, pid.0, cpumask_hex),
            None,
        )?,

        // ---- BPF helpers (TOP-8: time/cgroup/cpu visibility) -----------
        TraceKind::HelperNow { ret_ns } => {
            emit_line(writer, ts, cpu, 0, "helper", "now", "entry", "", None)?;
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "now",
                "exit",
                "",
                Some(&ret_ns.to_string()),
            )?;
        }
        TraceKind::HelperTaskCgroup { pid, cgid } => {
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "helper",
                "task_cgroup",
                "entry",
                &format!(r#""task_pid":{}"#, pid.0),
                None,
            )?;
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "helper",
                "task_cgroup",
                "exit",
                "",
                Some(&cgid.0.to_string()),
            )?;
        }
        TraceKind::HelperTaskCpu { pid, ret_cpu } => {
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "helper",
                "task_cpu",
                "entry",
                &format!(r#""task_pid":{}"#, pid.0),
                None,
            )?;
            emit_line(
                writer,
                ts,
                cpu,
                pid.0,
                "helper",
                "task_cpu",
                "exit",
                "",
                Some(&ret_cpu.0.to_string()),
            )?;
        }

        // ---- BPF DSQ-creation helpers (TOP-9: DSQ-creation visibility) -
        TraceKind::CreateDsq { dsq_id, node, rc } => {
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "create_dsq",
                "entry",
                &format!(r#""dsq_id":{},"node":{}"#, dsq_id.0, node),
                None,
            )?;
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "create_dsq",
                "exit",
                "",
                Some(&rc.to_string()),
            )?;
        }
        TraceKind::DestroyDsq { dsq_id } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "helper",
            "destroy_dsq",
            "entry",
            &format!(r#""dsq_id":{}"#, dsq_id.0),
            None,
        )?,
        TraceKind::DsqNrQueued { dsq_id, ret } => {
            emit_line(
                writer,
                ts,
                cpu,
                0,
                "helper",
                "dsq_nr_queued",
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
                "dsq_nr_queued",
                "exit",
                "",
                Some(&ret.to_string()),
            )?;
        }

        // ---- BTQ park/unpark structops (cpu-bw-stall-bug critical path) -
        // tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds` (A1+A2):
        // surface `cbw_put_aside` (BTQ park) + `cbw_drain_btq_batch`
        // (BTQ unpark) as structops in JSONL. Names mirror the lib
        // function names so the diff harness can pivot on the lib's
        // vocabulary directly. `count` is net delta between snapshot
        // points; `btq_len_after` is aggregate BTQ length after the
        // snapshot (the stall fingerprint is `btq_len_after` staying
        // high or growing across consecutive `cbw_put_aside` events on
        // the same cgid).
        TraceKind::CbwPutAside {
            cgid,
            count,
            btq_len_after,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "cbw_put_aside",
            "entry",
            &format!(
                r#""cgid":{},"count":{},"btq_len_after":{}"#,
                cgid.0, count, btq_len_after
            ),
            None,
        )?,
        TraceKind::CbwDrainBtqBatch {
            cgid,
            count,
            btq_len_after,
        } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "cbw_drain_btq_batch",
            "entry",
            &format!(
                r#""cgid":{},"count":{},"btq_len_after":{}"#,
                cgid.0, count, btq_len_after
            ),
            None,
        )?,
        // tg `add-cbw-throttle-cgroups-tracekind` (A3 from cgroup_bw audit):
        // surface `cbw_throttle_cgroups` (top-down throttle propagation)
        // as a structop in JSONL. Emitted on every `is_throttled` 0↔1
        // transition observed in the snapshot/diff cycle. The
        // `throttled` arg is the AFTER-snapshot value (true = newly
        // throttled, false = newly unthrottled). Name mirrors the lib
        // function `cbw_throttle_cgroups` so the diff harness can pivot
        // on the lib's vocabulary directly. See variant doc-comment for
        // the Step-1-vs-Step-2 aliasing caveat.
        TraceKind::CbwThrottleCgroups { cgid, throttled } => emit_line(
            writer,
            ts,
            cpu,
            0,
            "structop",
            "cbw_throttle_cgroups",
            "entry",
            &format!(
                r#""cgid":{},"throttled":{}"#,
                cgid.0,
                if *throttled { "true" } else { "false" },
            ),
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
        | TraceKind::FutexBoost { .. }
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

    /// tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins`:
    /// confirm the 8 new TraceKind variants emit the expected JSONL
    /// `(kind, name, phase)` tuples for the live-vs-sim diff harness.
    /// One assertion per variant; the schema-validity check is already
    /// covered by [`emit_basic_events_jsonl`] above.
    #[test]
    fn bundle_cpu_bw_easy_wins_emit_jsonl() {
        use crate::cgroup::CgroupId;

        let mut trace = Trace::with_warmup(2, &[], 0);
        trace.record(
            10,
            CpuId(0),
            TraceKind::Runnable {
                pid: Pid(7),
                enq_flags: 0x1,
            },
        );
        trace.record(
            20,
            CpuId(0),
            TraceKind::Dequeue {
                pid: Pid(7),
                deq_flags: 0x2,
            },
        );
        trace.record(
            30,
            CpuId(0),
            TraceKind::Quiescent {
                pid: Pid(7),
                deq_flags: 0x2,
            },
        );
        trace.record(
            40,
            CpuId(1),
            TraceKind::UpdateIdle {
                cpu: CpuId(1),
                idle: true,
            },
        );
        trace.record(
            50,
            CpuId(0),
            TraceKind::CgroupInit {
                cgid: CgroupId(11),
                rc: 0,
            },
        );
        trace.record(60, CpuId(0), TraceKind::CgroupExit { cgid: CgroupId(11) });
        trace.record(
            70,
            CpuId(0),
            TraceKind::CgroupSetBandwidth {
                cgid: CgroupId(11),
                period_us: 100_000,
                quota_us: 50_000,
                burst_us: 0,
            },
        );
        trace.record(
            80,
            CpuId(0),
            TraceKind::CgroupMove {
                pid: Pid(7),
                from_cgid: CgroupId(1),
                to_cgid: CgroupId(11),
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        // 7 variants emit 1 line; CgroupInit emits 2 (entry + exit).
        assert_eq!(lines.len(), 9, "lines: {:#?}", lines);

        // Schema sanity per line.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(v.get("ts_ns").is_some());
            assert!(v.get("name").is_some());
            assert!(v.get("phase").is_some());
            assert!(v.get("args").is_some());
        }

        // Per-variant (kind, name, phase) assertions:
        let find = |name: &str, phase: &str| -> Vec<&&str> {
            lines
                .iter()
                .filter(|l| {
                    l.contains(&format!(r#""name":"{name}""#))
                        && l.contains(&format!(r#""phase":"{phase}""#))
                })
                .collect()
        };

        // Runnable / Dequeue / Quiescent (TOP-4 cluster):
        let l = find("runnable", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));
        assert!(l[0].contains(r#""enq_flags":1"#));
        let l = find("dequeue", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""deq_flags":2"#));
        let l = find("quiescent", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""deq_flags":2"#));

        // UpdateIdle (TOP-3):
        let l = find("update_idle", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""cpu_arg":1"#));
        assert!(l[0].contains(r#""idle":true"#));

        // CgroupInit entry+exit (TOP-2). Exit carries `rc` in `ret`.
        let l = find("cgroup_init", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""cgid":11"#));
        let l = find("cgroup_init", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":0"#));

        // CgroupExit (TOP-2):
        let l = find("cgroup_exit", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""cgid":11"#));

        // CgroupSetBandwidth (TOP-1, the headline cpu-bw-stall-bug
        // critical-path easy-win — must carry all four configurable
        // fields verbatim, no fake approximation):
        let l = find("cgroup_set_bandwidth", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""cgid":11"#));
        assert!(l[0].contains(r#""period_us":100000"#));
        assert!(l[0].contains(r#""quota_us":50000"#));
        assert!(l[0].contains(r#""burst_us":0"#));

        // CgroupMove (TOP-6):
        let l = find("cgroup_move", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));
        assert!(l[0].contains(r#""from_cgid":1"#));
        assert!(l[0].contains(r#""to_cgid":11"#));
    }

    /// tg `bundle-implement-secondary-tracekind-easy-wins`:
    /// confirm the 10 new TraceKind variants emit the expected JSONL
    /// `(kind, name, phase)` tuples for the live-vs-sim diff harness.
    /// Mirrors the `bundle_cpu_bw_easy_wins_emit_jsonl` test above —
    /// one helper closure to extract `(name, phase)` matches, then
    /// one assertion per variant.
    #[test]
    fn bundle_secondary_easy_wins_emit_jsonl() {
        use crate::cgroup::CgroupId;

        let mut trace = Trace::with_warmup(4, &[], 0);

        // TOP-5 task-lifecycle:
        trace.record(10, CpuId(0), TraceKind::InitTask { pid: Pid(7), rc: 0 });
        trace.record(20, CpuId(0), TraceKind::ExitTask { pid: Pid(7) });
        trace.record(30, CpuId(0), TraceKind::Enable { pid: Pid(7) });

        // TOP-7 affinity:
        trace.record(
            40,
            CpuId(1),
            TraceKind::SetCpumask {
                pid: Pid(7),
                cpumask_hex: "0xf".to_string(),
            },
        );

        // TOP-8 helpers:
        trace.record(50, CpuId(2), TraceKind::HelperNow { ret_ns: 1_234_567 });
        trace.record(
            60,
            CpuId(2),
            TraceKind::HelperTaskCgroup {
                pid: Pid(7),
                cgid: CgroupId(11),
            },
        );
        trace.record(
            70,
            CpuId(2),
            TraceKind::HelperTaskCpu {
                pid: Pid(7),
                ret_cpu: CpuId(3),
            },
        );

        // TOP-9 DSQ-creation helpers:
        trace.record(
            80,
            CpuId(0),
            TraceKind::CreateDsq {
                dsq_id: DsqId(0x1100),
                node: -1,
                rc: 0,
            },
        );
        trace.record(
            90,
            CpuId(0),
            TraceKind::DestroyDsq {
                dsq_id: DsqId(0x1100),
            },
        );
        trace.record(
            100,
            CpuId(0),
            TraceKind::DsqNrQueued {
                dsq_id: DsqId(0x1100),
                ret: 5,
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();

        let find = |name: &str, phase: &str| -> Vec<&str> {
            let needle_name = format!(r#""name":"{name}""#);
            let needle_phase = format!(r#""phase":"{phase}""#);
            lines
                .iter()
                .filter(|l| l.contains(&needle_name) && l.contains(&needle_phase))
                .copied()
                .collect()
        };

        // TOP-5 init_task entry+exit (rc carried in `ret`).
        let l = find("init_task", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));
        let l = find("init_task", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":0"#));

        // TOP-5 exit_task entry-only.
        let l = find("exit_task", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));

        // TOP-5 enable entry-only.
        let l = find("enable", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));

        // TOP-7 set_cpumask carries a stable hex string.
        let l = find("set_cpumask", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));
        assert!(l[0].contains(r#""cpumask_hex":"0xf""#));

        // TOP-8 helpers — all three emit entry+exit (the rc / ret value
        // is the cpu-bw-stall-bug-relevant payload).
        let l = find("now", "entry");
        assert_eq!(l.len(), 1);
        let l = find("now", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":1234567"#));

        let l = find("task_cgroup", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""task_pid":7"#));
        let l = find("task_cgroup", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":11"#));

        let l = find("task_cpu", "entry");
        assert_eq!(l.len(), 1);
        let l = find("task_cpu", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":3"#));

        // TOP-9 DSQ-creation helpers.
        let l = find("create_dsq", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""dsq_id":4352"#)); // 0x1100
        assert!(l[0].contains(r#""node":-1"#));
        let l = find("create_dsq", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":0"#));

        let l = find("destroy_dsq", "entry");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""dsq_id":4352"#));

        let l = find("dsq_nr_queued", "entry");
        assert_eq!(l.len(), 1);
        let l = find("dsq_nr_queued", "exit");
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(r#""ret":5"#));

        // Schema sanity: every line round-trips through serde_json.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(v.get("ts_ns").is_some());
        }
    }

    /// tg `bundle-implement-secondary-tracekind-easy-wins`: smoke-test
    /// the cpumask_to_hex helper, which is the new piece that does NOT
    /// have a paired live counterpart and so isn't covered by the
    /// JSONL-emit tests above.
    #[test]
    fn cpumask_to_hex_renders_lsb_cpu_zero() {
        use crate::ffi::cpumask_to_hex;
        // NULL cpumask → "0x0".
        assert_eq!(cpumask_to_hex(std::ptr::null(), 4), "0x0");
        // nr_cpus == 0 → "0x0".
        assert_eq!(cpumask_to_hex(0xdead_beef as *const _, 0), "0x0");
        // Note: passing a real mask is the responsibility of an
        // integration test (it requires a live `bpf_cpumask` set up by
        // the C-side); the JSONL-emit assertion above covers the round
        // trip via a SetCpumask record with a hand-crafted hex string.
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

    /// tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds` (A1+A2):
    /// confirm the 2 new BTQ-flux TraceKinds emit one JSONL record
    /// each with the right name + args. Schema-validity is covered
    /// by [`emit_basic_events_jsonl`] above.
    #[test]
    fn cbw_btq_park_unpark_emit_jsonl() {
        use crate::cgroup::CgroupId;

        let mut trace = Trace::with_warmup(2, &[], 0);
        trace.record(
            100,
            CpuId(0),
            TraceKind::CbwPutAside {
                cgid: CgroupId(7),
                count: 3,
                btq_len_after: 8,
            },
        );
        trace.record(
            200,
            CpuId(0),
            TraceKind::CbwDrainBtqBatch {
                cgid: CgroupId(7),
                count: 5,
                btq_len_after: 3,
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "lines: {:#?}", lines);

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(v.get("ts_ns").is_some());
            assert!(v.get("name").is_some());
            assert!(v.get("phase").is_some());
            assert!(v.get("args").is_some());
        }

        // CbwPutAside record:
        assert!(lines[0].contains(r#""name":"cbw_put_aside""#));
        assert!(lines[0].contains(r#""phase":"entry""#));
        assert!(lines[0].contains(r#""cgid":7"#));
        assert!(lines[0].contains(r#""count":3"#));
        assert!(lines[0].contains(r#""btq_len_after":8"#));

        // CbwDrainBtqBatch record:
        assert!(lines[1].contains(r#""name":"cbw_drain_btq_batch""#));
        assert!(lines[1].contains(r#""phase":"entry""#));
        assert!(lines[1].contains(r#""cgid":7"#));
        assert!(lines[1].contains(r#""count":5"#));
        assert!(lines[1].contains(r#""btq_len_after":3"#));
    }

    /// tg `add-cbw-throttle-cgroups-tracekind` (A3 from cgroup_bw audit):
    /// confirm `CbwThrottleCgroups` emits one JSONL record per
    /// transition with the right name + args. Schema-validity is
    /// covered by `emit_basic_events_jsonl` above.
    #[test]
    fn cbw_throttle_cgroups_emits_jsonl() {
        use crate::cgroup::CgroupId;

        let mut trace = Trace::with_warmup(2, &[], 0);
        // Throttle (0→1) transition.
        trace.record(
            100,
            CpuId(0),
            TraceKind::CbwThrottleCgroups {
                cgid: CgroupId(11),
                throttled: true,
            },
        );
        // Unthrottle (1→0) transition (next replenish-period boundary).
        trace.record(
            200,
            CpuId(1),
            TraceKind::CbwThrottleCgroups {
                cgid: CgroupId(11),
                throttled: false,
            },
        );

        let mut buf = Vec::new();
        write_jsonl(&trace, &mut buf).expect("emit");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "lines: {:#?}", lines);

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(v.get("name").is_some());
            assert!(v.get("phase").is_some());
            assert!(v.get("args").is_some());
        }

        // Newly-throttled record.
        assert!(lines[0].contains(r#""name":"cbw_throttle_cgroups""#));
        assert!(lines[0].contains(r#""phase":"entry""#));
        assert!(lines[0].contains(r#""cgid":11"#));
        assert!(lines[0].contains(r#""throttled":true"#));

        // Newly-unthrottled record.
        assert!(lines[1].contains(r#""name":"cbw_throttle_cgroups""#));
        assert!(lines[1].contains(r#""cgid":11"#));
        assert!(lines[1].contains(r#""throttled":false"#));
    }
}
