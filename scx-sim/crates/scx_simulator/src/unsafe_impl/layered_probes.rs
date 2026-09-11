//! scx_layered probe accessors and monitor.
//!
//! [`LayeredProbes`] resolves function pointers from the loaded scx_layered
//! `.so` and wraps them for safe(r) access. [`LayeredMonitor`] implements the
//! [`Monitor`](crate::monitor::Monitor) trait to sample layered state at each
//! scheduling event, the same way [`LavdMonitor`](crate::probes::LavdMonitor)
//! samples LAVD's.
//!
//! # What separates the two halves of this surface
//!
//! [`LayeredProbes::task_layer`] reports the OUTCOME of layer assignment. On
//! its own that is the equivalent of having LAVD's `lat_cri` without
//! `wait_freq`, `avg_runtime` or `svc_time_iwgt`: enough to see that a decision
//! differs from expectation, never enough to prove it wrong. The match probes
//! ([`LayeredProbes::match_term`], [`LayeredProbes::match_first_failure`],
//! [`LayeredProbes::task_comm`], [`LayeredProbes::task_cgrp_path`]) report the
//! FACTORS, which is what makes a specific wrong behaviour demonstrable.
//!
//! Every verdict comes from the scheduler's own `match_one()`. Nothing here
//! re-derives matching in Rust.
//!
//! # Safety
//!
//! This module resolves C function pointers via `dlsym` at runtime and calls
//! them through `unsafe extern "C" fn` types. The resolved pointers are valid
//! only for the lifetime of the loaded scheduler `.so`. Callers must ensure the
//! scheduler remains loaded while [`LayeredProbes`] is in use.
//!
//! In practice that means **binding the [`Simulator`](crate::Simulator) to a
//! named local**, not calling it as a temporary:
//!
//! ```ignore
//! let sim = Simulator::new(sched);                    // keeps the .so mapped
//! let result = sim.run_monitored(scenario, &mut monitor);
//! assert_eq!(monitor.probes().nr_layers(), 2);        // fine
//!
//! let result = Simulator::new(sched).run_monitored(..); // .so unmapped here
//! monitor.probes().nr_layers();                         // SIGSEGV, not a panic
//! ```
//!
//! The scheduler moves into the `Simulator`, so a temporary one is dropped at
//! the end of the statement and `dlclose` unmaps everything the probes point
//! at. The failure is a segfault with no message, which is why it is spelled
//! out here.

use std::ffi::{c_char, c_void, CStr};

use crate::ffi::DynamicScheduler;
use crate::kfuncs::{self, CallbackContext, OpsContext};
use crate::monitor::{Monitor, ProbeContext, ProbePoint};
use crate::types::{CpuId, Pid, TimeNs};

/// scx_layered's "task belongs to no layer" sentinel (`MAX_LAYERS`).
pub const LAYERED_NO_LAYER: u32 = 16;

/// Buffer size for the string probes. `MAX_PATH` in `intf.h` is 4096.
const STR_BUF_LEN: usize = 4096;

/// Names the CPU a probe's observation is happening on, for the duration of
/// one probe call.
///
/// The probes that reach real scheduler code — `format_cgrp_path()`, and the
/// cgroup arms of `match_one()` that consume its output — resolve a per-CPU
/// scratch buffer through the substrate's per-callback identity context.
/// `sim_callback!` clears that context when scheduler C code returns, so by
/// the time a monitor probe point or a post-run assertion runs there is none
/// installed and the scratch lookup would return NULL.
///
/// Supplying it is the honest answer rather than a workaround: the observation
/// really is happening on some CPU, and a scratch buffer has to come from
/// somewhere. Any previously installed context is restored on drop, so a probe
/// called from inside a callback context does not disturb it.
struct ProbeCpuGuard {
    saved: Option<CallbackContext>,
}

impl ProbeCpuGuard {
    fn enter(cpu: CpuId) -> Self {
        let saved = kfuncs::get_callback_ctx();
        kfuncs::install_callback_ctx(CallbackContext {
            current_cpu: cpu,
            ops_context: OpsContext::None,
            waker_task_raw: None,
        });
        ProbeCpuGuard { saved }
    }
}

impl Drop for ProbeCpuGuard {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(ctx) => kfuncs::install_callback_ctx(ctx),
            None => kfuncs::clear_callback_ctx(),
        }
    }
}

// C function pointer types, grouped by shape.
type EnumFn = unsafe extern "C" fn(i32) -> i32;
type PidI32Fn = unsafe extern "C" fn(i32) -> i32;
type PidU32Fn = unsafe extern "C" fn(i32) -> u32;
type PidU64Fn = unsafe extern "C" fn(i32) -> u64;
type VoidU32Fn = unsafe extern "C" fn() -> u32;
type VoidU64Fn = unsafe extern "C" fn() -> u64;
type U32ToU32Fn = unsafe extern "C" fn(u32) -> u32;
type U32ToU64Fn = unsafe extern "C" fn(u32) -> u64;
type U32ToI32Fn = unsafe extern "C" fn(u32) -> i32;
type PairToI32Fn = unsafe extern "C" fn(u32, u32) -> i32;
type PairToU32Fn = unsafe extern "C" fn(u32, u32) -> u32;
type PairToU64Fn = unsafe extern "C" fn(u32, u32) -> u64;
type TripleToI32Fn = unsafe extern "C" fn(u32, u32, u32) -> i32;
type TripleToU64Fn = unsafe extern "C" fn(u32, u32, u32) -> u64;
type NeedleFn = unsafe extern "C" fn(u32, u32, u32, *mut c_char, u32) -> i32;
type TaskStrFn = unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> i32;
type TaskTermFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32) -> i32;
type TaskOrFn = unsafe extern "C" fn(*mut c_void, u32, u32) -> i32;

