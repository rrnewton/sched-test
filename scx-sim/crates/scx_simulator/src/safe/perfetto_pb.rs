//! Perfetto protobuf TrackEvent writer for scxsim, wprof-compatible.
//!
//! Writes the simulation trace as a binary perfetto `Trace` protobuf
//! using `TrackEvent` slices and instants whose category names and
//! `debug_annotations` match the vocabulary that wprof emits, so that
//! scxtop's `load_perfetto_trace` (and its `parse_oncpu_slice` /
//! `parse_wakeup_instant` / `parse_track_event` helpers) can ingest
//! scxsim and wprof traces side-by-side.
//!
//! See the wprof-trace-baseline report
//! (`experiments/wprof_trace_baseline_20260513/REPORT.md`) §3 for the
//! authoritative wprof event vocabulary this writer targets, and the
//! parser at `scx/tools/scxtop/src/mcp/perfetto_track_event_types.rs`
//! for the receiving end. The mapping table this writer implements is
//! the §5 `scxsim ↔ wprof event mapping table` from that report.
//!
//! ## Track layout
//!
//! - One synthetic `scxsim` process descriptor (the parent of every
//!   thread track in the trace).
//! - One thread track per task PID. ONCPU slice begin/end pairs and
//!   per-task instants (WAKEE, SCX_DSQ) live on this track. The
//!   thread name is the scxsim task comm.
//! - One thread track per CPU representing the per-CPU "scheduler
//!   substrate". CPU-keyed instants (TIMER tick, HARDIRQ/SOFTIRQ,
//!   IPI_SEND:resched, OFFCPU/idle, scxsim-only ops/kfunc/cgroup_bw
//!   instants) live on this track. CPU lanes use the wprof
//!   `swapper/N → tid -(N+1)` idle-thread convention so that scxtop's
//!   wprof loader recognizes the lanes as CPU lanes rather than as
//!   regular tasks.
//!
//! ## TrackEvent shape
//!
//! Each scheduling action becomes one `TracePacket` with `timestamp`
//! set in nanoseconds (scxsim's logical clock starts at 0, which is
//! also wprof's session-relative origin per
//! `WPROF_COMPATIBILITY_GUIDE.md` §"Timestamp Format"). The packet's
//! `data` is a `TrackEvent` whose:
//!
//! - `type_` is `TYPE_SLICE_BEGIN` / `TYPE_SLICE_END` / `TYPE_INSTANT`.
//! - `categories` carries one of wprof's category strings (`ONCPU`,
//!   `WAKEE`, `SCX_DSQ`, `TIMER`, `HARDIRQ`, `SOFTIRQ`, `OFFCPU`,
//!   `IPI_SEND:resched`) or, for events with no wprof counterpart, a
//!   `SCXSIM_*` category (e.g. `SCXSIM_CGBW_CHARGE`,
//!   `SCXSIM_DISPATCH_REJECTED`).
//! - `track_uuid` is the per-task or per-CPU track this event belongs
//!   to (deterministic from PID / CPU index — see [`task_track_uuid`]
//!   and [`cpu_track_uuid`]).
//! - `debug_annotations` carry wprof's standard keys (`cpu`, `pid`,
//!   `tid`, `comm`, `scx_dsq_id`, `target_cpu`, etc.) plus scxsim
//!   extensions for events without a wprof counterpart.

use std::io::Write;

use perfetto_protos::{
    debug_annotation::{debug_annotation, DebugAnnotation},
    process_descriptor::ProcessDescriptor,
    thread_descriptor::ThreadDescriptor,
    trace::Trace as TraceProto,
    trace_packet::{trace_packet, TracePacket},
    track_descriptor::TrackDescriptor,
    track_event::{track_event, TrackEvent},
};
use protobuf::{Message, MessageField};

use crate::scenario::IrqType;
use crate::trace::{DispatchRejectReason, Trace, TraceKind};
use crate::types::{CpuId, DsqId, Pid};

