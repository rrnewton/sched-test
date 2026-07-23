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
//!   IPI_SEND, OFFCPU/idle, scxsim-only ops/kfunc/cgroup_bw
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
//!   `WAKEE`, `SCX_DSQ`, `HARDIRQ`, `SOFTIRQ`,
//!   `IDLE`, `IPI_SEND`) or one of two coarse SCXSIM_*
//!   buckets — `SCXSIM_OPS` (all sched_ext ops/kfunc-level events
//!   that have no wprof counterpart) and `SCXSIM_CGROUP_BW` (the
//!   cpu-bw-stall causal channel: charge / denied / replenish).
//!   Per-event identity is preserved in the TrackEvent `name` field
//!   so SQL queries can pivot either coarsely on `category` or
//!   precisely on `name`. See the [`cat`] module for the canonical
//!   string constants.
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

/// Single packet-sequence id used for every TracePacket scxsim
/// emits.
///
/// **Why this is required**: per the Perfetto wire-format invariants
/// (see `scx/tools/scxtop/...` and the Perfetto protobuf spec at
/// <https://perfetto.dev/docs/concepts/buffers>), every TracePacket
/// MUST carry a `trusted_packet_sequence_id` so the ingester
/// (`trace_processor`, `ui.perfetto.dev`, scxtop's
/// `load_perfetto_trace`) can group packets into a coherent stream
/// and resolve sequence-scoped state (interned data, defaults, etc.).
/// Packets without this field are silently dropped by
/// `trace_processor` — the shipped R3 emitter omitted it and made
/// 100% of its TrackEvents invisible downstream.
///
/// scxsim emits a single linear stream from one logical writer, so
/// one constant id is sufficient. The value is arbitrary; `1` is
/// what wprof uses too. See
/// `experiments/wprof_live_vs_scxsim_perfetto_20260513/REPORT.md`
/// §6 F1 for the postmortem that motivated this constant.
const SCXSIM_TRUSTED_SEQ_ID: u32 = 1;