/// Read-only accessors for scx_layered's own state.
///
/// Every value below is read straight out of the scheduler's `task_ctx` /
/// `cpu_ctx` / `layers[]`, or produced by calling one of the scheduler's own
/// functions, so tests assert on what scx_layered actually decided rather than
/// on a re-derivation of what it should have decided.
///
/// Created from a loaded [`DynamicScheduler`] via [`LayeredProbes::new`]; the
/// resolved pointers live as long as that scheduler.
pub struct LayeredProbes {
    enum_fn: EnumFn,
    task_layer_fn: PidU32Fn,
    task_dsq_fn: PidU64Fn,
    nr_layers_fn: VoidU32Fn,
    layer_nr_cpus_fn: U32ToU32Fn,
    layer_has_cpu_fn: PairToI32Fn,
    layer_bpf_has_cpu_fn: PairToI32Fn,
    layer_nr_tasks_fn: U32ToU64Fn,
    layer_stat_fn: PairToU64Fn,
    layer_usage_fn: PairToU64Fn,
    layer_node_usage_fn: PairToU64Fn,
    layer_node_pinned_usage_fn: PairToU64Fn,
    global_stat_fn: U32ToU64Fn,
    cpu_llc_fn: U32ToU32Fn,
    cpu_node_fn: U32ToU32Fn,
    nr_llcs_fn: VoidU32Fn,
    nr_nodes_fn: VoidU32Fn,
    sibling_cpu_fn: U32ToI32Fn,
    timer_fires_fn: VoidU64Fn,
    growth_denied_fn: PairToI32Fn,
    growth_denied_count_fn: PairToU64Fn,
    layer_node_duty_raw_fn: PairToU64Fn,
    xnuma_rate_fn: TripleToU64Fn,
    xnuma_is_mig_src_fn: PairToI32Fn,
    // Per-task state (pid-keyed; reads wrapper-owned task_ctx storage).
    task_refresh_layer_fn: PidI32Fn,
    task_recheck_membership_fn: PidU64Fn,
    task_layer_refresh_seq_fn: PidU64Fn,
    layer_refresh_seq_fn: VoidU64Fn,
    task_llc_fn: PidU32Fn,
    task_pinned_node_fn: PidU32Fn,
    task_all_cpus_allowed_fn: PidI32Fn,
    task_runtime_avg_fn: PidU64Fn,
    // Match evaluation.
    match_nr_ors_fn: U32ToU32Fn,
    match_nr_ands_fn: PairToU32Fn,
    match_kind_fn: TripleToI32Fn,
    match_exclude_fn: TripleToI32Fn,
    match_needle_fn: NeedleFn,
    match_term_fn: TaskTermFn,
    match_first_failure_fn: TaskOrFn,
    task_comm_fn: TaskStrFn,
    task_cgrp_path_fn: TaskStrFn,
}

impl LayeredProbes {
    /// Resolve all scx_layered probe symbols from the loaded scheduler.
    ///
    /// # Panics
    /// Panics if a probe symbol is missing, which means the loaded `.so` is
    /// not `libscx_layered.so`.
    pub fn new(sched: &DynamicScheduler) -> Self {
        // SAFETY: Symbols are resolved from a loaded scheduler `.so` built by
        // our build system; the fn-pointer types match schedulers/layered/
        // wrapper.c's exported signatures.
        unsafe {
            macro_rules! resolve {
                ($name:expr, $ty:ty) => {
                    *sched.get_symbol::<$ty>($name).unwrap_or_else(|| {
                        panic!(
                            "probe symbol {:?} not found (is this libscx_layered.so?)",
                            std::str::from_utf8($name).unwrap_or("<invalid>")
                        )
                    })
                };
            }
            LayeredProbes {
                enum_fn: resolve!(b"layered_probe_enum", EnumFn),
                task_layer_fn: resolve!(b"layered_probe_task_layer", PidU32Fn),
                task_dsq_fn: resolve!(b"layered_probe_task_dsq", PidU64Fn),
                nr_layers_fn: resolve!(b"layered_probe_nr_layers", VoidU32Fn),
                layer_nr_cpus_fn: resolve!(b"layered_probe_layer_nr_cpus", U32ToU32Fn),
                layer_has_cpu_fn: resolve!(b"layered_probe_layer_has_cpu", PairToI32Fn),
                layer_bpf_has_cpu_fn: resolve!(b"layered_probe_layer_bpf_has_cpu", PairToI32Fn),
                layer_nr_tasks_fn: resolve!(b"layered_probe_layer_nr_tasks", U32ToU64Fn),
                layer_stat_fn: resolve!(b"layered_probe_layer_stat", PairToU64Fn),
                layer_usage_fn: resolve!(b"layered_probe_layer_usage", PairToU64Fn),
                layer_node_usage_fn: resolve!(b"layered_probe_layer_node_usage", PairToU64Fn),
                layer_node_pinned_usage_fn: resolve!(
                    b"layered_probe_layer_node_pinned_usage",
                    PairToU64Fn
                ),
                global_stat_fn: resolve!(b"layered_probe_global_stat", U32ToU64Fn),
                cpu_llc_fn: resolve!(b"layered_probe_cpu_llc", U32ToU32Fn),
                cpu_node_fn: resolve!(b"layered_probe_cpu_node", U32ToU32Fn),
                nr_llcs_fn: resolve!(b"layered_probe_nr_llcs", VoidU32Fn),
                nr_nodes_fn: resolve!(b"layered_probe_nr_nodes", VoidU32Fn),
                sibling_cpu_fn: resolve!(b"layered_probe_sibling_cpu", U32ToI32Fn),
                timer_fires_fn: resolve!(b"layered_probe_timer_fires", VoidU64Fn),
                growth_denied_fn: resolve!(b"layered_probe_growth_denied", PairToI32Fn),
                growth_denied_count_fn: resolve!(b"layered_probe_growth_denied_count", PairToU64Fn),
                layer_node_duty_raw_fn: resolve!(b"layered_probe_layer_node_duty_raw", PairToU64Fn),
                xnuma_rate_fn: resolve!(b"layered_probe_xnuma_rate", TripleToU64Fn),
                xnuma_is_mig_src_fn: resolve!(b"layered_probe_xnuma_is_mig_src", PairToI32Fn),
                task_refresh_layer_fn: resolve!(b"layered_probe_task_refresh_layer", PidI32Fn),
                task_recheck_membership_fn: resolve!(
                    b"layered_probe_task_recheck_membership",
                    PidU64Fn
                ),
                task_layer_refresh_seq_fn: resolve!(
                    b"layered_probe_task_layer_refresh_seq",
                    PidU64Fn
                ),
                layer_refresh_seq_fn: resolve!(b"layered_probe_layer_refresh_seq", VoidU64Fn),
                task_llc_fn: resolve!(b"layered_probe_task_llc", PidU32Fn),
                task_pinned_node_fn: resolve!(b"layered_probe_task_pinned_node", PidU32Fn),
                task_all_cpus_allowed_fn: resolve!(
                    b"layered_probe_task_all_cpus_allowed",
                    PidI32Fn
                ),
                task_runtime_avg_fn: resolve!(b"layered_probe_task_runtime_avg", PidU64Fn),
                match_nr_ors_fn: resolve!(b"layered_probe_match_nr_ors", U32ToU32Fn),
                match_nr_ands_fn: resolve!(b"layered_probe_match_nr_ands", PairToU32Fn),
                match_kind_fn: resolve!(b"layered_probe_match_kind", TripleToI32Fn),
                match_exclude_fn: resolve!(b"layered_probe_match_exclude", TripleToI32Fn),
                match_needle_fn: resolve!(b"layered_probe_match_needle", NeedleFn),
                match_term_fn: resolve!(b"layered_probe_match_term", TaskTermFn),
                match_first_failure_fn: resolve!(b"layered_probe_match_first_failure", TaskOrFn),
                task_comm_fn: resolve!(b"layered_probe_task_comm", TaskStrFn),
                task_cgrp_path_fn: resolve!(b"layered_probe_task_cgrp_path", TaskStrFn),
            }
        }
    }