/// Stable UUID of the synthetic `scxsim` process descriptor that
/// parents every thread track in the trace.
const SCXSIM_PROCESS_UUID: u64 = 0x5C50_0000_0000_0001;

/// Synthetic PID exposed in the `scxsim` ProcessDescriptor. Distinct
/// from any task pid (scxsim task pids are small positive integers
/// from rt-app-style fixtures).
const SCXSIM_PROCESS_PID: i32 = 0x5C51_5C51;

/// Build the deterministic per-task TrackDescriptor uuid.
///
/// High bits 0x5C50_0100_…  are the "scxsim task track" namespace; the
/// low 32 bits hold the PID. This avoids any collision with the
/// per-CPU namespace and with [`SCXSIM_PROCESS_UUID`].
#[inline]
fn task_track_uuid(pid: Pid) -> u64 {
    // Cast to u32 first to truncate the sign-extended bits without
    // colliding with the per-CPU namespace (task pids are positive
    // small integers in scxsim fixtures, but the Pid newtype is i32
    // for kernel parity).
    0x5C50_0100_0000_0000 | u64::from(pid.0 as u32)
}

/// Build the deterministic per-CPU TrackDescriptor uuid.
#[inline]
fn cpu_track_uuid(cpu: CpuId) -> u64 {
    0x5C50_0200_0000_0000 | u64::from(cpu.0)
}

/// Wprof-style idle-thread tid for a given CPU: `swapper/N → -(N+1)`.
///
/// See `scx/tools/scxtop/WPROF_COMPATIBILITY_GUIDE.md` §"Idle Thread
/// Handling".
#[inline]
fn cpu_swapper_tid(cpu: CpuId) -> i32 {
    -(cpu.0 as i32 + 1)
}

/// Write the trace as a Perfetto Trace protobuf, wprof-compatible.
///
/// All event payloads are appended to a single in-memory `Trace`
/// message, then serialized once at the end. The total packet count
/// is bounded by `2 * nr_cpus + nr_tasks + trace.events().len()`,
/// which matches the JSON writer's bound; the per-event allocation
/// is dominated by the `TrackEvent` itself.
pub(crate) fn write_pb(trace: &Trace, writer: &mut impl Write) -> std::io::Result<()> {
    let mut proto = TraceProto::new();

    // 1. Process descriptor for the synthetic scxsim process.
    proto.packet.push(packet_track_descriptor(TrackDescriptor {
        uuid: Some(SCXSIM_PROCESS_UUID),
        process: MessageField::some(ProcessDescriptor {
            pid: Some(SCXSIM_PROCESS_PID),
            process_name: Some("scxsim".to_string()),
            ..ProcessDescriptor::default()
        }),
        ..TrackDescriptor::default()
    }));

    // 2. One CPU track per CPU (using wprof's idle-thread tid
    //    convention so scxtop recognizes the lane as a CPU lane).
    for cpu_idx in 0..trace.nr_cpus {
        let cpu = CpuId(cpu_idx);
        proto.packet.push(packet_track_descriptor(TrackDescriptor {
            uuid: Some(cpu_track_uuid(cpu)),
            parent_uuid: Some(SCXSIM_PROCESS_UUID),
            thread: MessageField::some(ThreadDescriptor {
                pid: Some(SCXSIM_PROCESS_PID),
                tid: Some(cpu_swapper_tid(cpu)),
                thread_name: Some(format!("swapper/{cpu_idx}")),
                ..ThreadDescriptor::default()
            }),
            ..TrackDescriptor::default()
        }));
    }

    // 3. One task track per known PID. Using `task_name` to populate
    //    the thread name matches the JSON writer's behavior so the
    //    Perfetto UI shows the same lane labels for both formats.
    for pid in collect_task_pids(trace) {
        let comm = trace.task_name(pid).to_string();
        proto.packet.push(packet_track_descriptor(TrackDescriptor {
            uuid: Some(task_track_uuid(pid)),
            parent_uuid: Some(SCXSIM_PROCESS_UUID),
            thread: MessageField::some(ThreadDescriptor {
                pid: Some(SCXSIM_PROCESS_PID),
                tid: Some(pid.0),
                thread_name: Some(comm),
                ..ThreadDescriptor::default()
            }),
            ..TrackDescriptor::default()
        }));
    }

    // 4. Per-event TrackEvent packets, in chronological order.
    for ev in trace.events() {
        emit_event(trace, ev, &mut proto);
    }

    // 5. Serialize once and stream out.
    let bytes = proto
        .write_to_bytes()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_all(&bytes)
}