/// Stable category strings used in `TrackEvent.categories`.
///
/// The vocabulary is the union of (a) wprof's standard categories
/// for events that exist on both sides (so a single `SELECT … FROM
/// slice WHERE category = 'ONCPU'` works against either trace) and
/// (b) coarse `SCXSIM_*` category buckets for scxsim-only events.
///
/// Per the §6 F2 recommendation in the wprof_live_vs_scxsim_perfetto
/// report, scxsim-only ops/kfunc events collapse into a single
/// `SCXSIM_OPS` bucket and all cgroup_bw events collapse into a
/// single `SCXSIM_CGROUP_BW` bucket: the per-event identity is
/// preserved in the TrackEvent `name` field while the category is
/// what category-based UI filters and trace_processor SQL queries
/// pivot on.
mod cat {
    pub const ONCPU: &str = "ONCPU";
    pub const WAKEE: &str = "WAKEE";
    pub const SCX_DSQ: &str = "SCX_DSQ";
    /// IPI-send category. Live wprof emits this as the bare string
    /// `"IPI_SEND"`, with the per-event distinction (`single` vs
    /// `multi` target count) carried in the TrackEvent `name` field
    /// (e.g., `"IPI_SEND:single"`, `"IPI_SEND:multi"`). scxsim
    /// previously used `"IPI_SEND:resched"` as the category, which
    /// fragmented the live-vs-sim category vocabulary so that
    /// `SELECT DISTINCT category FROM slice` on either trace did
    /// not show overlap. Verified live-side via `trace_processor_shell
    /// -q 'SELECT name, COUNT(*) FROM slice WHERE category=...'` on
    /// `scratch/wprof_cpu_bw_stall_capture_20260513/wprof_trace.pb`:
    /// category `IPI_SEND` (204k events) carries names
    /// `IPI_SEND:single` (200k) + `IPI_SEND:multi` (4k); `:resched`
    /// only appears under the receive-side `IPI` category, not under
    /// `IPI_SEND`. tg `align-ipi-send-category-naming` (N3 follow-up
    /// to closed `rerun-stream2-comparison-after-f1-f2-land`).
    pub const IPI_SEND: &str = "IPI_SEND";
    pub const HARDIRQ: &str = "HARDIRQ";
    /// SOFTIRQ category. Live wprof emits this as the bare string
    /// `"SOFTIRQ"`, with the per-event subtype carried in the
    /// TrackEvent `name` field — verified against
    /// `scratch/wprof_cpu_bw_stall_capture_20260513/wprof_trace.pb`:
    /// category `SOFTIRQ` carries names `SOFTIRQ:rcu` (51115),
    /// `SOFTIRQ:hrtimer` (20503), `SOFTIRQ:timer` (19557),
    /// `SOFTIRQ:sched` (207). scxsim previously used the discriminator
    /// `"SOFTIRQ:timer"` AS the category, fragmenting the live-vs-sim
    /// vocabulary; the `Tick` emit now uses the bare `SOFTIRQ` constant
    /// here with name `"SOFTIRQ:timer"` carried in the TrackEvent name
    /// field. tg `align-softirq-timer-category-naming` (small
    /// follow-up to closed `align-ipi-send-category-naming`).
    pub const SOFTIRQ: &str = "SOFTIRQ";
    pub const IDLE: &str = "IDLE";
    /// All scxsim-only ops/kfunc-level events (PutPrevTask,
    /// SelectTaskRq, EnqueueTask, Balance, PickTask, SetNextTask,
    /// DsqMoveToLocal, DispatchRejected).
    pub const SCXSIM_OPS: &str = "SCXSIM_OPS";
    /// All scxsim-only cgroup-bandwidth events (Charge, Throttle,
    /// Denied, Refill, Replenish).
    pub const SCXSIM_CGROUP_BW: &str = "SCXSIM_CGROUP_BW";
}

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
                    categories: vec![cat::ONCPU.to_string()],
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
                    categories: vec![cat::WAKEE.to_string()],
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
                    categories: vec![cat::IDLE.to_string()],
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
                cat::SCX_DSQ,
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
                cat::SCX_DSQ,
                "dsq_insert_vtime",
                anns,
            );
        }

        // ----- IPI send (wprof category `IPI_SEND`, name
        //                 `IPI_SEND:single` for single-target kicks).
        //
        //       scx_bpf_kick_cpu always targets exactly one CPU, so
        //       the `:single` name applies; the `:multi` variant is
        //       reserved for future bulk-kick helpers (e.g. an IPI
        //       broadcast). The annotations preserve scxsim's
        //       sender_cpu/target_cpu identity. -----
        TraceKind::KickCpu { target_cpu } => {
            let anns = vec![
                ann_uint("sender_cpu", u64::from(cpu.0)),
                ann_uint("target_cpu", u64::from(target_cpu.0)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                cat::IPI_SEND,
                "IPI_SEND:single",
                anns,
            );
        }

        // ----- Periodic tick (wprof category `SOFTIRQ`, name
        //       `SOFTIRQ:timer` matching live wprof's per-subtype
        //       naming convention `:rcu` / `:hrtimer` / `:timer` /
        //       `:sched`). scxsim's periodic tick maps to the kernel's
        //       softirq timer subtype, so the bare `SOFTIRQ` category
        //       overlap with live traces is direct.
        //
        //       tg `align-softirq-timer-category-naming` (small
        //       follow-up to closed `align-ipi-send-category-naming`).
        //       Pre-fix scxsim emitted category `"SOFTIRQ:timer"` with
        //       name `"tick"`, fragmenting the live-vs-sim category
        //       vocabulary on the SOFTIRQ axis. -----
        TraceKind::Tick { pid } => {
            let mut anns = vec![ann_uint("cpu", u64::from(cpu.0))];
            push_task_anns(&mut anns, *pid, trace.task_name(*pid));
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                cat::SOFTIRQ,
                "SOFTIRQ:timer",
                anns,
            );
        }

        // ----- IRQ slices (wprof: HARDIRQ / SOFTIRQ).
        //       scxsim doesn't subtype softirq today; emit the bare
        //       category. -----
        TraceKind::IrqStart {
            cpu: irq_cpu,
            irq_type,
        } => {
            let category = match irq_type {
                IrqType::HardIrq => cat::HARDIRQ,
                IrqType::SoftIrq => cat::SOFTIRQ,
            };
            proto.packet.push(packet_track_event(
                TrackEvent {
                    type_: Some(track_event::Type::TYPE_SLICE_BEGIN.into()),
                    categories: vec![category.to_string()],
                    track_uuid: Some(cpu_track_uuid(*irq_cpu)),
                    name_field: Some(track_event::Name_field::Name(category.to_string())),
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_OPS,
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
                cat::SCXSIM_CGROUP_BW,
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
                cat::SCXSIM_CGROUP_BW,
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
                cat::SCXSIM_CGROUP_BW,
                "cgroup_bw_replenish",
                anns,
            );
        }
        // tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins`:
        // perfetto-pb instants for the 8 new structop hooks. Categories
        // use the `SCXSIM_STRUCTOP` namespace to match the existing
        // engine/cgroup-bw category convention.
        TraceKind::Runnable { pid, enq_flags } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_uint("enq_flags", *enq_flags),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.runnable",
                anns,
            );
        }
        TraceKind::Dequeue { pid, deq_flags } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_uint("deq_flags", *deq_flags),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.dequeue",
                anns,
            );
        }
        TraceKind::Quiescent { pid, deq_flags } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_uint("deq_flags", *deq_flags),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.quiescent",
                anns,
            );
        }
        TraceKind::UpdateIdle {
            cpu: idle_cpu,
            idle,
        } => {
            let anns = vec![
                ann_uint("cpu", u64::from(idle_cpu.0)),
                ann_uint("idle", u64::from(*idle)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.update_idle",
                anns,
            );
        }
        TraceKind::CgroupInit { cgid, rc } => {
            let anns = vec![ann_uint("cgid", cgid.0), ann_int("rc", i64::from(*rc))];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.cgroup_init",
                anns,
            );
        }
        TraceKind::CgroupExit { cgid } => {
            let anns = vec![ann_uint("cgid", cgid.0)];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.cgroup_exit",
                anns,
            );
        }
        TraceKind::CgroupSetBandwidth {
            cgid,
            period_us,
            quota_us,
            burst_us,
        } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_uint("period_us", *period_us),
                ann_uint("quota_us", *quota_us),
                ann_uint("burst_us", *burst_us),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.cgroup_set_bandwidth",
                anns,
            );
        }
        TraceKind::CgroupMove {
            pid,
            from_cgid,
            to_cgid,
        } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_uint("from_cgid", from_cgid.0),
                ann_uint("to_cgid", to_cgid.0),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.cgroup_move",
                anns,
            );
        }

        // tg `bundle-implement-secondary-tracekind-easy-wins`: perfetto-pb
        // SCXSIM_STRUCTOP instants for the 10 new structop / helper hooks.
        TraceKind::InitTask { pid, rc } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_int("rc", i64::from(*rc)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.init_task",
                anns,
            );
        }
        TraceKind::ExitTask { pid } => {
            let anns = vec![ann_int("pid", i64::from(pid.0))];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.exit_task",
                anns,
            );
        }
        TraceKind::Enable { pid } => {
            let anns = vec![ann_int("pid", i64::from(pid.0))];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.enable",
                anns,
            );
        }
        TraceKind::SetCpumask { pid, cpumask_hex } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_string("cpumask_hex", cpumask_hex.clone()),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "ops.set_cpumask",
                anns,
            );
        }
        TraceKind::HelperNow { ret_ns } => {
            let anns = vec![ann_uint("ret_ns", *ret_ns)];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_now",
                anns,
            );
        }
        TraceKind::HelperTaskCgroup { pid, cgid } => {
            let anns = vec![ann_int("pid", i64::from(pid.0)), ann_uint("cgid", cgid.0)];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_task_cgroup",
                anns,
            );
        }
        TraceKind::HelperTaskCpu { pid, ret_cpu } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_uint("ret_cpu", u64::from(ret_cpu.0)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_task_cpu",
                anns,
            );
        }
        TraceKind::CreateDsq { dsq_id, node, rc } => {
            let anns = vec![
                ann_uint("dsq_id", dsq_id.0),
                ann_int("node", i64::from(*node)),
                ann_int("rc", i64::from(*rc)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_create_dsq",
                anns,
            );
        }
        TraceKind::DestroyDsq { dsq_id } => {
            let anns = vec![ann_uint("dsq_id", dsq_id.0)];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_destroy_dsq",
                anns,
            );
        }
        TraceKind::DsqNrQueued { dsq_id, ret } => {
            let anns = vec![
                ann_uint("dsq_id", dsq_id.0),
                ann_int("ret", i64::from(*ret)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "scx_bpf_dsq_nr_queued",
                anns,
            );
        }

        // tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds` (A1+A2 from
        // cgroup_bw audit): perfetto-pb instants for BTQ park/unpark
        // events. Category `SCXSIM_STRUCTOP` matches the brief's verify
        // query and the secondary-tracekind cluster's category convention.
        // Names `cbw_put_aside` and `cbw_drain_btq_batch` mirror the lib
        // function names directly so the mechanistic-analysis SQL can
        // pivot on the lib's vocabulary.
        TraceKind::CbwPutAside {
            cgid,
            count,
            btq_len_after,
        } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_uint("count", u64::from(*count)),
                ann_uint("btq_len_after", u64::from(*btq_len_after)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "cbw_put_aside",
                anns,
            );
        }
        TraceKind::CbwDrainBtqBatch {
            cgid,
            count,
            btq_len_after,
        } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_uint("count", u64::from(*count)),
                ann_uint("btq_len_after", u64::from(*btq_len_after)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "cbw_drain_btq_batch",
                anns,
            );
        }
        // tg `add-cbw-throttle-cgroups-tracekind` (A3 from cgroup_bw audit):
        // perfetto-pb instant for top-down throttle propagation transitions.
        // Category SCXSIM_STRUCTOP matches the brief's verify query
        // convention used by A1+A2.
        TraceKind::CbwThrottleCgroups { cgid, throttled } => {
            let anns = vec![
                ann_uint("cgid", cgid.0),
                ann_uint("throttled", u64::from(*throttled)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "cbw_throttle_cgroups",
                anns,
            );
        }
        TraceKind::FutexBoost { pid, op, boosted } => {
            let anns = vec![
                ann_int("pid", i64::from(pid.0)),
                ann_string("op", format!("{op:?}")),
                ann_uint("boosted", u64::from(*boosted)),
            ];
            push_instant(
                proto,
                ts,
                cpu_track_uuid(cpu),
                "SCXSIM_STRUCTOP",
                "futex_boost",
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
            categories: vec![cat::ONCPU.to_string()],
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

/// Wrap a TrackDescriptor in a TracePacket. Stamps the
/// [`SCXSIM_TRUSTED_SEQ_ID`] (F1 fix) so `trace_processor` accepts
/// the descriptor.
fn packet_track_descriptor(desc: TrackDescriptor) -> TracePacket {
    TracePacket {
        optional_trusted_packet_sequence_id: Some(
            trace_packet::Optional_trusted_packet_sequence_id::TrustedPacketSequenceId(
                SCXSIM_TRUSTED_SEQ_ID,
            ),
        ),
        data: Some(trace_packet::Data::TrackDescriptor(desc)),
        ..TracePacket::default()
    }
}

/// Wrap a TrackEvent in a TracePacket carrying the event timestamp
/// and the [`SCXSIM_TRUSTED_SEQ_ID`] sequence id (F1 fix — without
/// it `trace_processor` silently drops every TrackEvent and the
/// trace appears empty to scxtop / Perfetto-UI).
fn packet_track_event(event: TrackEvent, timestamp_ns: u64) -> TracePacket {
    TracePacket {
        timestamp: Some(timestamp_ns),
        optional_trusted_packet_sequence_id: Some(
            trace_packet::Optional_trusted_packet_sequence_id::TrustedPacketSequenceId(
                SCXSIM_TRUSTED_SEQ_ID,
            ),
        ),
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
        | TraceKind::Runnable { pid, .. }
        | TraceKind::Dequeue { pid, .. }
        | TraceKind::Quiescent { pid, .. }
        | TraceKind::CgroupMove { pid, .. }
        | TraceKind::CgroupBwDequeueOnThrottle { pid, .. }
        | TraceKind::CgroupBwReenqueueOnReplenish { pid, .. }
        | TraceKind::LavdBailOnCgroupThrottle { pid, .. } => Some(*pid),
        // tg `bundle-implement-secondary-tracekind-easy-wins`: route the
        // pid-bearing variants of the secondary bundle through the
        // per-task track so they appear on the right thread lane in
        // perfetto-pb (matching the engine.rs/kfunc emit-site context
        // — InitTask/ExitTask/Enable on the boot CPU, SetCpumask in
        // task_init order, HelperTask{Cgroup,Cpu} on whatever CPU the
        // BPF helper fires from).
        TraceKind::InitTask { pid, .. }
        | TraceKind::ExitTask { pid }
        | TraceKind::Enable { pid }
        | TraceKind::SetCpumask { pid, .. }
        | TraceKind::HelperTaskCgroup { pid, .. }
        | TraceKind::HelperTaskCpu { pid, .. } => Some(*pid),
        TraceKind::Balance { prev_pid } => *prev_pid,
        TraceKind::CpuIdle
        | TraceKind::DsqMoveToLocal { .. }
        | TraceKind::KickCpu { .. }
        | TraceKind::IrqStart { .. }
        | TraceKind::IrqEnd { .. }
        | TraceKind::FutexBoost { .. }
        | TraceKind::CgroupBwReplenish { .. }
        | TraceKind::UpdateIdle { .. }
        | TraceKind::CgroupInit { .. }
        | TraceKind::CgroupExit { .. }
        | TraceKind::CgroupSetBandwidth { .. }
        // CPU-/global-keyed helpers: no per-task track.
        | TraceKind::HelperNow { .. }
        | TraceKind::CreateDsq { .. }
        | TraceKind::DestroyDsq { .. }
        | TraceKind::DsqNrQueued { .. }
        // tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds`: BTQ
        // park/unpark events are cgid-keyed (the lib's BTQ is per-cgroup,
        // not per-task), and the snapshot/diff helper coarsens N
        // put-asides into a single net-delta event so per-task PID is
        // not even available — route to the CPU lane instead.
        | TraceKind::CbwPutAside { .. }
        | TraceKind::CbwDrainBtqBatch { .. }
        // tg `add-cbw-throttle-cgroups-tracekind`: CbwThrottleCgroups
        // is cgid-keyed (a top-down hierarchy propagation observed
        // per-cgroup, not per-task) — route to the CPU lane.
        | TraceKind::CbwThrottleCgroups { .. }
        // V2 (`scxsim-eager-throttle-v2-track-lavd-bail-path`):
        // LavdReenqueueViaBtqDrain is a per-cgroup BTQ drain event
        // observed by the wrapper.c hook; PID is not the relevant
        // axis. Route to the CPU lane.
        | TraceKind::LavdReenqueueViaBtqDrain { .. }
        | TraceKind::CgroupBwConsumeNs { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Trace;
    use crate::types::CpuId;
    use perfetto_protos::trace::Trace as TraceProto;
    use perfetto_protos::trace_packet::trace_packet;
    use perfetto_protos::track_event::track_event;
    use protobuf::Message;

    /// tg `align-ipi-send-category-naming` (N3 follow-up to closed
    /// `rerun-stream2-comparison-after-f1-f2-land`):
    ///
    /// Live wprof emits IPI sends under category `IPI_SEND` (no
    /// `:resched` suffix), with the per-event distinction carried
    /// in the TrackEvent `name` field as `IPI_SEND:single` /
    /// `IPI_SEND:multi`. The original perfetto-pb F2 hotfix used
    /// category `IPI_SEND:resched` which fragmented the live-vs-sim
    /// vocabulary so a `SELECT DISTINCT category FROM slice` query
    /// against either trace did not show overlap on the IPI axis.
    /// This regression test pins the fix:
    ///
    /// - Build a tiny in-memory `Trace` containing one `KickCpu`.
    /// - Round-trip through `perfetto_protos::Trace::parse_from_bytes`.
    /// - Assert exactly one TrackEvent carries category `IPI_SEND`
    ///   (NOT `IPI_SEND:resched`) and name `IPI_SEND:single`,
    ///   with both `sender_cpu` and `target_cpu` annotations.
    ///
    /// Verified live-side via `trace_processor_shell -q 'SELECT name,
    /// COUNT(*) FROM slice WHERE category = ?'` on
    /// `scratch/wprof_cpu_bw_stall_capture_20260513/wprof_trace.pb`:
    ///   category `IPI_SEND` → name `IPI_SEND:single` (200 139) +
    ///   `IPI_SEND:multi` (4 087); category `IPI_SEND:resched` does
    ///   not exist in live traces.
    #[test]
    fn kick_cpu_uses_wprof_ipi_send_naming() {
        // 4-CPU trace, one KickCpu instant. (`Trace::with_warmup`
        // and `record` are pub(crate); accessible because this is a
        // lib-internal test inside the same crate as `Trace`.)
        let mut trace = Trace::with_warmup(4, &[], 0);
        trace.record(
            1_234,
            CpuId(0),
            TraceKind::KickCpu {
                target_cpu: CpuId(3),
            },
        );

        let mut buf = Vec::new();
        write_pb(&trace, &mut buf).expect("write_pb failed");

        let proto: TraceProto =
            TraceProto::parse_from_bytes(&buf).expect("parse_from_bytes failed");

        // Find the (single) IPI_SEND TrackEvent.
        let mut kick_events: Vec<&TrackEvent> = Vec::new();
        let mut all_categories: Vec<String> = Vec::new();
        for packet in &proto.packet {
            if let Some(trace_packet::Data::TrackEvent(ev)) = &packet.data {
                for c in &ev.categories {
                    all_categories.push(c.clone());
                }
                let is_ipi_send = ev
                    .categories
                    .iter()
                    .any(|c| c == "IPI_SEND" || c == "IPI_SEND:resched");
                let is_instant = ev.type_.as_ref().map(|t| t.enum_value_or_default())
                    == Some(track_event::Type::TYPE_INSTANT);
                if is_ipi_send && is_instant {
                    kick_events.push(ev);
                }
            }
        }
        assert_eq!(
            kick_events.len(),
            1,
            "expected exactly one IPI_SEND* TrackEvent, found {} \
             (categories seen: {:?})",
            kick_events.len(),
            all_categories,
        );

        let ev = kick_events[0];

        // Category MUST be plain `IPI_SEND` (matches live wprof
        // vocabulary). Pre-fix value `IPI_SEND:resched` must NOT
        // reappear — regression class for this PR.
        assert!(
            ev.categories.iter().any(|c| c == "IPI_SEND"),
            "KickCpu TrackEvent must use category 'IPI_SEND' \
             (live wprof's vocabulary); got categories {:?}",
            ev.categories,
        );
        assert!(
            !ev.categories.iter().any(|c| c == "IPI_SEND:resched"),
            "KickCpu TrackEvent regressed to category 'IPI_SEND:resched' \
             (does not match live wprof, which uses plain 'IPI_SEND' \
             with the per-event distinction in the `name` field). \
             See tg align-ipi-send-category-naming.",
        );

        // Name MUST match wprof's per-event naming:
        // `IPI_SEND:single` for single-target kicks
        // (scx_bpf_kick_cpu always targets exactly one CPU).
        let name = ev.name_field.as_ref().and_then(|n| match n {
            track_event::Name_field::Name(s) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(
            name,
            Some("IPI_SEND:single"),
            "KickCpu TrackEvent name must be 'IPI_SEND:single' \
             (matches live wprof's name vocabulary)",
        );

        // Annotations: sender_cpu + target_cpu both present.
        let mut saw_sender = false;
        let mut saw_target = false;
        for ann in &ev.debug_annotations {
            if let Some(debug_annotation::Name_field::Name(n)) = &ann.name_field {
                if n == "sender_cpu" {
                    saw_sender = true;
                }
                if n == "target_cpu" {
                    saw_target = true;
                }
            }
        }
        assert!(
            saw_sender,
            "KickCpu TrackEvent missing sender_cpu annotation",
        );
        assert!(
            saw_target,
            "KickCpu TrackEvent missing target_cpu annotation",
        );
    }

    /// tg `align-softirq-timer-category-naming` (small follow-up to
    /// closed `align-ipi-send-category-naming`):
    ///
    /// Live wprof emits SOFTIRQ events under category `"SOFTIRQ"`
    /// (no `:timer` suffix), with the per-event subtype carried in
    /// the TrackEvent `name` field — verified live-side as
    /// `SOFTIRQ:rcu` (51115) + `SOFTIRQ:hrtimer` (20503) +
    /// `SOFTIRQ:timer` (19557) + `SOFTIRQ:sched` (207) on
    /// `scratch/wprof_cpu_bw_stall_capture_20260513/wprof_trace.pb`.
    /// The pre-fix scxsim emitter used `"SOFTIRQ:timer"` AS the
    /// category (with `name="tick"`), fragmenting the live-vs-sim
    /// vocabulary so that a `SELECT DISTINCT category FROM slice`
    /// query against either trace did not show overlap on the
    /// SOFTIRQ axis. This regression test pins the fix:
    ///
    /// - Build a tiny in-memory `Trace` containing one `Tick`.
    /// - Round-trip through `parse_from_bytes`.
    /// - Assert exactly one TrackEvent carries category `"SOFTIRQ"`
    ///   (NOT `"SOFTIRQ:timer"`) and name `"SOFTIRQ:timer"`, with
    ///   the `cpu` debug_annotation present.
    #[test]
    fn tick_uses_wprof_softirq_naming() {
        use crate::trace::Trace;
        use crate::types::{CpuId, Pid};

        // 4-CPU trace, one Tick instant. (`Trace::with_warmup` and
        // `record` are pub(crate); accessible because this is a
        // lib-internal test inside the same crate as `Trace`.)
        let mut trace = Trace::with_warmup(4, &[], 0);
        trace.record(5_678, CpuId(2), TraceKind::Tick { pid: Pid(0) });

        let mut buf = Vec::new();
        write_pb(&trace, &mut buf).expect("write_pb failed");

        let proto: TraceProto =
            TraceProto::parse_from_bytes(&buf).expect("parse_from_bytes failed");

        // Find the (single) SOFTIRQ-family TrackEvent.
        let mut tick_events: Vec<&TrackEvent> = Vec::new();
        let mut all_categories: Vec<String> = Vec::new();
        for packet in &proto.packet {
            if let Some(trace_packet::Data::TrackEvent(ev)) = &packet.data {
                for c in &ev.categories {
                    all_categories.push(c.clone());
                }
                let is_softirq = ev
                    .categories
                    .iter()
                    .any(|c| c == "SOFTIRQ" || c == "SOFTIRQ:timer");
                let is_instant = ev.type_.as_ref().map(|t| t.enum_value_or_default())
                    == Some(track_event::Type::TYPE_INSTANT);
                if is_softirq && is_instant {
                    tick_events.push(ev);
                }
            }
        }
        assert_eq!(
            tick_events.len(),
            1,
            "expected exactly one SOFTIRQ* TrackEvent for one Tick, found {} \
             (categories seen: {:?})",
            tick_events.len(),
            all_categories,
        );

        let ev = tick_events[0];

        // Category MUST be plain `SOFTIRQ` (matches live wprof).
        // Pre-fix value `SOFTIRQ:timer` MUST NOT reappear —
        // regression class for this PR.
        assert!(
            ev.categories.iter().any(|c| c == "SOFTIRQ"),
            "Tick TrackEvent must use category 'SOFTIRQ' (live wprof's vocabulary); \
             got categories {:?}",
            ev.categories,
        );
        assert!(
            !ev.categories.iter().any(|c| c == "SOFTIRQ:timer"),
            "Tick TrackEvent regressed to category 'SOFTIRQ:timer' \
             (does not match live wprof, which uses plain 'SOFTIRQ' \
             with the per-event subtype in the `name` field). \
             See tg align-softirq-timer-category-naming.",
        );

        // Name MUST match wprof's per-event naming convention:
        // `SOFTIRQ:timer` for periodic-tick events.
        let name = ev.name_field.as_ref().and_then(|n| match n {
            track_event::Name_field::Name(s) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(
            name,
            Some("SOFTIRQ:timer"),
            "Tick TrackEvent name must be 'SOFTIRQ:timer' (matches live wprof's \
             SOFTIRQ subtype naming convention)",
        );
        assert_ne!(
            name,
            Some("tick"),
            "Tick TrackEvent regressed to name 'tick' — see tg \
             align-softirq-timer-category-naming for the wprof alignment.",
        );

        // Annotations: `cpu` present.
        let mut saw_cpu = false;
        for ann in &ev.debug_annotations {
            if let Some(debug_annotation::Name_field::Name(n)) = &ann.name_field {
                if n == "cpu" {
                    saw_cpu = true;
                }
            }
        }
        assert!(saw_cpu, "Tick TrackEvent missing cpu annotation");
    }
}