    // -- Outcomes: what scx_layered decided --

    /// Read one intf.h enum value by [`LayeredEnumProbe`] selector.
    pub fn enum_value(&self, which: LayeredEnumProbe) -> i32 {
        // SAFETY: pure switch over an integer selector, no pointers.
        unsafe { (self.enum_fn)(which as i32) }
    }

    /// The layer scx_layered assigned to `pid`, or [`LAYERED_NO_LAYER`].
    ///
    /// This is the OUTCOME. When it is not what you expected, the question
    /// "why" is answered by [`LayeredProbes::match_first_failure`], not here.
    pub fn task_layer(&self, pid: Pid) -> u32 {
        // SAFETY: pid is bounds-checked C-side; no pointers cross the call.
        unsafe { (self.task_layer_fn)(pid.0) }
    }

    /// The DSQ scx_layered last enqueued `pid` to.
    pub fn task_dsq(&self, pid: Pid) -> u64 {
        // SAFETY: pid is bounds-checked C-side.
        unsafe { (self.task_dsq_fn)(pid.0) }
    }

    /// Number of configured layers.
    pub fn nr_layers(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_layers_fn)() }
    }

    /// `layer->nr_cpus` for `layer_id`.
    pub fn layer_nr_cpus(&self, layer_id: u32) -> u32 {
        // SAFETY: layer_id is bounds-checked C-side.
        unsafe { (self.layer_nr_cpus_fn)(layer_id) }
    }

    /// Whether `cpu` is in `layer_id`'s published cpumask.
    pub fn layer_has_cpu(&self, layer_id: u32, cpu: CpuId) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_has_cpu_fn)(layer_id, cpu.0) != 0 }
    }

    /// Whether the real BPF kptr cpumask contains `cpu` after refresh.
    pub fn layer_bpf_has_cpu(&self, layer_id: u32, cpu: CpuId) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_bpf_has_cpu_fn)(layer_id, cpu.0) != 0 }
    }

    /// `layer->nr_tasks` — how many tasks scx_layered currently places in it.
    pub fn layer_nr_tasks(&self, layer_id: u32) -> u64 {
        // SAFETY: layer_id is bounds-checked C-side.
        unsafe { (self.layer_nr_tasks_fn)(layer_id) }
    }

    /// A per-layer stat summed across CPUs (see `enum layer_stat_id`).
    pub fn layer_stat(&self, layer_id: u32, stat: LayerStat) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_stat_fn)(layer_id, stat as u32) }
    }

    /// Cumulative runtime (ns) a layer accrued in one usage class, summed
    /// across CPUs. This is the raw signal the userspace allocator sizes
    /// layers from, so it answers "what demand did the control loop see?".
    pub fn layer_usage(&self, layer_id: u32, usage: LayerUsage) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_usage_fn)(layer_id, usage as u32) }
    }

    /// Production `Stats::read_layer_node_usages()` over the real BPF
    /// counters: a layer's runtime on one NUMA node.
    pub fn layer_node_usage(&self, layer_id: u32, node_id: u32) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_node_usage_fn)(layer_id, node_id) }
    }

    /// Production `Stats::read_layer_node_pinned_usages()`: the part of a
    /// layer's per-node runtime that came from node-PINNED tasks, which the
    /// allocator may not satisfy from another node.
    pub fn layer_node_pinned_usage(&self, layer_id: u32, node_id: u32) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_node_pinned_usage_fn)(layer_id, node_id) }
    }

    /// A global stat summed across CPUs (see `enum global_stat_id`).
    pub fn global_stat(&self, stat: GlobalStat) -> u64 {
        // SAFETY: the index is bounds-checked C-side.
        unsafe { (self.global_stat_fn)(stat as u32) }
    }

    /// The LLC id scx_layered recorded for `cpu`.
    pub fn cpu_llc(&self, cpu: CpuId) -> u32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.cpu_llc_fn)(cpu.0) }
    }

    /// The NUMA node id scx_layered recorded for `cpu`.
    pub fn cpu_node(&self, cpu: CpuId) -> u32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.cpu_node_fn)(cpu.0) }
    }

    /// Number of LLCs the scheduler was told about.
    pub fn nr_llcs(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_llcs_fn)() }
    }

    /// Number of NUMA nodes the scheduler was told about.
    pub fn nr_nodes(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_nodes_fn)() }
    }

    /// `__sibling_cpu[cpu]`, or `-1` when SMT is off.
    pub fn sibling_cpu(&self, cpu: CpuId) -> i32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.sibling_cpu_fn)(cpu.0) }
    }

    /// How many times the antistall timer callback has run.
    pub fn timer_fires(&self) -> u64 {
        // SAFETY: no arguments.
        unsafe { (self.timer_fires_fn)() }
    }

    /// Whether the latest userspace allocation pass denied this layer's
    /// measured unpinned growth demand on `node_id`.
    pub fn growth_denied(&self, layer_id: u32, node_id: u32) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.growth_denied_fn)(layer_id, node_id) != 0 }
    }

    /// Number of control iterations which produced `growth_denied`.
    pub fn growth_denied_count(&self, layer_id: u32, node_id: u32) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.growth_denied_count_fn)(layer_id, node_id) }
    }

    // -- Cross-NUMA migration gate (userspace-written, BPF-read) --

    /// Sum of `cpu_ctx.layer_duty_sum[layer_id]` over the CPUs of `node_id`.
    ///
    /// The raw counter behind upstream's `layer_node_duty_sums`, and the input
    /// the cross-NUMA gate decides from. It counts smoothed *runnable* time,
    /// so a saturated node reports more than its CPU count — see
    /// `layered_stopping()` in `main.bpf.c`.
    pub fn layer_node_duty_raw(&self, layer_id: u32, node_id: u32) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_node_duty_raw_fn)(layer_id, node_id) }
    }

    /// `layers[layer_id].node[src].xnuma[dst].rate`.
    ///
    /// `u64::MAX` = gating off (always allow), `0` = deny, else a token-bucket
    /// rate in duty-cycle units per second. Written only by the userspace
    /// control loop; `0` on every pair is the pre-fix state of mb sim-dox34.
    pub fn xnuma_rate(&self, layer_id: u32, src_node: u32, dst_node: u32) -> u64 {
        // SAFETY: all three indices are bounds-checked C-side.
        unsafe { (self.xnuma_rate_fn)(layer_id, src_node, dst_node) }
    }

    /// `layers[layer_id].node[node_id].xnuma_is_mig_src`.
    ///
    /// Both `pick_idle_cpu()`'s remote-node walk and `try_consume_layer()`'s
    /// remote-LLC loop check this before they check the budget, so a false
    /// here closes cross-NUMA migration regardless of the rates.
    pub fn xnuma_is_mig_src(&self, layer_id: u32, node_id: u32) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.xnuma_is_mig_src_fn)(layer_id, node_id) != 0 }
    }

    // -- Membership lifecycle: "why is this task STILL in that layer?" --

    /// `taskc->refresh_layer` — a re-match is pending for this task.
    /// `None` when the task has no `task_ctx`.
    pub fn task_refresh_layer(&self, pid: Pid) -> Option<bool> {
        // SAFETY: pid is bounds-checked C-side.
        match unsafe { (self.task_refresh_layer_fn)(pid.0) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// `taskc->recheck_layer_membership`, decoded.
    pub fn task_recheck_membership(&self, pid: Pid) -> MemberState {
        // SAFETY: pid is bounds-checked C-side.
        MemberState::from_raw(unsafe { (self.task_recheck_membership_fn)(pid.0) })
    }

    /// `taskc->layer_refresh_seq` — the global seq as of this task's last
    /// match. Compare against [`LayeredProbes::layer_refresh_seq`]: a task
    /// whose seq lags is one a `periodically_refresh` layer is due to re-match.
    pub fn task_layer_refresh_seq(&self, pid: Pid) -> u64 {
        // SAFETY: pid is bounds-checked C-side.
        unsafe { (self.task_layer_refresh_seq_fn)(pid.0) }
    }

    /// The global `layer_refresh_seq_avgruntime` counter.
    pub fn layer_refresh_seq(&self) -> u64 {
        // SAFETY: no arguments.
        unsafe { (self.layer_refresh_seq_fn)() }
    }

    // -- Placement inputs --

    /// `taskc->llc_id` — the LLC scx_layered last placed this task in.
    /// `None` when the task has no `task_ctx`.
    pub fn task_llc(&self, pid: Pid) -> Option<u32> {
        // SAFETY: pid is bounds-checked C-side.
        match unsafe { (self.task_llc_fn)(pid.0) } {
            u32::MAX => None,
            llc => Some(llc),
        }
    }

    /// `taskc->pinned_node` — the one NUMA node the task's affinity confines
    /// it to. `None` when there is no `task_ctx`; the value equals the node
    /// count when the task is not node-pinned.
    pub fn task_pinned_node(&self, pid: Pid) -> Option<u32> {
        // SAFETY: pid is bounds-checked C-side.
        match unsafe { (self.task_pinned_node_fn)(pid.0) } {
            u32::MAX => None,
            node => Some(node),
        }
    }

    /// `taskc->all_cpus_allowed` — false means a real affinity restriction is
    /// in force, which changes which placement paths are reachable at all.
    pub fn task_all_cpus_allowed(&self, pid: Pid) -> Option<bool> {
        // SAFETY: pid is bounds-checked C-side.
        match unsafe { (self.task_all_cpus_allowed_fn)(pid.0) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// `taskc->runtime_avg` — the value `MATCH_AVG_RUNTIME` compares against.
    pub fn task_runtime_avg(&self, pid: Pid) -> u64 {
        // SAFETY: pid is bounds-checked C-side.
        unsafe { (self.task_runtime_avg_fn)(pid.0) }
    }

    // -- Match configuration (no task needed) --

    /// `layer->nr_match_ors` — how many alternative rule groups the layer has.
    pub fn match_nr_ors(&self, layer_id: u32) -> u32 {
        // SAFETY: layer_id is bounds-checked C-side.
        unsafe { (self.match_nr_ors_fn)(layer_id) }
    }

    /// `nr_match_ands` — how many terms must all hold in one OR group.
    pub fn match_nr_ands(&self, layer_id: u32, or_id: u32) -> u32 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.match_nr_ands_fn)(layer_id, or_id) }
    }

    /// The kind of one configured term, or `None` when the indices are out of
    /// range (or the kind is one this build of `intf.h` does not know).
    pub fn match_kind(&self, layer_id: u32, or_id: u32, and_id: u32) -> Option<LayeredMatchKind> {
        // SAFETY: all three indices are bounds-checked C-side.
        LayeredMatchKind::from_raw(unsafe { (self.match_kind_fn)(layer_id, or_id, and_id) })
    }

    /// `match->exclude` — whether the term is negated.
    pub fn match_exclude(&self, layer_id: u32, or_id: u32, and_id: u32) -> Option<bool> {
        // SAFETY: all three indices are bounds-checked C-side.
        match unsafe { (self.match_exclude_fn)(layer_id, or_id, and_id) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// The configured string a string-kind term compares against — the
    /// "needle". `None` for indices out of range or a kind carrying no string.
    ///
    /// Pair with [`LayeredProbes::task_comm`] / [`LayeredProbes::task_cgrp_path`]
    /// to see BOTH sides of the comparison the scheduler made.
    pub fn match_needle(&self, layer_id: u32, or_id: u32, and_id: u32) -> Option<String> {
        let mut buf = [0u8; STR_BUF_LEN];
        // SAFETY: the buffer is `STR_BUF_LEN` bytes and its length is passed
        // alongside; the C side NUL-terminates within it.
        let n = unsafe {
            (self.match_needle_fn)(
                layer_id,
                or_id,
                and_id,
                buf.as_mut_ptr() as *mut c_char,
                STR_BUF_LEN as u32,
            )
        };
        (n >= 0).then(|| cstr_to_string(&buf))
    }

    // -- Match evaluation: the FACTORS behind the outcome --

    /// The raw verdict of ONE term, from the scheduler's own `match_one()`,
    /// BEFORE `exclude` is applied.
    ///
    /// # Safety
    /// `task_raw` must be a valid, live `task_struct` pointer with per-task
    /// storage allocated (i.e. one obtained from a [`ProbeContext`] during the
    /// run). Same contract as the LAVD task probes.
    ///
    /// Time-varying kinds (`AvgRuntime`, `SystemCpuUtilBelow`,
    /// `DsqInsertBelow`) are evaluated AS OF this call, not as of the decision
    /// that placed the task in its current layer.
    pub unsafe fn match_term(
        &self,
        task_raw: *mut c_void,
        cpu: CpuId,
        layer_id: u32,
        or_id: u32,
        and_id: u32,
    ) -> MatchVerdict {
        let _cpu = ProbeCpuGuard::enter(cpu);
        MatchVerdict::from_raw((self.match_term_fn)(task_raw, layer_id, or_id, and_id))
    }

    /// Walk one OR group the way `match_layer()` does and report where it
    /// stopped. This is the probe that turns "landed in the wrong layer" into
    /// "term 0 of OR group 0, a `CgroupContains`, does not hold".
    ///
    /// # Safety
    /// Same contract as [`LayeredProbes::match_term`].
    pub unsafe fn match_first_failure(
        &self,
        task_raw: *mut c_void,
        cpu: CpuId,
        layer_id: u32,
        or_id: u32,
    ) -> OrGroupVerdict {
        let _cpu = ProbeCpuGuard::enter(cpu);
        OrGroupVerdict::from_raw((self.match_first_failure_fn)(task_raw, layer_id, or_id))
    }

    /// `p->comm` as the scheduler sees it: the left-hand side of every
    /// `CommPrefix` comparison.
    ///
    /// # Safety
    /// Same contract as [`LayeredProbes::match_term`].
    pub unsafe fn task_comm(&self, task_raw: *mut c_void) -> Option<String> {
        let mut buf = [0u8; STR_BUF_LEN];
        let n = (self.task_comm_fn)(
            task_raw,
            buf.as_mut_ptr() as *mut c_char,
            STR_BUF_LEN as u32,
        );
        (n >= 0).then(|| cstr_to_string(&buf))
    }

    /// The cgroup path as produced by the scheduler's OWN `format_cgrp_path()`
    /// — not the harness's idea of the path, but the string layered actually
    /// compares against.
    ///
    /// # Safety
    /// Same contract as [`LayeredProbes::match_term`].
    pub unsafe fn task_cgrp_path(&self, task_raw: *mut c_void, cpu: CpuId) -> Option<String> {
        let _cpu = ProbeCpuGuard::enter(cpu);
        let mut buf = [0u8; STR_BUF_LEN];
        let n = (self.task_cgrp_path_fn)(
            task_raw,
            buf.as_mut_ptr() as *mut c_char,
            STR_BUF_LEN as u32,
        );
        (n >= 0).then(|| cstr_to_string(&buf))
    }

    /// Evaluate every configured term of every layer for one task and return
    /// the full picture: for each layer, which OR group matched, or where each
    /// one failed.
    ///
    /// # Safety
    /// Same contract as [`LayeredProbes::match_term`].
    pub unsafe fn match_trace(&self, task_raw: *mut c_void, cpu: CpuId) -> MatchTrace {
        let layers = (0..self.nr_layers())
            .map(|layer_id| {
                let groups = (0..self.match_nr_ors(layer_id))
                    .map(|or_id| self.match_first_failure(task_raw, cpu, layer_id, or_id))
                    .collect();
                LayerMatchTrace { layer_id, groups }
            })
            .collect();
        MatchTrace {
            comm: self.task_comm(task_raw),
            cgrp_path: self.task_cgrp_path(task_raw, cpu),
            layers,
        }
    }

    /// Render a term as `"<kind>(<needle>)"` / `"!<kind>"` for failure
    /// messages. Falls back to the raw indices when the term is out of range.
    pub fn describe_term(&self, layer_id: u32, or_id: u32, and_id: u32) -> String {
        let Some(kind) = self.match_kind(layer_id, or_id, and_id) else {
            return format!("<no term {layer_id}/{or_id}/{and_id}>");
        };
        let bang = if self.match_exclude(layer_id, or_id, and_id) == Some(true) {
            "!"
        } else {
            ""
        };
        match self.match_needle(layer_id, or_id, and_id) {
            Some(needle) => format!("{bang}{kind:?}({needle:?})"),
            None => format!("{bang}{kind:?}"),
        }
    }
}

/// Decode a NUL-terminated C string out of a probe buffer.
fn cstr_to_string(buf: &[u8]) -> String {
    CStr::from_bytes_until_nul(buf)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The verdict of one match term, mirroring `enum layered_match_verdict` in
/// `schedulers/layered/wrapper.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchVerdict {
    /// The scheduler's `match_one()` returned true.
    Holds,
    /// The scheduler's `match_one()` returned false.
    DoesNotHold,
    /// `UsedGpuTid` / `UsedGpuPid`. Evaluating these MUTATES scheduler state
    /// (they can set `recheck_layer_membership = MEMBER_EXPIRED`, and error out
    /// when GPU support is off), so a read-only probe declines to call them.
    ///
    /// This is UNCOVERED, not stubbed: nothing fake is substituted for the
    /// verdict, and the caller is told exactly which term went unobserved.
    Unprobeable,
    /// `format_cgrp_path()` failed for this task.
    NoCgroupPath,
    /// The task has no `task_ctx` (never went through `init_task`, or exited).
    NoTaskCtx,
    /// The layer / OR / AND indices name no configured term.
    OutOfRange,
}

impl MatchVerdict {
    fn from_raw(raw: i32) -> Self {
        match raw {
            1 => MatchVerdict::Holds,
            0 => MatchVerdict::DoesNotHold,
            -1 => MatchVerdict::OutOfRange,
            -2 => MatchVerdict::NoCgroupPath,
            -3 => MatchVerdict::Unprobeable,
            _ => MatchVerdict::NoTaskCtx,
        }
    }
}

/// The verdict of one whole OR group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrGroupVerdict {
    /// Every term holds — this group matches the task.
    Matches,
    /// The term at this AND index is the first that does not hold. This is the
    /// answer to "which rule rejected my task?".
    FailedAt(u32),
    /// The walk could not be completed; see [`MatchVerdict`].
    Indeterminate(MatchVerdict),
}

impl OrGroupVerdict {
    fn from_raw(raw: i32) -> Self {
        match raw {
            -1 => OrGroupVerdict::Matches,
            n if n >= 0 => OrGroupVerdict::FailedAt(n as u32),
            n => OrGroupVerdict::Indeterminate(MatchVerdict::from_raw(n)),
        }
    }
}

/// `taskc->recheck_layer_membership`, decoded from its sentinel encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    /// `MEMBER_NOEXPIRE` — membership never expires on its own.
    NoExpire,
    /// `MEMBER_EXPIRED` — a re-match is owed.
    Expired,
    /// `MEMBER_CANTMATCH` — the task matched a GPU rule but failed the rest,
    /// so layered stopped re-matching it.
    CantMatch,
    /// `MEMBER_INVALID` — no `task_ctx` for this pid.
    Invalid,
    /// An absolute deadline (ns) set from the layer's `member_expire_ms`.
    ExpiresAt(TimeNs),
}