// ---------------------------------------------------------------------------
// Per-event emission (one match arm per TraceKind, mirroring perfetto.rs).
// ---------------------------------------------------------------------------

fn emit_event(trace: &Trace, ev: &crate::trace::TraceEvent, proto: &mut TraceProto) {
    let ts = ev.time_ns;
    let cpu = ev.cpu;
    match &ev.kind {
        // ----- ONCPU slice begin -----
        TraceKind::TaskScheduled { pid } => {
            let comm = trace.task_name(*pid).to_string();
            let mut anns = vec![ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, &comm);
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_SLICE_BEGIN.into()),
                    categories: vec!["ONCPU".to_string()],
                    track_uuid: Some(task_track_uuid(*pid)),
                    name_field: Some(track_event::Name_field::Name(comm)),
                    debug_annotations: anns,
                    ..TrackEvent::default()
                },
                ts,
            ));
        }

        // ----- ONCPU slice end variants (all close the per-task slice) -----
        TraceKind::TaskPreempted { pid } => {
            push_oncpu_end(proto, ts, cpu, *pid, "preempted");
        }
        TraceKind::TaskYielded { pid } => {
            push_oncpu_end(proto, ts, cpu, *pid, "yielded");
        }
        TraceKind::TaskSlept { pid } => {
            push_oncpu_end(proto, ts, cpu, *pid, "slept");
        }
        TraceKind::TaskCompleted { pid } => {
            push_oncpu_end(proto, ts, cpu, *pid, "completed");
        }
        TraceKind::SimulationEnd { pid } => {
            push_oncpu_end(proto, ts, cpu, *pid, "sim_end");
        }

        // ----- Wakeup (wprof: WAKEE instant; waker side is R5 gap) -----
        TraceKind::TaskWoke { pid } => {
            // TODO(sim-wprof-r5): plumb waker_pid + prev_cpu from the
            // engine so we can also emit the matching WAKER instant.
            // Until then, emit only the WAKEE side and leave waker_pid
            // unset — matches the §6.2 gap analysis in the
            // wprof-trace-baseline report.
            let comm = trace.task_name(*pid).to_string();
            let anns = vec![
                ann_uint("target_cpu", u64::from(cpu.0)),
                ann_int("wakee_pid", i64::from(pid.0)),
                ann_int("wakee_tid", i64::from(pid.0)),
                ann_string("wakee_comm", comm.clone()),
            ];
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_INSTANT.into()),
                    categories: vec!["WAKEE".to_string()],
                    track_uuid: Some(task_track_uuid(*pid)),
                    name_field: Some(track_event::Name_field::Name("WAKEE".to_string())),
                    debug_annotations: anns,
                    ..TrackEvent::default()
                },
                ts,
            ));
        }

        // ----- CpuIdle: wprof models idle as the swapper thread; we
        //       emit an OFFCPU instant on the CPU lane to mark the
        //       boundary. -----
        TraceKind::CpuIdle => {
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_INSTANT.into()),
                    categories: vec!["OFFCPU".to_string()],
                    track_uuid: Some(cpu_track_uuid(cpu)),
                    name_field: Some(track_event::Name_field::Name("CPU_IDLE".to_string())),
                    debug_annotations: vec![ann_uint("cpu", u64::from(cpu.0))],
                    ..TrackEvent::default()
                },
                ts,
            ));
        }

        // ----- DSQ events (wprof: SCX_DSQ category with scx_dsq_id) -----
        TraceKind::DsqInsert { pid, dsq_id, slice } => {
            let mut anns = vec![
                ann_uint("cpu", u64::from(cpu.0)),
                ann_uint("scx_dsq_id", dsq_id.0),
                ann_string("scx_dsq", format_dsq_id(*dsq_id)),
                ann_uint("slice_ns", *slice),
            ];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                task_track_uuid(*pid),
                "SCX_DSQ",
                "dsq_insert",
                anns,
            );
        }
        TraceKind::DsqInsertVtime {
            pid,
            dsq_id,
            slice,
            vtime,
        } => {
            let mut anns = vec![
                ann_uint("cpu", u64::from(cpu.0)),
                ann_uint("scx_dsq_id", dsq_id.0),
                ann_string("scx_dsq", format_dsq_id(*dsq_id)),
                ann_uint("slice_ns", *slice),
                ann_uint("vtime", vtime.0),
            ];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                task_track_uuid(*pid),
                "SCX_DSQ",
                "dsq_insert_vtime",
                anns,
            );
        }

        // ----- IPI send (wprof: IPI_SEND:resched with target_cpu) -----
        TraceKind::KickCpu { target_cpu } => {
            let anns = vec![
                ann_uint("sender_cpu", u64::from(cpu.0)),
                ann_uint("target_cpu", u64::from(target_cpu.0)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "IPI_SEND:resched",
                "kick_cpu",
                anns,
            );
        }

        // ----- Periodic tick (wprof: TIMER) -----
        TraceKind::Tick { pid } => {
            let mut anns = vec![ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(proto, ts, cpu_track_uuid(cpu), "TIMER", "tick", anns);
        }

        // ----- IRQ slices (wprof: HARDIRQ / SOFTIRQ).
        //       scxsim doesn't subtype softirq today; emit the bare
        //       category. -----
        TraceKind::IrqStart {
            cpu: irq_cpu,
            irq_type,
        } => {
            let cat = match irq_type {
                IrqType::HardIrq => "HARDIRQ",
                IrqType::SoftIrq => "SOFTIRQ",
            };
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_SLICE_BEGIN.into()),
                    categories: vec![cat.to_string()],
                    track_uuid: Some(cpu_track_uuid(*irq_cpu)),
                    name_field: Some(track_event::Name_field::Name(cat.to_string())),
                    debug_annotations: vec![ann_uint("cpu", u64::from(irq_cpu.0))],
                    ..TrackEvent::default()
                },
                ts,
            ));
        }
        TraceKind::IrqEnd { cpu: irq_cpu } => {
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_SLICE_END.into()),
                    track_uuid: Some(cpu_track_uuid(*irq_cpu)),
                    debug_annotations: vec![ann_uint("cpu", u64::from(irq_cpu.0))],
                    ..TrackEvent::default()
                },
                ts,
            ));
        }

        // ----- Ops-level events with no wprof counterpart: emit on
        //       the CPU lane under SCXSIM_* categories so the wprof
        //       parser ignores them but a dedicated scxsim analyzer
        //       (or a human eyeballing perfetto.dev) can find them. -----
        TraceKind::PutPrevTask {
            pid,
            still_runnable,
        } => {
            let mut anns = vec![ann_bool("still_runnable", *still_runnable)];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_PUT_PREV_TASK",
                "put_prev_task",
                anns,
            );
        }
        TraceKind::SelectTaskRq {
            pid,
            prev_cpu,
            selected_cpu,
        } => {
            let mut anns = vec![
                ann_uint("prev_cpu", u64::from(prev_cpu.0)),
                ann_uint("selected_cpu", u64::from(selected_cpu.0)),
            ];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_SELECT_TASK_RQ",
                "select_task_rq",
                anns,
            );
        }
        TraceKind::EnqueueTask { pid, enq_flags } => {
            let mut anns = vec![ann_uint("enq_flags", *enq_flags)];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_ENQUEUE_TASK",
                "enqueue_task",
                anns,
            );
        }
        TraceKind::Balance { prev_pid } => {
            let mut anns = Vec::with_capacity(1);
            if let Some(p) = prev_pid {
                anns.push(ann_int("prev_pid", i64::from(p.0)));
            }
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_BALANCE",
                "balance",
                anns,
            );
        }
        TraceKind::PickTask { pid } => {
            let mut anns = Vec::new();
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_PICK_TASK",
                "pick_task",
                anns,
            );
        }
        TraceKind::SetNextTask { pid } => {
            let mut anns = Vec::new();
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_SET_NEXT_TASK",
                "set_next_task",
                anns,
            );
        }
        TraceKind::DsqMoveToLocal { dsq_id, success } => {
            let anns = vec![
                ann_uint("scx_dsq_id", dsq_id.0),
                ann_string("scx_dsq", format_dsq_id(*dsq_id)),
                ann_bool("success", *success),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_DSQ_MOVE_TO_LOCAL",
                "dsq_move_to_local",
                anns,
            );
        }
        TraceKind::DispatchRejected {
            pid,
            target_cpu,
            reason,
        } => {
            let reason_str = match reason {
                DispatchRejectReason::CpumaskViolation => "cpumask_violation",
                DispatchRejectReason::MigrationDisabled => "migration_disabled",
            };
            let mut anns = vec![
                ann_uint("target_cpu", u64::from(target_cpu.0)),
                ann_string("reason", reason_str.to_string()),
            ];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_DISPATCH_REJECTED",
                "dispatch_rejected",
                anns,
            );
        }

        // ----- Cgroup-bandwidth (the cpu-bw-stall causal channel; no
        //       wprof counterpart per §5.4 of the baseline report). -----
        TraceKind::CgroupBwCharge {
            pid,
            cgid,
            delta_ns,
        } => {
            let mut anns = vec![
                ann_uint("cpu", u64::from(cpu.0)),
                ann_uint("cgid", cgid.0),
                ann_uint("delta_ns", *delta_ns),
            ];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_CHARGE",
                "cgroup_bw_charge",
                anns,
            );
        }
        TraceKind::CgroupBwDenied { pid, cgid } => {
            let mut anns = vec![ann_uint("cgid", cgid.0), ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_DENIED",
                "cgroup_bw_denied",
                anns,
            );
        }
        TraceKind::CgroupBwDequeueOnThrottle { pid, cgid } => {
            let mut anns = vec![ann_uint("cgid", cgid.0), ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_DEQ_THR",
                "cgroup_bw_dequeue_on_throttle",
                anns,
            );
        }
        TraceKind::CgroupBwReenqueueOnReplenish { pid, cgid } => {
            let mut anns = vec![ann_uint("cgid", cgid.0), ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_REENQ_RPL",
                "cgroup_bw_reenqueue_on_replenish",
                anns,
            );
        }
        TraceKind::LavdBailOnCgroupThrottle { pid, cgid } => {
            let mut anns = vec![ann_uint("cgid", cgid.0), ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_LAVD_BAIL_CGT",
                "lavd_bail_on_cgroup_throttle",
                anns,
            );
        }
        TraceKind::LavdReenqueueViaBtqDrain { cgid } => {
            let anns = vec![ann_uint("cgid", cgid.0), ann_uint("cpu", u64::from(cpu.0))];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_LAVD_REENQ_BTQ",
                "lavd_reenqueue_via_btq_drain",
                anns,
            );
        }
        TraceKind::CgroupBwConsumeNs { cgid, ns } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_uint("ns", *ns),
                ann_uint("cpu", u64::from(cpu.0)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_CONSUME",
                "cgroup_bw_consume",
                anns,
            );
        }
        TraceKind::CbwAccountingTimerFired {
            slot,
            period_ns_since_last_arm,
            requested_period_ns,
        } => {
            let anns = vec![
                ann_uint("slot", u64::from(*slot)),
                ann_uint("period_ns_since_last_arm", *period_ns_since_last_arm),
                ann_uint("requested_period_ns", *requested_period_ns),
                ann_uint("cpu", u64::from(cpu.0)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CBW_AC_TIMER",
                "cbw_accounting_timer_fired",
                anns,
            );
        }
        TraceKind::CgroupBwReplenish {
            cgid,
            runtime_total_last,
            period_budget_in,
            debt,
            burst_credit,
            period_budget_out,
            keep_throttled,
        } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_int("runtime_total_last", *runtime_total_last),
                ann_int("period_budget_in", *period_budget_in),
                ann_int("debt", *debt),
                ann_int("burst_credit", *burst_credit),
                ann_int("period_budget_out", *period_budget_out),
                ann_uint("keep_throttled", u64::from(*keep_throttled)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_CGBW_REPLENISH",
                "cgroup_bw_replenish",
                anns,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Close an ONCPU slice for `pid` with an `offcpu_reason` annotation.
fn push_oncpu_end(proto: &mut TraceProto, ts: u64, cpu: CpuId, pid: Pid, offcpu_reason: &str) {
    let anns = vec![
        ann_uint("cpu", u64::from(cpu.0)),
        ann_int("pid", i64::from(pid.0)),
        ann_int("tid", i64::from(pid.0)),
        ann_string("offcpu_reason", offcpu_reason.to_string()),
    ];
    proto.packet.push(packet_track_event(
        TrackEvent {
            type_: Some(track_event::Type::TYPE_SLICE_END.into()),
            categories: vec!["ONCPU".to_string()],
            track_uuid: Some(task_track_uuid(pid)),
            debug_annotations: anns,
            ..TrackEvent::default()
        },
        ts,
    ));
}

/// Push an INSTANT TrackEvent on `track_uuid` with a category, name,
/// and pre-built annotation list.
fn push_instant(
    proto: &mut TraceProto,
    ts: u64,
    track_uuid: u64,
    category: &str,
    name: &str,
    annotations: Vec<DebugAnnotation>,
) {
    proto.packet.push(packet_track_event(
        TrackEvent {
            type_: Some(track_event::Type::TYPE_INSTANT.into()),
            categories: vec![category.to_string()],
            track_uuid: Some(track_uuid),
            name_field: Some(track_event::Name_field::Name(name.to_string())),
            debug_annotations: annotations,
            ..TrackEvent::default()
        },
        ts,
    ));
}

/// Wrap a TrackDescriptor in a TracePacket.
fn packet_track_descriptor(desc: TrackDescriptor) -> TracePacket {
    TracePacket {
        data: Some(trace_packet::Data::TrackDescriptor(desc)),
        ..TracePacket::default()
    }
}

/// Wrap a TrackEvent in a TracePacket carrying the event timestamp.
fn packet_track_event(event: TrackEvent, timestamp_ns: u64) -> TracePacket {
    TracePacket {
        timestamp: Some(timestamp_ns),
        data: Some(trace_packet::Data::TrackEvent(event)),
        ..TracePacket::default()
    }
}

/// Append the standard wprof `pid` / `tid` / `comm` annotations.
fn push_task_anns(anns: &mut Vec<DebugAnnotation>, pid: Pid, comm: &str) {
    anns.push(ann_int("pid", i64::from(pid.0)));
    anns.push(ann_int("tid", i64::from(pid.0)));
    anns.push(ann_string("comm", comm.to_string()));
}

fn ann_uint(name: &str, v: u64) -> DebugAnnotation {
    DebugAnnotation {
        name_field: Some(debug_annotation::Name_field::Name(name.to_string())),
        value: Some(debug_annotation::Value::UintValue(v)),
        ..DebugAnnotation::default()
    }
}

fn ann_int(name: &str, v: i64) -> DebugAnnotation {
    DebugAnnotation {
        name_field: Some(debug_annotation::Name_field::Name(name.to_string())),
        value: Some(debug_annotation::Value::IntValue(v)),
        ..DebugAnnotation::default()
    }
}

fn ann_bool(name: &str, v: bool) -> DebugAnnotation {
    DebugAnnotation {
        name_field: Some(debug_annotation::Name_field::Name(name.to_string())),
        value: Some(debug_annotation::Value::BoolValue(v)),
        ..DebugAnnotation::default()
    }
}

fn ann_string(name: &str, v: String) -> DebugAnnotation {
    DebugAnnotation {
        name_field: Some(debug_annotation::Name_field::Name(name.to_string())),
        value: Some(debug_annotation::Value::StringValue(v)),
        ..DebugAnnotation::default()
    }
}

/// Format a DSQ ID to a stable string for the `scx_dsq` annotation.
///
/// Mirrors the formatting in `perfetto.rs::format_dsq_id` so the JSON
/// and PB writers display DSQ ids identically.
fn format_dsq_id(dsq_id: DsqId) -> String {
    if dsq_id == DsqId::GLOBAL {
        "GLOBAL".to_string()
    } else if dsq_id.is_local() {
        "LOCAL".to_string()
    } else if dsq_id.is_local_on() {
        format!("LOCAL_ON({})", dsq_id.local_on_cpu().0)
    } else {
        format!("{:#x}", dsq_id.0)
    }
}

/// Collect the set of distinct PIDs that appear as a per-task event
/// subject in the trace. Used to build per-task TrackDescriptors so
/// every track_uuid referenced from a TrackEvent has a matching
/// descriptor, which the wprof loader requires.
fn collect_task_pids(trace: &Trace) -> Vec<Pid> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<i32> = BTreeSet::new();
    for ev in trace.events() {
        if let Some(pid) = event_pid(&ev.kind) {
            set.insert(pid.0);
        }
    }
    set.into_iter().map(Pid).collect()
}

/// Return the per-task PID an event refers to, if any. Events that
/// only carry a CPU (CpuIdle, IRQ, Balance with no prev_pid) return
/// `None`. Used by [`collect_task_pids`].
fn event_pid(kind: &TraceKind) -> Option<Pid> {
    match kind {
        TraceKind::TaskScheduled { pid }
        | TraceKind::TaskPreempted { pid }
        | TraceKind::TaskYielded { pid }
        | TraceKind::TaskSlept { pid }
        | TraceKind::TaskWoke { pid }
        | TraceKind::TaskCompleted { pid }
        | TraceKind::SimulationEnd { pid }
        | TraceKind::PutPrevTask { pid, .. }
        | TraceKind::SelectTaskRq { pid, .. }
        | TraceKind::EnqueueTask { pid, .. }
        | TraceKind::PickTask { pid }
        | TraceKind::SetNextTask { pid }
        | TraceKind::DsqInsert { pid, .. }
        | TraceKind::DsqInsertVtime { pid, .. }
        | TraceKind::DispatchRejected { pid, .. }
        | TraceKind::Tick { pid }
        | TraceKind::CgroupBwCharge { pid, .. }
        | TraceKind::CgroupBwDenied { pid, .. }
        | TraceKind::CgroupBwDequeueOnThrottle { pid, .. }
        | TraceKind::CgroupBwReenqueueOnReplenish { pid, .. }
        | TraceKind::LavdBailOnCgroupThrottle { pid, .. } => Some(*pid),
        TraceKind::Balance { prev_pid } => *prev_pid,
        TraceKind::CpuIdle
        | TraceKind::DsqMoveToLocal { .. }
        | TraceKind::KickCpu { .. }
        | TraceKind::IrqStart { .. }
        | TraceKind::IrqEnd { .. }
        | TraceKind::CgroupBwReplenish { .. }
        | TraceKind::LavdReenqueueViaBtqDrain { .. }
        | TraceKind::CgroupBwConsumeNs { .. }
        | TraceKind::CbwAccountingTimerFired { .. } => None,
    }
}