impl MemberState {
    fn from_raw(raw: u64) -> Self {
        match raw {
            v if v == u64::MAX => MemberState::NoExpire,
            v if v == u64::MAX - 1 => MemberState::Expired,
            v if v == u64::MAX - 2 => MemberState::CantMatch,
            v if v == u64::MAX - 3 => MemberState::Invalid,
            v => MemberState::ExpiresAt(v as TimeNs),
        }
    }
}

/// Per-layer part of a [`MatchTrace`].
#[derive(Debug, Clone)]
pub struct LayerMatchTrace {
    /// Which layer this covers.
    pub layer_id: u32,
    /// One verdict per configured OR group, in `match_layer()`'s walk order.
    pub groups: Vec<OrGroupVerdict>,
}

impl LayerMatchTrace {
    /// Whether any OR group of this layer matched the task.
    pub fn matched(&self) -> bool {
        self.groups.contains(&OrGroupVerdict::Matches)
    }
}

/// A complete "why this layer" record for one task at one instant: both sides
/// of the comparison (the task's `comm` and cgroup path) plus every layer's
/// per-OR-group verdict.
#[derive(Debug, Clone)]
pub struct MatchTrace {
    /// `p->comm` as the scheduler read it.
    pub comm: Option<String>,
    /// The path the scheduler's own `format_cgrp_path()` produced.
    pub cgrp_path: Option<String>,
    /// One entry per configured layer, in `maybe_refresh_layer()`'s scan order.
    pub layers: Vec<LayerMatchTrace>,
}

impl MatchTrace {
    /// The first layer whose rules match, which is the one
    /// `maybe_refresh_layer()` would pick: it scans in order and stops.
    pub fn first_matching_layer(&self) -> Option<u32> {
        self.layers.iter().find(|l| l.matched()).map(|l| l.layer_id)
    }
}

/// Selectors for [`LayeredProbes::enum_value`], mirroring
/// `enum layered_enum_probe_id` in `schedulers/layered/wrapper.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LayeredEnumProbe {
    /// `LAYER_KIND_OPEN`
    KindOpen = 0,
    /// `LAYER_KIND_GROUPED`
    KindGrouped = 1,
    /// `LAYER_KIND_CONFINED`
    KindConfined = 2,
    /// `MATCH_CGROUP_PREFIX`
    MatchCgroupPrefix = 3,
    /// `MATCH_COMM_PREFIX`
    MatchCommPrefix = 4,
    /// `MATCH_PCOMM_PREFIX`
    MatchPcommPrefix = 5,
    /// `MATCH_NICE_ABOVE`
    MatchNiceAbove = 6,
    /// `MATCH_NICE_BELOW`
    MatchNiceBelow = 7,
    /// `MATCH_NICE_EQUALS`
    MatchNiceEquals = 8,
    /// `MATCH_USER_ID_EQUALS`
    MatchUserIdEquals = 9,
    /// `MATCH_GROUP_ID_EQUALS`
    MatchGroupIdEquals = 10,
    /// `MATCH_PID_EQUALS`
    MatchPidEquals = 11,
    /// `MATCH_PPID_EQUALS`
    MatchPpidEquals = 12,
    /// `MATCH_TGID_EQUALS`
    MatchTgidEquals = 13,
    /// `MATCH_IS_GROUP_LEADER`
    MatchIsGroupLeader = 14,
    /// `MATCH_IS_KTHREAD`
    MatchIsKthread = 15,
    /// `MATCH_CGROUP_SUFFIX`
    MatchCgroupSuffix = 16,
    /// `MATCH_CGROUP_CONTAINS`
    MatchCgroupContains = 17,
    /// `MATCH_NUMA_NODE`
    MatchNumaNode = 18,
    /// `GROWTH_ALGO_STICKY`
    GrowthSticky = 19,
    /// `GROWTH_ALGO_LINEAR`
    GrowthLinear = 20,
    /// `GROWTH_ALGO_REVERSE`
    GrowthReverse = 21,
    /// `GROWTH_ALGO_TOPO`
    GrowthTopo = 22,
    /// `GROWTH_ALGO_ROUND_ROBIN`
    GrowthRoundRobin = 23,
    /// `MAX_LAYERS`
    MaxLayers = 24,
    /// `DEFAULT_LAYER_WEIGHT`
    DefaultLayerWeight = 25,
}

/// `enum layer_match_kind` from `intf.h`, in declaration order.
///
/// The subset that `LayerMatch` can lower to is pinned against the BPF enum by
/// `layer_enum_abi_matches_bpf`; the rest are here so that a term configured
/// through some other path still reports a name rather than a bare integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LayeredMatchKind {
    /// `MATCH_CGROUP_PREFIX`
    CgroupPrefix = 0,
    /// `MATCH_COMM_PREFIX`
    CommPrefix = 1,
    /// `MATCH_PCOMM_PREFIX`
    PcommPrefix = 2,
    /// `MATCH_NICE_ABOVE`
    NiceAbove = 3,
    /// `MATCH_NICE_BELOW`
    NiceBelow = 4,
    /// `MATCH_NICE_EQUALS`
    NiceEquals = 5,
    /// `MATCH_USER_ID_EQUALS`
    UserIdEquals = 6,
    /// `MATCH_GROUP_ID_EQUALS`
    GroupIdEquals = 7,
    /// `MATCH_PID_EQUALS`
    PidEquals = 8,
    /// `MATCH_PPID_EQUALS`
    PpidEquals = 9,
    /// `MATCH_TGID_EQUALS`
    TgidEquals = 10,
    /// `MATCH_NSPID_EQUALS`
    NsPidEquals = 11,
    /// `MATCH_NS_EQUALS`
    NsEquals = 12,
    /// `MATCH_SCXCMD_JOIN`
    ScxCmdJoin = 13,
    /// `MATCH_IS_GROUP_LEADER`
    IsGroupLeader = 14,
    /// `MATCH_IS_KTHREAD`
    IsKthread = 15,
    /// `MATCH_USED_GPU_TID` — [`MatchVerdict::Unprobeable`].
    UsedGpuTid = 16,
    /// `MATCH_USED_GPU_PID` — [`MatchVerdict::Unprobeable`].
    UsedGpuPid = 17,
    /// `MATCH_AVG_RUNTIME` — time-varying.
    AvgRuntime = 18,
    /// `MATCH_CGROUP_SUFFIX`
    CgroupSuffix = 19,
    /// `MATCH_CGROUP_CONTAINS`
    CgroupContains = 20,
    /// `MATCH_CGROUP_REGEX`
    CgroupRegex = 21,
    /// `MATCH_HINT_EQUALS`
    HintEquals = 22,
    /// `MATCH_SYSTEM_CPU_UTIL_BELOW` — time-varying.
    SystemCpuUtilBelow = 23,
    /// `MATCH_DSQ_INSERT_BELOW` — time-varying.
    DsqInsertBelow = 24,
    /// `MATCH_NUMA_NODE`
    NumaNode = 25,
}

impl LayeredMatchKind {
    /// Decode a raw `enum layer_match_kind` value.
    pub fn from_raw(raw: i32) -> Option<Self> {
        use LayeredMatchKind::*;
        Some(match raw {
            0 => CgroupPrefix,
            1 => CommPrefix,
            2 => PcommPrefix,
            3 => NiceAbove,
            4 => NiceBelow,
            5 => NiceEquals,
            6 => UserIdEquals,
            7 => GroupIdEquals,
            8 => PidEquals,
            9 => PpidEquals,
            10 => TgidEquals,
            11 => NsPidEquals,
            12 => NsEquals,
            13 => ScxCmdJoin,
            14 => IsGroupLeader,
            15 => IsKthread,
            16 => UsedGpuTid,
            17 => UsedGpuPid,
            18 => AvgRuntime,
            19 => CgroupSuffix,
            20 => CgroupContains,
            21 => CgroupRegex,
            22 => HintEquals,
            23 => SystemCpuUtilBelow,
            24 => DsqInsertBelow,
            25 => NumaNode,
            _ => return None,
        })
    }
}

/// `enum layer_usage` from `intf.h`: which class a layer's runtime fell into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayerUsage {
    /// `LAYER_USAGE_OWNED` — ran on a CPU the layer owns.
    Owned = 0,
    /// `LAYER_USAGE_OPEN` — ran on a CPU borrowed from an open layer.
    Open = 1,
    /// `LAYER_USAGE_PROTECTED`
    Protected = 2,
    /// `LAYER_USAGE_PROTECTED_PREEMPT`
    ProtectedPreempt = 3,
}

/// `enum layer_stat_id` from `intf.h`, in declaration order.
///
/// The values are positional in the BPF enum, so a reorder upstream silently
/// shifts every one of them. `layer_stat_ids_are_positionally_stable` pins the
/// count so that a reorder-plus-insert cannot pass unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayerStat {
    /// `LSTAT_SEL_LOCAL` — select_cpu found an idle CPU and went local.
    SelLocal = 0,
    /// `LSTAT_ENQ_LOCAL` — enqueue went straight to the local DSQ.
    EnqLocal = 1,
    /// `LSTAT_ENQ_WAKEUP` — enqueued on a wakeup.
    EnqWakeup = 2,
    /// `LSTAT_ENQ_EXPIRE` — enqueued after the slice expired.
    EnqExpire = 3,
    /// `LSTAT_ENQ_REENQ` — re-enqueued.
    EnqReenq = 4,
    /// `LSTAT_ENQ_DSQ` — enqueued to a layer DSQ.
    EnqDsq = 5,
    /// `LSTAT_KEEP` — kept running on the same CPU.
    Keep = 6,
    /// `LSTAT_MIN_EXEC` — held on-CPU to satisfy `min_exec_us`.
    MinExec = 7,
    /// `LSTAT_MIN_EXEC_NS` — nanoseconds spent doing so.
    MinExecNs = 8,
    /// `LSTAT_OPEN_IDLE` — an open layer used an idle CPU it does not own.
    OpenIdle = 9,
    /// `LSTAT_AFFN_VIOL` — the task's affinity excluded its layer's CPUs.
    AffnViol = 10,
    /// `LSTAT_KEEP_FAIL_MAX_EXEC` — could not keep running: `max_exec_us`.
    KeepFailMaxExec = 11,
    /// `LSTAT_KEEP_FAIL_BUSY` — could not keep running: another layer wanted
    /// the CPU. Pairs with `Keep` to answer "is this layer being held back?".
    KeepFailBusy = 12,
    /// `LSTAT_PREEMPT` — preempted another task.
    Preempt = 13,
    /// `LSTAT_PREEMPT_FIRST` — preempted before trying an idle CPU.
    PreemptFirst = 14,
    /// `LSTAT_PREEMPT_XLLC` — preemption crossed an LLC.
    PreemptXllc = 15,
    /// `LSTAT_PREEMPT_XNUMA` — preemption crossed a NUMA node.
    PreemptXnuma = 16,
    /// `LSTAT_PREEMPT_IDLE` — preempted an idle CPU.
    PreemptIdle = 17,
    /// `LSTAT_PREEMPT_FAIL` — wanted to preempt and could not.
    PreemptFail = 18,
    /// `LSTAT_EXCL_COLLISION` — exclusive layer hit an occupied SMT sibling.
    ExclCollision = 19,
    /// `LSTAT_EXCL_PREEMPT` — exclusive layer preempted the sibling.
    ExclPreempt = 20,
    /// `LSTAT_YIELD` — ops.yield handled a `sched_yield()`.
    Yield = 21,
    /// `LSTAT_YIELD_IGNORE` — ops.yield declined.
    YieldIgnore = 22,
    /// `LSTAT_MIGRATION` — task migrated between CPUs.
    Migration = 23,
    /// `LSTAT_XNUMA_MIGRATION` — migration crossed a NUMA node.
    XnumaMigration = 24,
    /// `LSTAT_XLLC_MIGRATION` — migration crossed an LLC.
    XllcMigration = 25,
    /// `LSTAT_XLLC_MIGRATION_SKIP` — declined an LLC-crossing migration.
    XllcMigrationSkip = 26,
    /// `LSTAT_XLAYER_WAKE` — woken by a task in another layer.
    XlayerWake = 27,
    /// `LSTAT_XLAYER_REWAKE` — re-woken across layers.
    XlayerRewake = 28,
    /// `LSTAT_LLC_DRAIN_TRY` — attempted to drain another LLC's queue.
    LlcDrainTry = 29,
    /// `LSTAT_LLC_DRAIN` — actually drained it.
    LlcDrain = 30,
    /// `LSTAT_SKIP_REMOTE_NODE` — declined work on a remote node.
    SkipRemoteNode = 31,
    /// `LSTAT_RUNQ_LAT_BASE` — first bucket of the runqueue-latency histogram.
    RunqLatBase = 32,
}

/// `enum global_stat_id` from `intf.h`, in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GlobalStat {
    /// `GSTAT_EXCL_IDLE`
    ExclIdle = 0,
    /// `GSTAT_EXCL_WAKEUP`
    ExclWakeup = 1,
    /// `GSTAT_HI_FB_EVENTS` — hi fallback DSQ was used.
    HiFbEvents = 2,
    /// `GSTAT_HI_FB_USAGE`
    HiFbUsage = 3,
    /// `GSTAT_LO_FB_EVENTS` — lo fallback DSQ was used.
    LoFbEvents = 4,
    /// `GSTAT_LO_FB_USAGE`
    LoFbUsage = 5,
    /// `GSTAT_FB_CPU_USAGE`
    FbCpuUsage = 6,
    /// `GSTAT_ANTISTALL` — antistall consumed a delayed DSQ.
    Antistall = 7,
    /// `GSTAT_SKIP_PREEMPT`
    SkipPreempt = 8,
    /// `GSTAT_FIXUP_VTIME`
    FixupVtime = 9,
    /// `GSTAT_PREEMPTING_MISMATCH`
    PreemptingMismatch = 10,
}

/// A snapshot of scx_layered's per-task state at a single probe point.
#[derive(Debug, Clone)]
pub struct LayeredSnapshot {
    /// Simulation time at the probe point.
    pub time_ns: TimeNs,
    /// The task sampled.
    pub pid: Pid,
    /// CPU the event occurred on.
    pub cpu: CpuId,
    /// Which scheduling event triggered the sample.
    pub point: ProbePoint,
    /// `taskc->layer_id`, or [`LAYERED_NO_LAYER`].
    pub layer_id: u32,
    /// `taskc->dsq_id`.
    pub dsq_id: u64,
    /// `taskc->llc_id`.
    pub llc_id: Option<u32>,
    /// `taskc->pinned_node`.
    pub pinned_node: Option<u32>,
    /// `taskc->refresh_layer` — a re-match is pending.
    pub refresh_layer: Option<bool>,
    /// `taskc->recheck_layer_membership`, decoded.
    pub member_state: MemberState,
    /// `taskc->runtime_avg`.
    pub runtime_avg: u64,
    /// The "why this layer" record, present on the first sample for a pid and
    /// again on every sample where `layer_id` differs from the previous one.
    /// See [`LayeredMonitor`] for why it is not taken every time.
    pub match_trace: Option<MatchTrace>,
}

/// Accumulates per-task scx_layered snapshots at each scheduling event, the
/// counterpart of [`LavdMonitor`](crate::probes::LavdMonitor).
///
/// # Why the match trace is sampled sparsely
///
/// A [`MatchTrace`] costs one `match_one()` call per configured term per layer
/// plus two string copies, while the scalar fields are single loads. Layer
/// assignment is also a step function: it changes only when
/// `maybe_refresh_layer()` re-runs. So the trace is captured on a task's first
/// sample and again whenever its `layer_id` has changed since the previous
/// sample — which is exactly where the interesting transitions are (a rename,
/// a cgroup move, a membership expiry) — and skipped in between.
///
/// The scalar `refresh_layer` / `member_state` fields ARE sampled every time,
/// so a pending-but-not-yet-applied re-match is still visible at full
/// resolution.
pub struct LayeredMonitor {
    probes: LayeredProbes,
    /// Time series of snapshots across all tasks and events.
    pub snapshots: Vec<LayeredSnapshot>,
    /// Last observed layer per pid, indexed by pid; drives trace capture.
    last_layer: Vec<Option<u32>>,
}

impl LayeredMonitor {
    /// Create a new layered monitor with the given probe accessors.
    pub fn new(probes: LayeredProbes) -> Self {
        LayeredMonitor {
            probes,
            snapshots: Vec::new(),
            last_layer: Vec::new(),
        }
    }

    /// The probe accessors, for end-of-run reads that need no task pointer
    /// (layer CPU counts, stats, usage, match configuration).
    pub fn probes(&self) -> &LayeredProbes {
        &self.probes
    }

    /// The final (most recent) snapshot for a task.
    pub fn final_snapshot(&self, pid: Pid) -> Option<&LayeredSnapshot> {
        self.snapshots.iter().rev().find(|s| s.pid == pid)
    }

    /// All snapshots for a task, ordered by time.
    pub fn task_history(&self, pid: Pid) -> impl Iterator<Item = &LayeredSnapshot> {
        self.snapshots.iter().filter(move |s| s.pid == pid)
    }

    /// The first recorded match trace for a task — the "why this layer" record
    /// as of the first time the monitor saw it.
    pub fn first_match_trace(&self, pid: Pid) -> Option<&MatchTrace> {
        self.task_history(pid).find_map(|s| s.match_trace.as_ref())
    }

    /// Every distinct layer this task was observed in, in order. A task that
    /// re-layers mid-run shows up here as more than one entry.
    pub fn layer_transitions(&self, pid: Pid) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for s in self.task_history(pid) {
            if out.last() != Some(&s.layer_id) {
                out.push(s.layer_id);
            }
        }
        out
    }
}

impl Monitor for LayeredMonitor {
    fn sample(&mut self, ctx: &ProbeContext) {
        let pid = ctx.pid;
        let layer_id = self.probes.task_layer(pid);

        let idx = pid.0.max(0) as usize;
        if self.last_layer.len() <= idx {
            self.last_layer.resize(idx + 1, None);
        }
        let changed = self.last_layer[idx] != Some(layer_id);
        self.last_layer[idx] = Some(layer_id);

        // SAFETY: `ctx.task_raw` is a live `task_struct` pointer supplied by
        // the engine at a probe point, with per-task storage allocated during
        // `init_task` — the same contract `LavdMonitor::sample` relies on.
        let match_trace =
            changed.then(|| unsafe { self.probes.match_trace(ctx.task_raw, ctx.cpu) });

        self.snapshots.push(LayeredSnapshot {
            time_ns: ctx.time_ns,
            pid,
            cpu: ctx.cpu,
            point: ctx.point,
            layer_id,
            dsq_id: self.probes.task_dsq(pid),
            llc_id: self.probes.task_llc(pid),
            pinned_node: self.probes.task_pinned_node(pid),
            refresh_layer: self.probes.task_refresh_layer(pid),
            member_state: self.probes.task_recheck_membership(pid),
            runtime_avg: self.probes.task_runtime_avg(pid),
            match_trace,
        });
    }
}
