//! Event-driven simulation engine.
//!
//! This is the core of the simulator. It maintains the event queue, simulated
//! clock, CPU/task state, and drives the scheduler through its ops callbacks.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use tracing::{debug, info, trace, warn};

use crate::backend::e9patch::E9PatchReplayBackend;
use crate::backend::replay::ReplayBackend;
use crate::cgroup::{CgroupId, CgroupRegistry};
use crate::cgroup_wrapper::default_cgroup_init_args;
use crate::cpu::{IrqContext, LastStopReason, SimCpu};
use crate::dsq::DsqManager;
use crate::ffi::{self, Scheduler};
use crate::fmt::FmtN;
use crate::kfuncs::{self, OpsContext, SimArc, SimState, SimulatorState, StagedEvent};
use crate::monitor::{Monitor, ProbeContext, ProbePoint};
use crate::perf;
use crate::preempt::{
    is_determinism_mode_enabled, record_checkpoint, scheduler_so_path, CheckpointEvent,
};
use crate::scenario::{
    CgroupCpusetChangeEvent, CgroupCreateEvent, CgroupDestroyEvent, FutexOp, IrqType, PreemptMode,
    Scenario,
};
use crate::scheduler_wrapper::{OptionalPtr, SchedulerWrapper, TaskPtr};
use crate::sim_task::SimTask;
use crate::task::{OpsTaskState, Phase, TaskState};
use crate::task_wrapper::SimTaskHandle;
use crate::trace::{DsqSampleTrigger, Trace, TraceKind};
use crate::types::{CpuId, DsqId, KickFlags, Pid, TimeNs};

/// Source file suffixes whose functions should be skippable via step-avoid-regexp.
const HELPER_SOURCE_SUFFIXES: &[&str] = &["wrapper.c", "util.bpf.c"];

/// Discover helper function names from a scheduler `.so` using `nm -l`.
///
/// Runs `nm -l <so_path>` and filters for text symbols (`T`/`t`) whose source
/// file path ends with one of [`HELPER_SOURCE_SUFFIXES`]. Skips symbols
/// starting with `_` or `.` (compiler-generated internals). Returns a sorted,
/// deduplicated list of function names. Returns an empty vec on any failure.
fn discover_helper_functions(so_path: &str) -> Vec<String> {
    let output = match std::process::Command::new("nm")
        .args(["-l", so_path])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names: Vec<String> = stdout.lines().filter_map(parse_nm_helper_symbol).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Parse a single `nm -l` output line, returning the function name if it
/// is a text symbol from a helper source file.
///
/// Expected format: `<addr> <type> <name>\t<source_path>:<line>`
fn parse_nm_helper_symbol(line: &str) -> Option<String> {
    // Split on tab to separate symbol info from source path.
    let (sym_part, source_part) = line.split_once('\t')?;
    // Source path must end with one of our helper suffixes (before `:line`).
    let source_file = source_part.split_once(':').map_or(source_part, |(f, _)| f);
    if !HELPER_SOURCE_SUFFIXES
        .iter()
        .any(|suffix| source_file.ends_with(suffix))
    {
        return None;
    }
    // Parse the symbol part: "<addr> <type> <name>"
    let mut fields = sym_part.split_whitespace();
    let _addr = fields.next()?;
    let sym_type = fields.next()?;
    let name = fields.next()?;
    // Only text (function) symbols.
    if sym_type != "T" && sym_type != "t" {
        return None;
    }
    // Skip internal/compiler-generated symbols.
    if name.starts_with('_') || name.starts_with('.') {
        return None;
    }
    Some(name.to_owned())
}

/// Which debugger flavour to generate a script for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DebuggerFlavor {
    Lldb,
    Gdb,
}

impl DebuggerFlavor {
    fn extension(self) -> &'static str {
        match self {
            Self::Lldb => "lldb",
            Self::Gdb => "gdb",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Lldb => "lldb",
            Self::Gdb => "gdb",
        }
    }

    /// Emit a single breakpoint command.
    fn fmt_breakpoint(self, sym: &str) -> String {
        match self {
            Self::Lldb => format!("breakpoint set --name {sym}\n"),
            Self::Gdb => format!("break {sym}\n"),
        }
    }

    /// Emit signal-handling commands so the debugger does not intercept
    /// signals the simulator uses internally.
    ///
    /// The simulator installs a SIGFPE handler to emulate BPF's
    /// "division by zero returns zero" semantics.  Without these
    /// commands the debugger would stop on every such SIGFPE.
    fn fmt_signal_handling(self) -> &'static str {
        match self {
            Self::Lldb => {
                "\
# The simulator emulates BPF div-by-zero semantics via a SIGFPE handler.\n\
# Let the handler work without the debugger intercepting the signal.\n\
process handle SIGFPE -s false -n false -p true\n\n"
            }
            Self::Gdb => {
                "\
# The simulator emulates BPF div-by-zero semantics via a SIGFPE handler.\n\
# Let the handler work without GDB intercepting the signal.\n\
handle SIGFPE nostop noprint pass\n\
\n\
# Enable pending breakpoints for symbols not yet loaded (e.g., .so not dlopen'd).\n\
set breakpoint pending on\n\n"
            }
        }
    }

    /// Emit commands that make `step` skip the simulator binary so the
    /// user stays inside the scheduler `.so` code.
    fn fmt_skip_simulator(self) -> String {
        match self {
            Self::Lldb => {
                if let Ok(exe) = std::env::current_exe() {
                    format!(
                        "settings set target.process.thread.step-avoid-libraries \"{}\"\n\n",
                        exe.display()
                    )
                } else {
                    String::new()
                }
            }
            Self::Gdb => {
                // GDB has no direct step-avoid-libraries equivalent.
                // Skip all Rust-mangled functions (the simulator is Rust),
                // plus C stub prefixes used by the simulator's kfunc layer.
                "\
skip -rfu \"^_ZN\"\n\
skip -rfu \"^scx_bpf_\"\n\
skip -rfu \"^scx_test_\"\n\
skip -rfu \"^sim_\"\n\n"
                    .to_owned()
            }
        }
    }

    /// Build custom commands for skipping/unskipping helper functions.
    ///
    /// Returns `None` if `names` is empty, otherwise returns a multi-line
    /// string the user can invoke as `skip-helpers` / `unskip-helpers`.
    fn build_helper_commands(self, names: &[String]) -> Option<String> {
        if names.is_empty() {
            return None;
        }
        let alternation = names.join("|");
        Some(match self {
            Self::Lldb => format!(
                "\
command alias skip-helpers settings set target.process.thread.step-avoid-regexp \"^({alternation})\"
command alias unskip-helpers settings clear target.process.thread.step-avoid-regexp",
            ),
            Self::Gdb => format!(
                "\
define skip-helpers
  skip -rfu \"^({alternation})\"
end
define unskip-helpers
  # List current skips so you can selectively delete helper skips
  info skip
end",
            ),
        })
    }
}

/// Write a debugger breakpoint script alongside the scheduler `.so`.
///
/// Given a `.so` path like `/path/to/libscx_simple.so` and a
/// [`DebuggerFlavor`], writes the script to the matching extension
/// (e.g. `.lldb` or `.gdb`). Returns `(script_path, has_helpers)`.
fn write_debugger_script(
    info: &crate::ffi::DebuggerInfo,
    flavor: DebuggerFlavor,
) -> (String, bool) {
    let so = std::path::Path::new(&info.so_path);
    let script_path = so.with_extension(flavor.extension());
    let mut script = String::new();

    script.push_str(&format!(
        "# Auto-generated {} breakpoint script for scheduler ops\n",
        flavor.name()
    ));
    script.push_str(&format!("# Scheduler: {}\n\n", info.prefix));

    // Tell the debugger not to intercept signals the simulator handles.
    script.push_str(flavor.fmt_signal_handling());

    // Make `step` stay in scheduler C code by skipping the simulator binary.
    script.push_str(&flavor.fmt_skip_simulator());

    // Breakpoints on all loaded scheduler ops callbacks.
    for sym in &info.ops_symbol_names {
        script.push_str(&flavor.fmt_breakpoint(sym));
    }

    // Helper skip/unskip commands.
    let helpers = discover_helper_functions(&info.so_path);
    let cmds = flavor.build_helper_commands(&helpers);
    if let Some(ref c) = cmds {
        script.push_str(
            "\n# Custom commands for skipping utility helpers (util.bpf.c, wrapper.c):\n",
        );
        script.push_str(c);
        script.push('\n');
    }

    if let Err(e) = std::fs::write(&script_path, &script) {
        eprintln!(
            "warning: could not write {} script to {}: {e}",
            flavor.name(),
            script_path.display()
        );
    }
    (script_path.to_string_lossy().into_owned(), cmds.is_some())
}

/// Spin-wait until a debugger (ptrace tracer) attaches to this process.
///
/// Reads `/proc/self/status` and checks `TracerPid`. Returns once
/// `TracerPid` is non-zero, polling every 100ms.
fn wait_for_debugger_attach() {
    loop {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(pid_str) = line.strip_prefix("TracerPid:\t") {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        if pid != 0 {
                            return;
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Search the current directory and up to 3 parent directories for a
/// `*.code-workspace` file, returning the first match found.
fn find_workspace_file() -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..4 {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("code-workspace") {
                    return Some(path);
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Pause execution so a debugger can attach before scheduler code runs.
///
/// Writes debugger breakpoint scripts (`.lldb` and `.gdb`) alongside the
/// scheduler `.so`, prints the PID and copy-pasteable attach commands for
/// both debuggers, then spin-waits for a debugger to attach. Once
/// attached, execution proceeds directly to `ops.init()`. The user types
/// a single `continue` from the debugger's attach stop to hit the first
/// ops breakpoint.
fn wait_for_debugger<S: Scheduler>(scheduler: &SchedulerWrapper<S>) {
    let pid = std::process::id();
    let info = scheduler.debugger_info();

    let (so_display, lldb_script, gdb_script, bp_count, has_helpers) = match &info {
        Some(dbg) => {
            let (lldb_path, lldb_helpers) = write_debugger_script(dbg, DebuggerFlavor::Lldb);
            let (gdb_path, gdb_helpers) = write_debugger_script(dbg, DebuggerFlavor::Gdb);
            let count = dbg.ops_symbol_names.len();
            (
                dbg.so_path.as_str().to_owned(),
                Some(lldb_path),
                Some(gdb_path),
                count,
                lldb_helpers || gdb_helpers,
            )
        }
        None => {
            let fallback = scheduler_so_path().unwrap_or_else(|| "<unknown>".to_string());
            (fallback, None, None, 0, false)
        }
    };

    eprintln!();
    eprintln!("=== --wait-debugger ===");
    eprintln!("PID: {pid}");
    eprintln!("Scheduler .so: {so_display}");
    if bp_count > 0 {
        eprintln!("Breakpoints: {bp_count} ops callbacks");
    }
    eprintln!();
    eprintln!("Waiting for debugger to attach...");
    eprintln!();
    eprintln!("Attach with lldb:");
    match &lldb_script {
        Some(script) => eprintln!("  lldb -p {pid} -o \"command source {script}\""),
        None => eprintln!("  lldb -p {pid}"),
    }
    eprintln!();
    eprintln!("Attach with gdb:");
    match &gdb_script {
        Some(script) => eprintln!("  gdb -p {pid} -x {script}"),
        None => eprintln!("  gdb -p {pid}"),
    }
    if has_helpers {
        eprintln!();
        eprintln!("Custom debugger commands available:");
        eprintln!("  skip-helpers    \u{2014} skip util.bpf.c/wrapper.c when stepping");
        eprintln!("  unskip-helpers  \u{2014} stop skipping helpers");
    }
    eprintln!();
    eprintln!("Attach with VSCode:");
    if let Some(ws) = find_workspace_file() {
        eprintln!("  1. Open workspace: code {}", ws.display());
        eprintln!("  2. Run & Debug (Ctrl+Shift+D) \u{2192} \"Attach\" \u{2192} enter PID {pid}");
    } else {
        eprintln!("  Run & Debug (Ctrl+Shift+D) \u{2192} \"Attach\" \u{2192} enter PID {pid}");
    }
    eprintln!();

    wait_for_debugger_attach();

    eprintln!("Debugger attached, resuming...");
}

/// Check for BPF errors after a scheduler callback.
///
/// If `ignore` is false and a BPF error is pending, takes the error message
/// and returns `Some(ExitKind::ErrorBpf(...))`. Otherwise clears any pending
/// error and returns `None`.
fn check_bpf_error(state: &mut SimulatorState, ignore: bool) -> Option<ExitKind> {
    if !ignore {
        if let Some(msg) = state.bpf_error.take() {
            return Some(ExitKind::ErrorBpf(msg));
        }
    } else {
        state.bpf_error = None;
    }
    None
}

/// How the simulation terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    /// Simulation ran to completion (duration exhausted).
    Normal,
    /// BPF scheduler called scx_bpf_error().
    ErrorBpf(String),
    /// Watchdog detected a stalled runnable task.
    ErrorStall { pid: Pid, runnable_for_ns: TimeNs },
    /// Dispatch loop exceeded iteration limit without making progress.
    ErrorDispatchLoopExhausted { cpu: CpuId },
    /// Cgroup creation failed due to resource exhaustion (ENOMEM).
    ErrorCgroupExhausted {
        cgroup_name: String,
        active_count: u32,
        max_cgroups: u32,
    },
}

impl ExitKind {
    /// Returns true if this is an error exit (not Normal).
    pub fn is_error(&self) -> bool {
        !matches!(self, ExitKind::Normal)
    }
}

/// SCX wake flags.
const SCX_ENQ_WAKEUP: u64 = 0x1;
/// A regular wakeup routed through `try_to_wake_up()`. The kernel sets this on
/// every ttwu-path activation, which is the common case; schedulers gate
/// wakeup-only logic on it (e.g. cosmos `is_wakeup()` at main.bpf.c:858, which
/// guards `is_cpu_faster()`/`cpus_share_cache()`). Matches kernel
/// SCX_WAKE_TTWU (= 8). See mb sim-e10316.
const SCX_WAKE_TTWU: u64 = 8;
/// Synchronous wakeup: waker is about to sleep/yield, hinting the scheduler
/// to place the wakee on the same CPU.  Matches kernel SCX_WAKE_SYNC (= 16).
const SCX_WAKE_SYNC: u64 = 16;

/// SCX dequeue flags: task is going to sleep.
const SCX_DEQ_SLEEP: u64 = 1;

/// Tick interval in nanoseconds (4ms, matching HZ=250).
const TICK_INTERVAL_NS: TimeNs = 4_000_000;

/// Maximum dispatch loop iterations (matches kernel SCX_DSP_MAX_LOOPS).
/// TODO(sim-b825e): Use this to implement dispatch loop exhaustion detection.
#[allow(dead_code)]
const SCX_DSP_MAX_LOOPS: u32 = 32;

/// A simulation event, ordered by timestamp then tiebreaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) time_ns: TimeNs,
    /// Tiebreaker for events at the same time (lower = higher priority).
    /// In fixed-priority mode, this is a monotonic counter (insertion order).
    /// In randomized mode, this combines a PRNG-derived priority with a
    /// monotonic counter to explore different orderings while remaining
    /// deterministic for a given seed.
    pub(crate) seq: u64,
    pub(crate) kind: EventKind,
}

impl Ord for Event {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time_ns
            .cmp(&other.time_ns)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority-queue wrapper that assigns tiebreakers to events.
///
/// In **fixed-priority** mode, events at the same timestamp are processed in
/// insertion order (monotonic `seq`). In **randomized** mode, each event gets
/// a PRNG-derived priority in the upper 32 bits of `seq` with the monotonic
/// counter in the lower 32 bits. This explores different orderings for
/// same-timestamp events while remaining deterministic for a given seed.
///
/// The event PRNG is separate from `SimulatorState::rng` so that
/// adding/removing events does not perturb the scheduler's PRNG sequence.
pub(crate) struct EventQueue {
    pub(crate) heap: BinaryHeap<Reverse<Event>>,
    /// Monotonic counter for unique event identity / fixed-priority ordering.
    pub(crate) seq: u64,
    /// Separate PRNG for randomized tiebreaking (independent of simulator PRNG).
    pub(crate) event_rng: SmallRng,
    /// Separate PRNG for stochastic timer interleaving decisions.
    pub(crate) timer_interleave_rng: SmallRng,
    /// When true, use insertion-order tiebreaking (monotonic seq).
    pub(crate) fixed_priority: bool,
}

impl EventQueue {
    pub(crate) fn new(seed: u32, fixed_priority: bool) -> Self {
        // Derive event PRNG seed from scenario seed but offset it so it's
        // independent of the main simulation PRNG.
        let event_seed = (seed as u64)
            .wrapping_mul(0x9e3779b9)
            .wrapping_add(0xdeadbeef);
        let timer_seed = (seed as u64)
            .wrapping_mul(0xd1b54a32d192ed03)
            .wrapping_add(0xa5a5_51c3);
        EventQueue {
            heap: BinaryHeap::new(),
            seq: 0,
            event_rng: SmallRng::seed_from_u64(event_seed),
            timer_interleave_rng: SmallRng::seed_from_u64(timer_seed),
            fixed_priority,
        }
    }

    /// Compute the `seq` tiebreaker for a new event.
    pub(crate) fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        if self.fixed_priority {
            s
        } else {
            // Upper 32 bits: random priority; lower 32 bits: monotonic
            // counter for deterministic uniqueness.
            let random_priority = self.event_rng.next_u32() as u64;
            (random_priority << 32) | (s & 0xFFFF_FFFF)
        }
    }

    /// Push a new event, automatically assigning a tiebreaker.
    pub(crate) fn push(&mut self, time_ns: TimeNs, kind: EventKind) {
        let seq = self.next_seq();
        self.heap.push(Reverse(Event { time_ns, seq, kind }));
    }

    /// Pop the next event (earliest timestamp, then lowest tiebreaker).
    pub(crate) fn pop(&mut self) -> Option<Event> {
        self.heap.pop().map(|Reverse(e)| e)
    }

    /// Peek at the next event's timestamp without removing it.
    pub(crate) fn peek_time(&self) -> Option<TimeNs> {
        self.heap.peek().map(|Reverse(e)| e.time_ns)
    }

    /// Stochastically pull a non-slot-0 BPF timer forward for Phase 3 race modeling.
    ///
    /// The event keeps its original timestamp and tiebreaker; callers decide
    /// how to run it. Slot 0 is the legacy LAVD update timer, while the
    /// compiled-in cgroup_bw timers occupy later slots.
    pub(crate) fn pop_stochastic_timer_interleave(
        &mut self,
        horizon_ns: TimeNs,
        one_in: u32,
    ) -> Option<Event> {
        let one_in = one_in.max(1);
        if one_in > 1 && !self.timer_interleave_rng.next_u32().is_multiple_of(one_in) {
            return None;
        }

        let mut events = Vec::with_capacity(self.heap.len());
        while let Some(event) = self.pop() {
            events.push(event);
        }
        let candidates: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(idx, event)| match event.kind {
                EventKind::TimerFired { slot, .. } if slot != 0 && event.time_ns <= horizon_ns => {
                    Some(idx)
                }
                _ => None,
            })
            .collect();

        if candidates.is_empty() {
            self.heap.extend(events.into_iter().map(Reverse));
            return None;
        }

        let pick = (self.timer_interleave_rng.next_u32() as usize) % candidates.len();
        let selected_idx = candidates[pick];
        let selected = events.remove(selected_idx);
        self.heap.extend(events.into_iter().map(Reverse));
        Some(selected)
    }

    /// Deterministically pull the earliest non-slot-0 BPF timer at or before
    /// the horizon. Used by targeted cgroup_bw race-site hooks where the
    /// desired interleave is part of the experiment, not a random choice.
    pub(crate) fn pop_targeted_timer_interleave(&mut self, horizon_ns: TimeNs) -> Option<Event> {
        let mut events = Vec::with_capacity(self.heap.len());
        while let Some(event) = self.pop() {
            events.push(event);
        }

        let selected_idx = events.iter().position(|event| {
            matches!(event.kind, EventKind::TimerFired { slot, .. } if slot != 0 && event.time_ns <= horizon_ns)
        });

        let Some(selected_idx) = selected_idx else {
            self.heap.extend(events.into_iter().map(Reverse));
            return None;
        };

        let selected = events.remove(selected_idx);
        self.heap.extend(events.into_iter().map(Reverse));
        Some(selected)
    }
}

/// Context about the task that triggered a wakeup.
///
/// In the kernel, `select_cpu` runs in the waker's context: both
/// `bpf_get_current_task_btf()` and `bpf_get_smp_processor_id()` return
/// the waker's state. The engine uses this to set up the same context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WakerInfo {
    pub(crate) pid: Pid,
    pub(crate) cpu: CpuId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Phase 2 variants used when Phase 3 dynamic window is active
pub(crate) enum EventKind {
    /// A task becomes runnable (wakes up).
    /// `waker` identifies the task that triggered the wake (if any),
    /// enabling wake-affine scheduling (e.g., COSMOS mm_affinity).
    ///
    /// `cpu` is the CPU where the wakeup originates: the waker's CPU when a
    /// waker is present, or the task's `prev_cpu` otherwise. In the kernel,
    /// `try_to_wake_up()` always runs on a specific CPU (the waker's CPU),
    /// and `ops.select_cpu()` sees that CPU via `bpf_get_smp_processor_id()`.
    /// Assigning a CPU makes this a per-CPU event eligible for concurrent
    /// batch processing (matching kernel behavior where wakeups on different
    /// CPUs proceed in parallel).
    TaskWake {
        pid: Pid,
        waker: Option<WakerInfo>,
        cpu: CpuId,
    },
    /// A task's time slice expires on the given CPU.
    SliceExpired { cpu: CpuId },
    /// A task finishes its current Run phase on the given CPU.
    TaskPhaseComplete { cpu: CpuId },
    /// A BPF timer fires (e.g., deferred wakeup timer).
    ///
    /// `cpu` is the CPU where `bpf_timer_start()` was called. In the kernel,
    /// BPF timers fire in softirq context on the CPU that armed them (with
    /// `BPF_F_TIMER_CPU_PIN`). Assigning a CPU makes this a per-CPU event
    /// eligible for concurrent batch processing (matching kernel behavior
    /// where timers on different CPUs fire independently).
    ///
    /// `slot` selects which of the scheduler's `MAX_BPF_TIMERS` (currently
    /// 8) per-scheduler timer slots fired. Phase 1 BPF infra scale-up
    /// items 1+2 (tg `scxsim-bpf-infra-scale-up-phase1`): the slot is
    /// passed through to `<scheduler>_fire_timer(slot)` so wrappers can
    /// dispatch to the right callback. Single-timer schedulers (mitosis,
    /// cosmos, the legacy LAVD path) all use `slot = 0`.
    TimerFired { cpu: CpuId, slot: u8 },
    /// Periodic scheduler tick on a CPU.
    Tick { cpu: CpuId },
    /// A CPU goes offline (hotplug remove).
    CpuOffline { cpu: CpuId },
    /// A CPU comes online (hotplug add).
    CpuOnline { cpu: CpuId },
    /// A higher-priority scheduler class takes a CPU (cpu_release).
    CpuRelease { cpu: CpuId },
    /// sched_ext regains a CPU from higher-priority class (cpu_acquire).
    CpuAcquire { cpu: CpuId },
    /// A task is migrated between cgroups at runtime.
    ///
    /// `cpu` is the CPU where the migration is initiated. In the kernel,
    /// `cgroup_migrate()` runs in process context on the CPU of the task
    /// writing to `cgroup.procs`. Assigning a CPU makes this a per-CPU
    /// event eligible for concurrent batch processing.
    CgroupMigrate {
        pid: Pid,
        from_cgroup: String,
        to_cgroup: String,
        cpu: CpuId,
    },
    /// A cgroup is created at runtime.
    ///
    /// `cpu` is the CPU where the creation is initiated. In the kernel,
    /// `cgroup_mkdir()` runs in process context on the CPU of the task
    /// creating the cgroup. Assigning a CPU makes this a per-CPU event
    /// eligible for concurrent batch processing.
    CgroupCreate {
        event: CgroupCreateEvent,
        cpu: CpuId,
    },
    /// A cgroup is destroyed at runtime.
    ///
    /// `cpu` is the CPU where the destruction is initiated. In the kernel,
    /// `cgroup_rmdir()` runs in process context on the CPU of the task
    /// removing the cgroup. Assigning a CPU makes this a per-CPU event
    /// eligible for concurrent batch processing.
    CgroupDestroy {
        event: CgroupDestroyEvent,
        cpu: CpuId,
    },
    /// A cgroup's cpuset is changed at runtime.
    ///
    /// `cpu` is the CPU where the change is initiated. In the kernel,
    /// writing to `cpuset.cpus` runs in process context on the CPU of the
    /// writing task. Assigning a CPU makes this a per-CPU event eligible
    /// for concurrent batch processing.
    CgroupCpusetChange {
        event: CgroupCpusetChangeEvent,
        cpu: CpuId,
    },
    /// An interrupt starts on a CPU (hardirq or softirq).
    IrqStart {
        cpu: CpuId,
        irq_type: IrqType,
        duration_ns: TimeNs,
        wake_pids: Vec<Pid>,
    },
    /// An interrupt handler completes on a CPU.
    IrqEnd { cpu: CpuId },
    /// A scheduled futex transition: deliver `op` to the scheduler's real
    /// futex hooks for `pid`. The CPU is derived at fire time from whichever
    /// CPU `pid` is running on (the hooks act on the running task). See
    /// `handle_futex_op` and `ai_docs/FUTEX_SIM_DESIGN.md`.
    FutexOp { pid: Pid, op: FutexOp },
    /// Per-CPU event: consume from global DSQ into local DSQ.
    ///
    /// Scheduled after ops.dispatch() returns with an empty local DSQ.
    /// Consumes logical time (`dsq_consume_ns`), modeling the cache-line
    /// transfer and dequeue overhead that is instantaneous in the
    /// sequential post-processing path.
    DsqConsume { cpu: CpuId },
    /// Per-CPU event: run the picked task (ops.running + start execution).
    ///
    /// Scheduled after a task is picked from the local DSQ (either from
    /// dispatch or from `DsqConsume`). Consumes logical time
    /// (`running_overhead_ns`), modeling the kernel overhead of setting
    /// up the task context and calling `ops.running()`.
    StartRunning { cpu: CpuId, pid: Pid },
    /// Per-CPU event: process a kick (IPI delivered to this CPU).
    ///
    /// Scheduled when `scx_bpf_kick_cpu(target, flags)` is called.
    /// Consumes logical time (`ipi_delivery_ns`), modeling the
    /// inter-processor interrupt latency between the source and target CPU.
    KickDelivered { cpu: CpuId, flags: KickFlags },
}

/// Flush staged events from `SimulatorState` into the event queue.
///
/// After each callback returns, kfuncs may have staged events (e.g.,
/// `KickDelivered`) in `state.staged_events`. This function drains
/// them and pushes corresponding `EventKind` entries into the event queue.
///
/// Staged events are sorted by `(time, cpu)` before flushing to ensure
/// deterministic insertion order regardless of the order kfuncs staged them.
pub(crate) fn flush_staged_events(state: &mut SimulatorState, events: &mut EventQueue) {
    if state.staged_events.is_empty() {
        return;
    }
    let mut staged = std::mem::take(&mut state.staged_events);
    // Sort by (time, cpu) for deterministic event queue insertion.
    staged.sort_by_key(|(t, ev)| {
        let cpu_ord = match ev {
            StagedEvent::KickDelivered { cpu, .. } => cpu.0,
        };
        (*t, cpu_ord)
    });
    for (time, staged_event) in staged {
        let kind = match staged_event {
            StagedEvent::KickDelivered { cpu, flags } => EventKind::KickDelivered { cpu, flags },
        };
        events.push(time, kind);
    }
}

/// The main simulator.
pub struct Simulator<S: Scheduler> {
    scheduler: SchedulerWrapper<S>,
}

/// Result of a simulation, keeping task storage alive for post-simulation
/// probing of scheduler-internal state.
pub struct SimulationResult {
    /// The event trace.
    pub trace: Trace,
    /// Kept alive so raw task pointers remain valid until dropped.
    tasks: HashMap<Pid, SimTask>,
}

impl SimulationResult {
    /// Get the raw `task_struct` pointer for a task.
    ///
    /// Valid until this `SimulationResult` is dropped.
    pub fn task_raw(&self, pid: Pid) -> Option<*mut c_void> {
        self.tasks.get(&pid).map(|t| t.raw())
    }
}

/// A no-op monitor (zero overhead when no monitor is needed).
struct NoopMonitor;
impl Monitor for NoopMonitor {
    fn sample(&mut self, _ctx: &ProbeContext) {}
}

/// Print simulation completion summary to stderr.
///
/// Shows logical time elapsed, task counts, concurrency, and time-slice
/// totals. For finite workloads, also prints when all tasks completed.
fn print_simulation_summary(trace: &Trace, tasks: &HashMap<Pid, SimTask>, final_clock: TimeNs) {
    use crate::fmt::fmt_duration_ns;

    let total_tasks = tasks.len();

    // End-of-simulation task state counts.
    let tasks_alive = tasks
        .values()
        .filter(|t| !matches!(t.state, TaskState::Exited))
        .count();
    let tasks_runnable = tasks
        .values()
        .filter(|t| matches!(t.state, TaskState::Runnable | TaskState::Running { .. }))
        .count();

    // Compute stats from trace events in one pass.
    let mut running = 0u32;
    let mut max_running = 0u32;
    let mut total_slices = 0u64;
    let mut completed_count = 0u64;
    let mut all_completed_at: Option<TimeNs> = None;

    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskScheduled { .. } => {
                running += 1;
                max_running = max_running.max(running);
                total_slices += 1;
            }
            TraceKind::TaskPreempted { .. }
            | TraceKind::TaskSlept { .. }
            | TraceKind::TaskYielded { .. } => {
                running = running.saturating_sub(1);
            }
            TraceKind::TaskCompleted { .. } => {
                running = running.saturating_sub(1);
                completed_count += 1;
                if completed_count == total_tasks as u64 {
                    all_completed_at = Some(event.time_ns);
                }
            }
            TraceKind::SimulationEnd { .. } => {
                running = running.saturating_sub(1);
            }
            _ => {}
        }
    }

    println!();
    println!("Simulation complete:");
    println!("  Logical time elapsed:   {}", fmt_duration_ns(final_clock));
    println!("  Total tasks:            {total_tasks}");
    println!("  Max concurrent running: {max_running}");
    println!("  Total time slices:      {total_slices}");
    println!("  Tasks at end:           {tasks_alive} alive, {tasks_runnable} runnable");

    if let Some(t) = all_completed_at {
        if final_clock > 0 {
            println!(
                "  All tasks completed:    {} ({:.1}% of simulation)",
                fmt_duration_ns(t),
                t as f64 / final_clock as f64 * 100.0,
            );
        } else {
            println!("  All tasks completed:    {}", fmt_duration_ns(t));
        }
    } else if completed_count > 0 {
        println!("  Tasks completed:        {completed_count}/{total_tasks}");
    }
}

/// Print preemption-related RBC stats at end of simulation.
///
/// Shows the longest total RBC for any single structop, the longest
/// unbroken interval between kfuncs, and the REPLAY_MARGIN constant
/// for comparison. Only printed when RBC data was collected.
fn print_preemption_stats(longest_structop_rbc: u64, longest_rbc_interval: u64, _is_e9: bool) {
    if longest_structop_rbc == 0 {
        return;
    }
    let margin = crate::preempt::REPLAY_MARGIN;
    println!();
    println!("Preemption stats:");
    println!("  longest_structop_rbc:    {longest_structop_rbc}");
    println!("  longest_rbc_interval:    {longest_rbc_interval}  (between kfuncs)");
    println!("  REPLAY_MARGIN:           {margin}");
}

/// Reset kfunc accumulators and enable the RBC counter before an ops call.
///
/// Set `ops_context` on both shared `SimulatorState` and the per-thread
/// TLS (`CURRENT_OPS_CONTEXT`).
///
/// The per-thread copy is read by the PMU signal handler, which fires
/// asynchronously. Using TLS avoids cross-worker contamination: when
/// Worker A yields, Worker B may modify the shared state. When Worker A
/// resumes and its pending signal fires, reading from TLS gives the
/// correct context (the one Worker A set before yielding) rather than
/// whatever Worker B left on the shared state.
fn set_ops_context(state: &mut SimulatorState, ctx: OpsContext) {
    state.ops_context = ctx;
    crate::preempt::set_current_ops_context(ctx);
}

/// The kfunc counters are always reset so that `charge_sched_time` can
/// apply the accumulated kfunc cost even when the RBC counter is
/// unavailable (VM/container, etc.).
///
/// Resets the PMU counter and caches its fd in TLS for lock-free
/// enable/disable, but does NOT enable it. The counter is enabled
/// later by `sim_callback!` right before C code runs, ensuring
/// only C scheduler branches are measured. Sets the RBC pause depth
/// to 1 (disabled) so that `enable_rbc_counter()` in `sim_callback!`
/// will transition depth 1→0 and enable the counter.
fn start_rbc(state: &mut SimulatorState) {
    state.rbc_kfunc_calls = 0;
    state.rbc_kfunc_ns = 0;
    if state.e9_fns.is_some() {
        // e9patch mode: snapshot the deterministic software counter.
        // The counter decrements on each Jcc, so `snapshot - current = branches`.
        let snapshot = crate::preempt::e9_read_counter();
        state.rbc_e9_snapshot = snapshot;
        state.rbc_e9_last_kfunc = snapshot;
    } else if let Some(ref rbc) = state.rbc_counter {
        let _ = rbc.reset();
        // Cache the fd for lock-free disable/enable in with_sim() and
        // sim_callback!. Do NOT enable yet — the counter is enabled by
        // sim_callback! right before the C code call block.
        kfuncs::set_rbc_counter_fd(rbc.raw_fd());
        // Set depth to 1 (disabled). sim_callback!'s enable_rbc_counter()
        // will decrement to 0, enabling the counter for C code.
        kfuncs::set_rbc_pause_depth(1);
        // PMU counter starts at 0 after reset.
        state.rbc_pmu_last_kfunc = 0;
    }
}

/// Update `p->se.sum_exec_runtime` before a scheduler callback.
///
/// In the kernel, `update_curr()` increments `sum_exec_runtime` by the
/// CPU time consumed since `exec_start`. The simulator mirrors this by
/// computing elapsed time from `task_started_at` and adding it to the
/// base snapshot taken when the task started running.
fn update_sum_exec(raw: *mut c_void, base: TimeNs, elapsed: TimeNs) {
    ffi::task_set_sum_exec_runtime(raw, base + elapsed);
}

/// Charge `delta_ns` of CPU time consumed by `pid` against its cgroup's
/// `cpu.max` quota (and finite ancestors).
///
/// Trace-only `cpu.max` charge record. Called from `stop_and_reenqueue`
/// and `handle_task_phase_complete` — both points where the engine
/// knows exactly how much on-CPU time the task consumed since it
/// started running.
///
/// Records a `CgroupBwCharge` trace event for downstream analysis.
/// The actual cpu.max accounting + throttle decisions are made by
/// the scheduler-side `cgroup_bw` library
/// (`account_task_runtime` -> `scx_cgroup_bw_consume` ->
/// `accounting_timerfn` -> `cbw_throttle_cgroups`); the engine no
/// longer maintains a parallel Rust mirror of that work
/// (tg `shrink-rust-bandwidthmanager-518-to-30-lines-no-fake-
/// approximation`, scx-sim/CLAUDE.md "CRITICAL: No-Stub Rule").
fn charge_cgroup_bw(
    fields: &mut crate::kfuncs::SimFields<'_>,
    pid: Pid,
    delta_ns: TimeNs,
    now_ns: TimeNs,
    cpu: CpuId,
) {
    if delta_ns == 0 {
        return;
    }
    let Some(&cgid) = fields.task_to_cgid.get(&pid) else {
        return;
    };
    fields.sim.trace.record(
        now_ns,
        cpu,
        TraceKind::CgroupBwCharge {
            pid,
            cgid,
            delta_ns,
        },
    );
}

/// Returns `Some(cgid)` if `pid`'s cgroup is currently throttled by
/// the scheduler-side `cpu.max` library. Used by the DSQ-pop
/// admission gate to refuse dispatch of tasks in a throttled cgroup.
///
/// Sole data source is the loaded scheduler's `cgroup_bw` library
/// (via wrapper.c forwarder `scxsim_cgroup_bw_is_cgroup_throttled`).
/// Schedulers that do not link the library (simple, tickless) get
/// `None` -- the only honest answer when the scheduler does not
/// model cpu.max.
///
/// History: pre-Stage-E, this function had a fallback to an engine-
/// side `BandwidthManager` Rust mirror. That mirror was deleted in
/// `tg shrink-rust-bandwidthmanager-518-to-30-lines-no-fake-
/// approximation` per the No-Stub Rule -- the library is the single
/// source of truth (scx-sim/CLAUDE.md "CRITICAL: No-Stub Rule").
fn pid_is_bw_throttled<S: crate::ffi::Scheduler>(
    scheduler: &crate::scheduler_wrapper::SchedulerWrapper<S>,
    fields: &crate::kfuncs::SimFields<'_>,
    pid: Pid,
) -> Option<CgroupId> {
    let cgid = *fields.task_to_cgid.get(&pid)?;
    let throttled = scheduler.is_cgroup_throttled(cgid.0)?;
    if throttled {
        Some(cgid)
    } else {
        None
    }
}

/// Eagerly remove a bw-throttled task from its local DSQ + stash it in
/// `bw_blocked[cgid]` for re-runnable on the next cgroup_bw replenish.
///
/// Mirrors the kernel's bandwidth controller behavior: when a task
/// belongs to a throttled cgroup, the kernel removes it from sched_ext
/// queues entirely (not just blocks dispatch). Replaces the LAZY
/// `CgroupBwDenied` admission gate that left throttled tasks in the
/// local DSQ — see tg
/// `scxsim-eager-cgroup-bw-throttle-via-dequeue-wakeup-cycle` for the
/// motivation (live LAVD's `dsq_insert : dsq_insert_vtime` ratio of
/// 13:1 vs scxsim's 1:16,000 inversion is caused by sim's throttled
/// tasks never re-traversing the wakeup→select_cpu→can_direct_dispatch
/// chain that the simple-insert direct-dispatch path lives on).
///
/// Records `TraceKind::CgroupBwDequeueOnThrottle` so the eager-throttle
/// cycle is observable in scxsim's JSONL emit + perfetto trace.
///
/// DANGER TODO(sim-eager-throttle-v2): does NOT call `ops.dequeue` /
/// `ops.quiescent` on the stashed task. Real kernel does call those
/// when the bandwidth controller dequeues a task. V1 of the eager
/// model skips these notifications because the call site is mid-
/// dispatch and the sim_callback! re-entrancy is awkward; if the
/// validation matrix shows scheduler-internal state diverges
/// (e.g. LAVD's per-task ops_state stays Queued when it shouldn't),
/// V2 will add the explicit ops.dequeue + ops.quiescent calls.
fn eager_stash_throttled(s: &mut crate::kfuncs::SimState, pid: Pid, cgid: CgroupId, cpu: CpuId) {
    // Pop the task from the local DSQ (we already verified it's at
    // the front).
    let popped = s.sim.cpus[cpu.0 as usize].local_dsq.pop_front();
    debug_assert_eq!(popped, Some(pid), "front-of-DSQ contract violated");

    // Stash in bw_blocked[cgid] preserving FIFO insertion order.
    s.sim.bw_blocked.entry(cgid).or_default().push_back(pid);

    // Record the kernel-faithful "dequeue on throttle" event.
    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
    s.sim.trace.record(
        __local_t,
        cpu,
        TraceKind::CgroupBwDequeueOnThrottle { pid, cgid },
    );
    debug!(
        pid = pid.0,
        cgid = cgid.0,
        cpu = cpu.0,
        "cgroup_bw: eager-dequeue throttled task (stashed in bw_blocked)"
    );
}

/// Compute the maximum permitted run-time slice (ns) for `pid` based on
/// its cgroup's remaining quota in the current `cpu.max` period. Returns
/// `None` when no bandwidth cap applies — either because (a) the task
/// is not in a tracked cgroup, (b) the loaded scheduler does not link
/// the `cgroup_bw` library (e.g. simple, tickless), or (c) the library
/// reports the cgroup as unlimited.
///
/// Asks the library directly via the wrapper.c forwarder
/// `scxsim_cgroup_bw_budget_remaining` so the LIBRARY is the single
/// source of truth for budget accounting (No-Stub Rule, scx-sim/CLAUDE.md).
/// The wrapper.c sentinel `u64::MAX` is mapped to `None` here.
fn pid_bw_max_run_ns<S: crate::ffi::Scheduler>(
    scheduler: &crate::scheduler_wrapper::SchedulerWrapper<S>,
    fields: &crate::kfuncs::SimFields<'_>,
    pid: Pid,
) -> Option<u64> {
    let cgid = *fields.task_to_cgid.get(&pid)?;
    let remaining = scheduler.cgroup_bw_budget_remaining(cgid.0)?;
    if remaining == u64::MAX {
        // wrapper.c sentinel: "no cap" (unknown / untracked / unlimited).
        None
    } else {
        Some(remaining)
    }
}

/// Disable the RBC counter, read the count, and charge scheduler overhead to `cpu`.
///
/// Three modes:
/// 1. **e9patch** (`e9_fns.is_some()`): Uses the deterministic software counter.
///    The e9 trampoline decrements a counter at every Jcc in the instrumented
///    `.so`. `start_rbc()` snapshots the counter; here we compute the difference.
///    Fully deterministic — no PMU hardware involved.
/// 2. **PMU** (`rbc_counter.is_some()`): Uses the hardware PMU counter.
///    Signal delivery has skid but counter values are deterministic for
///    a given instruction stream. See sim-70abc8 for known measurement bugs.
/// 3. **Fallback** (neither): Uses accumulated kfunc cost with a minimum floor.
fn charge_sched_time(state: &mut SimulatorState, cpu: CpuId, ops: &str) {
    let idx = cpu.0 as usize;

    // Accumulate per-CPU structop stats for the summary table.
    // Always counted here for consistency across all modes.
    if idx < state.structop_accum.len() {
        state.structop_accum[idx].cpu_count += 1;
        state.structop_accum[idx].kfunc_count += state.rbc_kfunc_calls as u64;
    }

    if state.e9_fns.is_some() {
        // e9patch mode: deterministic software branch count.
        let current = crate::preempt::e9_read_counter();
        // Counter decrements, so snapshot - current = branches executed.
        // Clamp to 0 in case the counter was re-armed between start and now.
        let count = (state.rbc_e9_snapshot - current).max(0) as u64;
        // Track the final RBC interval (from last kfunc to structop end).
        let final_interval = (state.rbc_e9_last_kfunc - current).max(0) as u64;
        if final_interval > state.longest_rbc_interval {
            state.longest_rbc_interval = final_interval;
        }
        // Track longest total structop RBC.
        if count > state.longest_structop_rbc {
            state.longest_structop_rbc = count;
        }
        if let Some(ns_per_rbc) = state.sched_overhead_rbc_ns {
            let rbc_ns = count * ns_per_rbc;
            let kfunc_ns = state.rbc_kfunc_ns;
            let total_ns = rbc_ns + kfunc_ns;
            state.cpus[cpu.0 as usize].local_clock += total_ns;
            trace!(
                ops,
                rbc = count,
                kfuncs = state.rbc_kfunc_calls,
                kfunc_ns,
                total_ns,
                "sched overhead"
            );
            // Accumulate RBC into structop summary.
            if idx < state.structop_accum.len() {
                state.structop_accum[idx].rbc_total += count;
            }
        }
    } else if let Some(ref rbc) = state.rbc_counter {
        // Counter was already disabled by sim_callback! or the caller
        // (e.g. call_enqueue) right after C code returned. Read the
        // stopped value — it contains only C scheduler branches.
        let count = rbc.read().unwrap_or(0);
        // Track the final RBC interval (from last kfunc to structop end).
        let final_interval = count.saturating_sub(state.rbc_pmu_last_kfunc);
        if final_interval > state.longest_rbc_interval {
            state.longest_rbc_interval = final_interval;
        }
        // Track longest total structop RBC (PMU mode).
        if count > state.longest_structop_rbc {
            state.longest_structop_rbc = count;
        }
        if let Some(ns_per_rbc) = state.sched_overhead_rbc_ns {
            let rbc_ns = count * ns_per_rbc;
            let kfunc_ns = state.rbc_kfunc_ns;
            let total_ns = rbc_ns + kfunc_ns;
            state.cpus[cpu.0 as usize].local_clock += total_ns;
            kfuncs::clock_window_check(cpu, state.cpus[cpu.0 as usize].local_clock);
            trace!(
                ops,
                rbc = count,
                kfuncs = state.rbc_kfunc_calls,
                kfunc_ns,
                total_ns,
                "sched overhead"
            );
            // Accumulate RBC into structop summary.
            if idx < state.structop_accum.len() {
                state.structop_accum[idx].rbc_total += count;
            }
        }
    } else if state.overhead.enabled {
        // Fallback: no RBC counter — use accumulated kfunc cost directly.
        // Each with_sim() call adds its kfunc_cost tier to rbc_kfunc_ns,
        // so this advances local_clock by the sum of all kfunc costs in
        // this callback invocation. A minimum of MIN_CALLBACK_COST_NS
        // ensures time advances even for callbacks that make no kfuncs.
        //
        // Gated on overhead.enabled so that instant_timing() scenarios
        // get zero-cost transitions (matching the pre-fallback behavior).
        const MIN_CALLBACK_COST_NS: u64 = 50;
        let kfunc_ns = state.rbc_kfunc_ns;
        let total_ns = kfunc_ns.max(MIN_CALLBACK_COST_NS);
        state.cpus[cpu.0 as usize].local_clock += total_ns;
        kfuncs::clock_window_check(cpu, state.cpus[cpu.0 as usize].local_clock);
        trace!(
            ops,
            kfuncs = state.rbc_kfunc_calls,
            kfunc_ns,
            total_ns,
            "sched overhead (fallback)"
        );
    }
}

/// Record a determinism checkpoint after a scheduler callback.
///
/// Only records if aggressive determinism mode is enabled.
/// The `event` parameter identifies the callback type.
#[inline]
fn maybe_record_checkpoint(state: &SimulatorState, event: CheckpointEvent, cpu: CpuId) {
    if !is_determinism_mode_enabled() {
        return;
    }

    // Read RBC count if available.
    //
    // In PMU mode (rbc_counter is Some, e9_fns is None), the counter
    // measures C scheduler branches but includes a small fixed overhead
    // from the enable/disable ioctl boundary (a few Rust branches
    // between the ioctl return and the C function call/return). These
    // boundary branches are deterministic but the PMU counter's
    // interaction with OS thread scheduling at the ioctl transition
    // can cause 1-3 branch variations. For determinism checking, we
    // skip the RBC field — the memory hash is the authoritative signal.
    //
    // In e9patch mode (e9_fns is Some), the software counter is fully
    // deterministic — use it for RBC comparison.
    let rbc_count = if state.e9_fns.is_some() {
        // e9patch: read deterministic software counter.
        let current = crate::preempt::e9_read_counter();
        (state.rbc_e9_snapshot - current).max(0) as u64
    } else {
        // PMU or no counter: skip RBC in checkpoint comparison.
        0
    };

    // Compute memory hash
    let memory_hash = state.compute_state_hash();

    // Instruction pointer is 0 for engine-driven checkpoints (not signal-driven).
    // Signal-driven preemption points have real RIP values.
    let instruction_pointer = 0;

    record_checkpoint(event, instruction_pointer, rbc_count, memory_hash, cpu);
}

/// Drop the MutexGuard, install SIM_ARC, call scheduler code, then reacquire.
///
/// Takes `$s` (the `&mut SimState` deref of `$guard`) and shadows it to
/// end the borrow before dropping the guard. After relocking, rebinds `$s`.
///
/// The call block uses `SchedulerWrapper` methods which are safe wrappers
/// around the underlying FFI calls.
///
/// Usage:
/// ```ignore
/// let mut guard = sim_arc.lock().unwrap();
/// let s = &mut *guard;
/// s.sim.current_cpu = cpu;
/// sim_callback!(s, guard, sim_arc, cpu, {
///     self.scheduler.init();
/// });
/// // s and guard are now rebound (relocked)
/// ```
macro_rules! sim_callback {
    ($s:ident, $guard:ident, $arc:expr, $cpu:expr, $call:block) => {
        // Read context through $s (the caller's &mut *guard deref).
        // NLL ends $s's borrow on $guard after the last use (install_callback_ctx).
        let __cpu = $cpu;
        $s.sim.current_cpu = __cpu;
        kfuncs::set_sim_clock($s.sim.cpus[__cpu.0 as usize].local_clock, Some(__cpu));
        kfuncs::install_callback_ctx(kfuncs::CallbackContext {
            current_cpu: __cpu,
            ops_context: $s.sim.ops_context,
            waker_task_raw: $s.sim.waker_task_raw,
        });
        // $s is no longer used. NLL ends its borrow on $guard.
        drop($guard);
        kfuncs::install_sim_arc(&$arc);
        // Allow PMU preemption during scheduler C code.
        // process_event inhibits preemption around the entire event
        // processing to prevent signal handler deadlocks with the
        // SIM_ARC mutex.  Here we temporarily allow it while C code
        // runs, since the mutex is not held.
        crate::preempt::allow_preemption();
        // Enable the RBC counter RIGHT BEFORE C code runs.
        // All Rust infrastructure above (mutex drop, preemption management)
        // executed with the counter disabled, so their branches are not
        // counted. Only C scheduler code branches are measured. (sim-70abc8)
        kfuncs::enable_rbc_counter();
        $call
        // Re-inhibit preemption IMMEDIATELY after C code returns.
        // This must happen before disable_rbc_counter() to close a
        // race window: if a queued PMU signal fires between $call
        // returning and inhibit_preemption(), the handler would yield
        // (count is 0), parking this worker while another worker tries
        // to lock SIM_ARC, causing a token-ring/mutex deadlock.
        crate::preempt::inhibit_preemption();
        // Disable the RBC counter after inhibiting preemption.
        // All Rust infrastructure below (mutex reacquire, context
        // restore) runs with the counter disabled.
        kfuncs::disable_rbc_counter();
        kfuncs::clear_sim_arc();
        crate::preempt::pause_timer();
        $guard = $arc.lock().unwrap();
        if let Some(__ctx) = kfuncs::get_callback_ctx() {
            $guard.sim.current_cpu = __ctx.current_cpu;
            $guard.sim.ops_context = __ctx.ops_context;
            $guard.sim.waker_task_raw = __ctx.waker_task_raw;
        }
        kfuncs::clear_callback_ctx();
        // Caller must rebind: let $s = &mut *$guard;
    };
}

impl<S: Scheduler> Simulator<S> {
    pub fn new(scheduler: S) -> Self {
        Simulator {
            scheduler: SchedulerWrapper::new(scheduler),
        }
    }

    /// Check for stalled runnable tasks (watchdog).
    ///
    /// Iterates all tasks and checks if any Runnable task has been waiting
    /// longer than the timeout. Returns the stall with the lowest PID for
    /// determinism (HashMap iteration order is non-deterministic).
    fn check_watchdog(
        tasks: &HashMap<Pid, SimTask>,
        current_time: TimeNs,
        timeout_ns: TimeNs,
    ) -> Option<ExitKind> {
        let mut worst: Option<(Pid, TimeNs)> = None;
        for task in tasks.values() {
            if matches!(task.state, TaskState::Runnable) {
                if let Some(runnable_at) = task.runnable_at_ns {
                    let runnable_for = current_time.saturating_sub(runnable_at);
                    if runnable_for > timeout_ns {
                        // Pick the lowest PID for deterministic error reporting.
                        let dominated = worst.map(|(pid, _)| task.pid < pid).unwrap_or(true);
                        if dominated {
                            worst = Some((task.pid, runnable_for));
                        }
                    }
                }
            }
        }
        worst.map(|(pid, runnable_for_ns)| ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        })
    }

    /// Call the scheduler's enqueue callback with proper state tracking.
    ///
    /// Sets `ops_context = Enqueue`, calls the scheduler, charges RBC time,
    /// records a checkpoint, clears ops_context, and resolves deferred
    /// dispatch. Caller must be inside an `enter_sim` / `exit_sim` scope.
    ///
    /// Note: callers that need `set_task_ops_state(pid, Queued)` must do
    /// so before calling this helper (most do, but cpu_offline drain doesn't).
    #[allow(dead_code)]
    fn call_enqueue(&self, cpu: CpuId, raw: *mut c_void, flags: u64, state: &mut SimulatorState) {
        set_ops_context(state, OpsContext::Enqueue);
        start_rbc(state);
        kfuncs::enable_rbc_counter();
        self.scheduler.enqueue(TaskPtr::new(raw), flags);
        kfuncs::disable_rbc_counter();
        charge_sched_time(state, cpu, "enqueue");
        maybe_record_checkpoint(state, CheckpointEvent::Enqueue, cpu);
        set_ops_context(state, OpsContext::None);
        state.resolve_pending_dispatch(cpu);
    }

    /// Run a scenario and return the trace.
    pub fn run(&self, scenario: Scenario) -> Trace {
        let result = self.run_internal(scenario, &mut NoopMonitor);
        result.trace
    }

    /// Run a scenario with a monitor, returning the full result.
    ///
    /// The monitor is called at each scheduling event (Running, Stopping,
    /// Quiescent, Dispatched) with a [`ProbeContext`] that includes the
    /// raw task pointer for scheduler-internal inspection.
    pub fn run_monitored(&self, scenario: Scenario, monitor: &mut dyn Monitor) -> SimulationResult {
        self.run_internal(scenario, monitor)
    }

    /// Internal simulation loop shared by `run()` and `run_monitored()`.
    fn run_internal(&self, scenario: Scenario, monitor: &mut dyn Monitor) -> SimulationResult {
        // Reset global C state that persists between simulation runs.
        // These static variables are compiled into the main binary (not the
        // scheduler .so), so they survive across runs and cause non-determinism.
        //
        // NOTE: We do NOT call scx_test_map_clear_all() here because maps are
        // registered during scheduler setup() which happens before run_internal().
        // Clearing maps here would break map lookups in the scheduler.
        ffi::reset_task_state();

        let nr_cpus = scenario.nr_cpus;
        let smt = scenario.smt_threads_per_core;

        // Build CPUs with SMT sibling groups
        let mut cpus: Vec<SimCpu> = (0..nr_cpus).map(|i| SimCpu::new(CpuId(i))).collect();
        if smt > 1 {
            for core_base in (0..nr_cpus).step_by(smt as usize) {
                let siblings: Vec<CpuId> = (core_base..core_base + smt).map(CpuId).collect();
                for &sib in &siblings {
                    cpus[sib.0 as usize].siblings = siblings.clone();
                }
            }
        }

        // Assign LLC domain IDs (CCX topology)
        if let Some(cpus_per_llc) = std::num::NonZeroU32::new(scenario.cpus_per_llc) {
            for i in 0..nr_cpus {
                cpus[i as usize].llc_id = i / cpus_per_llc.get();
            }
        }

        // Initialize all CPUs as idle in the C cpumasks
        for i in 0..nr_cpus {
            ffi::cpumask_set_all(i as i32);
            ffi::cpumask_set_idle(i as i32);
            // All CPUs idle => all cores fully idle
            ffi::cpumask_set_idle_smt(i as i32);
        }

        // Build tasks
        let mut tasks: HashMap<Pid, SimTask> = HashMap::new();
        let mut task_raw_to_pid: HashMap<usize, Pid> = HashMap::new();
        let task_pid_to_raw: HashMap<Pid, usize> = HashMap::new();

        // Allocate a synthetic idle task for bpf_get_current_task_btf() fallback.
        // In the kernel, there's always a task running (idle task on idle CPUs).
        // PF_IDLE = 0x2, mm = NULL (calloc-zeroed).
        // The RAII handle owns the C allocation; Drop frees it automatically.
        let idle_task_handle = SimTaskHandle::new_idle();
        let idle_task_raw = idle_task_handle.as_raw();

        for def in &scenario.tasks {
            let task = SimTask::new(def, nr_cpus);
            let raw_addr = task.raw() as usize;
            task_raw_to_pid.insert(raw_addr, task.pid);
            // NOTE: We do NOT insert into task_pid_to_raw here. Registration
            // is deferred until after init_task completes. This ensures that
            // self-referencing real_parent pointers (where real_parent == p)
            // cause bpf_task_from_pid() to return NULL during init_task,
            // triggering the correct initialization path in scheduler code
            // (e.g., LAVD's avg_runtime_wall = sys_stat.slice_wall).
            // Set up cpus_ptr — restricted to allowed_cpus if specified
            ffi::task_setup_cpumask(task.raw(), def.allowed_cpus.as_deref());
            // Set mm pointer for address-space grouping (wake-affine scheduling)
            if let Some(mm_id) = def.mm_id {
                // Synthetic non-NULL pointer: never dereferenced, only compared.
                // Each unique MmId maps to a distinct non-NULL value.
                let mm_ptr = ((mm_id.0 as usize) + 1) * 0x1000;
                ffi::task_set_mm(task.raw(), mm_ptr as *mut c_void);
            }
            tasks.insert(task.pid, task);
        }

        // Set up parent-child relationships (must happen after all tasks exist).
        for def in &scenario.tasks {
            if let Some(parent_pid) = def.parent_pid {
                let parent_raw = tasks
                    .get(&parent_pid)
                    .unwrap_or_else(|| {
                        panic!(
                            "parent_pid {:?} for task {:?} does not exist",
                            parent_pid, def.pid
                        )
                    })
                    .raw();
                let child_raw = tasks[&def.pid].raw();
                ffi::task_set_real_parent(child_raw, parent_raw);
            }
        }

        // Build simulator state (shared with kfuncs via thread-local)
        //
        // In e9patch mode, the e9 software counter provides a deterministic
        // branch count — skip creating the PMU counter (which has signal skid).
        // The e9 trampoline counts every Jcc in the instrumented `.so`,
        // which is exactly the set of branches the PMU counter measures.
        let is_e9 = scenario
            .preemptive
            .as_ref()
            .is_some_and(|cfg| cfg.preempt_mode == PreemptMode::E9patch);
        let rbc_ns = scenario.sched_overhead_rbc_ns.filter(|&ns| ns > 0);
        let rbc_counter = if is_e9 {
            // e9patch mode: use deterministic software counter instead of PMU.
            None
        } else if rbc_ns.is_some() {
            // PMU mode: the RBC counter is created with pid=0 (current thread)
            // and measures retired conditional branches on the main engine
            // thread. In interleave/preemptive mode, concurrent batches run
            // scheduler callbacks on worker threads where this counter is
            // invisible — those paths use per-thread `measure_counter`
            // instances instead. Sequential callbacks (global events,
            // single-CPU batches) still run on the main thread and benefit
            // from this counter. Concurrent processing temporarily takes
            // the counter out of state to prevent
            // worker threads from accessing a main-thread-only PMU fd.
            perf::try_create_rbc_counter()
        } else {
            None
        };

        let mut state = SimulatorState {
            cpus,
            dsqs: DsqManager::new(),
            current_cpu: CpuId(0),
            trace: Trace::with_warmup(scenario.nr_cpus, &scenario.tasks, scenario.warmup_ns),
            clock: 0,
            task_raw_to_pid,
            task_pid_to_raw,
            rng: SmallRng::seed_from_u64(scenario.seed as u64),
            ops_context: OpsContext::None,
            pending_dispatches: Default::default(),
            dsq_iter: None,
            staged_events: Vec::new(),
            task_last_cpu: HashMap::new(),
            task_ops_state: BTreeMap::new(),
            reenqueue_local_requested: false,
            pending_timers: [None; crate::kfuncs::MAX_BPF_TIMERS],
            waker_task_raw: None,
            idle_task_raw,
            noise: scenario.noise.clone(),
            overhead: scenario.overhead.clone(),
            rbc_counter,
            sched_overhead_rbc_ns: rbc_ns,
            rbc_kfunc_calls: 0,
            rbc_kfunc_ns: 0,
            rbc_e9_snapshot: 0,
            rbc_e9_last_kfunc: 0,
            rbc_pmu_last_kfunc: 0,
            longest_structop_rbc: 0,
            longest_rbc_interval: 0,
            bpf_error: None,
            interleave: scenario.interleave,
            stochastic_timer_interleave: scenario.stochastic_timer_interleave,
            stochastic_timer_interleave_window_ns: scenario.stochastic_timer_interleave_window_ns,
            stochastic_timer_interleave_one_in: scenario.stochastic_timer_interleave_one_in,
            targeted_cbw_yield_sites: scenario.targeted_cbw_yield_sites,
            targeted_cbw_yield_window_ns: scenario.targeted_cbw_yield_window_ns,
            targeted_cbw_yield_limit: scenario.targeted_cbw_yield_limit,
            targeted_cbw_yield_count: 0,
            preemptive: scenario.preemptive.clone(),
            replay_trace: scenario.replay_trace.clone(),
            replay_backend: None,    // Initialized below after state is built.
            e9_replay_backend: None, // Initialized below if e9 replay is active.
            e9_fns: None,            // Initialized below if e9patch mode is active.
            structop_accum: vec![
                crate::preempt::StructopInfo::default();
                scenario.nr_cpus as usize
            ],
            native_concurrent: scenario.native_concurrent,
            bw_blocked: std::collections::BTreeMap::new(),
        };

        // Build the persistent replay backend once if we have a replay trace.
        // This must happen after state construction because the backend holds
        // cursors that track progress across dispatch rounds -- creating a
        // fresh backend each round would reset cursors to index 0.
        //
        // We create cursors for nr_cpus workers (the maximum possible), not
        // trace.num_workers(). In any given round, only a subset of CPUs may
        // need dispatch, and the worker count varies. Extra workers beyond the
        // trace's worker count get empty cursors (no targets to replay).
        let is_e9_mode = state
            .preemptive
            .as_ref()
            .is_some_and(|cfg| cfg.preempt_mode == PreemptMode::E9patch);

        if let Some(ref trace) = state.replay_trace {
            let (ts_min, ts_max) = state
                .preemptive
                .as_ref()
                .map(|cfg| (cfg.timeslice_min, cfg.timeslice_max))
                .unwrap_or((100, 500));

            if is_e9_mode {
                // E9patch replay: resolve fns first, then build backend.
                let e9_fns = self.scheduler.resolve_e9_fns().unwrap_or_else(|| {
                    panic!(
                        "e9patch replay mode requires e9 trampoline symbols in the \
                         scheduler .so. Rebuild with: make -C schedulers e9"
                    )
                });
                state.e9_fns = Some(e9_fns);
                // Auto-detect RIP mode from the trace's break_on event.
                // Traces recorded with --break-on insn have preemption
                // points at arbitrary instruction addresses (not just Jcc),
                // requiring a RIP-patched .so and RIP-targeted replay.
                let rip_mode = trace.break_on() == crate::perf::PmuEvent::InstructionsRetired;
                state.e9_replay_backend = Some(E9PatchReplayBackend::new(
                    trace,
                    nr_cpus as usize,
                    ts_min,
                    ts_max,
                    e9_fns,
                    rip_mode,
                ));
            } else {
                state.replay_backend = Some(ReplayBackend::new(
                    trace,
                    nr_cpus as usize,
                    ts_min,
                    ts_max,
                    scenario.no_pmu_signal,
                ));
            }
        }

        // Resolve e9patch function pointers if e9patch mode is active
        // (recording, not replay — replay resolves them above).
        // The shared RBC page must already be mmap'd (done by the caller
        // before loading the .so, since e9-instrumented Jcc instructions
        // access the fixed address during DT_INIT).
        if is_e9_mode && state.e9_fns.is_none() {
            state.e9_fns = self.scheduler.resolve_e9_fns();
            if state.e9_fns.is_none() {
                panic!(
                    "e9patch mode requires e9 trampoline symbols in the scheduler .so. \
                     Rebuild with: make -C schedulers e9"
                );
            }
        }

        // Set CPU ID width for log formatting
        kfuncs::set_sim_cpu_width(nr_cpus);

        // Build cgroup registry from scenario definitions. We create and
        // install it before ops.init() so that bpf_for_each(css, ...) inside
        // init can discover cgroups (e.g. mitosis with cpu_controller_disabled).
        // In the real kernel the cgroup hierarchy already exists when init runs.
        let mut cgroup_registry = CgroupRegistry::new(nr_cpus, scenario.max_cgroups);
        for cg_def in &scenario.cgroups {
            let parent_cgid = match &cg_def.parent_name {
                Some(parent) => {
                    cgroup_registry
                        .get_by_name(parent)
                        .unwrap_or_else(|| panic!("parent cgroup '{parent}' not found"))
                        .cgid
                }
                None => CgroupId::ROOT,
            };
            cgroup_registry.create(&cg_def.name, parent_cgid, cg_def.cpuset.clone());
        }

        // Build event queue (created early so it can be bundled into SimState)
        let events = EventQueue::new(scenario.seed, scenario.fixed_priority);

        // Bundle all shared state into SimState. From this point forward,
        // all access goes through the Arc<Mutex<SimState>>.
        let sim_arc: SimArc = Arc::new(Mutex::new(SimState {
            sim: state,
            tasks,
            events,
            cgroup_registry,
            task_to_cgid: HashMap::new(),
        }));
        // Install the Arc in ENGINE_SIM_ARC so enter_sim can propagate it
        // to SIM_ARC for kfuncs and cgroup callbacks.
        kfuncs::set_engine_sim_arc(&sim_arc);
        // Lock the Arc for engine work. Dropped before C calls via sim_callback!.
        let mut s = sim_arc.lock().unwrap();

        // If --wait-debugger was requested, pause so the user can attach a
        // debugger while scheduler symbols are loaded but before init() runs.
        if scenario.wait_debugger {
            wait_for_debugger(&self.scheduler);
        }

        // Initialize scheduler
        {
            let cpu = s.sim.current_cpu;
            // Populate CSS iterator so bpf_for_each(css, ...) works in init.
            s.cgroup_registry.prepare_css_iter_from_root();
            start_rbc(&mut s.sim);
            #[allow(unused_assignments)]
            let mut rc = 0i32;
            sim_callback!(s, s, sim_arc, cpu, {
                rc = self.scheduler.init();
            });
            charge_sched_time(&mut s.sim, CpuId(0), "init");
            assert!(rc == 0, "scheduler init failed with rc={rc}");
        }

        // Call cgroup_init for each cgroup (root first, then children in order).
        // In the kernel, cgroup_init is called for all existing cgroups when
        // the scheduler is loaded.
        {
            let cpu = s.sim.current_cpu;
            // Refresh CSS iterator so cgroup_init callbacks can use
            // bpf_for_each(css, ...) if needed.
            s.cgroup_registry.prepare_css_iter_from_root();
            // Snapshot cgids and raw pointers before dropping guard for C calls.
            let cg_init_list: Vec<(CgroupId, *mut c_void)> = s
                .cgroup_registry
                .all_cgids_preorder()
                .into_iter()
                .filter_map(|cgid| s.cgroup_registry.get_raw(cgid).map(|raw| (cgid, raw)))
                .collect();
            for (cgid, raw) in cg_init_list {
                start_rbc(&mut s.sim);
                #[allow(unused_assignments)]
                let mut rc = 0i32;
                // Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
                // pre-Phase-2 we passed `OptionalPtr::null()` because the
                // weak `scx_cgroup_bw_init` shim accepted NULL safely.
                // The compiled-in cgroup_bw library dereferences
                // `args->bw_period_us` at lib/cgroup_bw.bpf.c:898; pass a
                // C-side default `struct scx_cgroup_init_args` (weight=100,
                // period=100ms, quota=-1, burst=0) instead. Per-cgroup
                // bandwidth still flows through the separate
                // `cgroup_set_bandwidth` invocation below.
                let args_ptr = OptionalPtr::new(default_cgroup_init_args());
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    rc = self.scheduler.cgroup_init(TaskPtr::new(raw), args_ptr);
                });
                // TOP-2 of cpu-bw-stall-bug TraceKind easy-win bundle.
                s.sim
                    .trace
                    .record(__local_t, cpu, TraceKind::CgroupInit { cgid, rc });
                charge_sched_time(&mut s.sim, CpuId(0), "cgroup_init");
                assert!(rc == 0, "cgroup_init failed for cgid={} rc={rc}", cgid.0);
            }
        }

        // Call cgroup_set_bandwidth for cgroups that have bandwidth configured.
        // In the kernel, this is called when writing to cpu.max.
        {
            let cpu = s.sim.current_cpu;
            // Snapshot bandwidth configs before dropping guard.
            // Capture cgid alongside raw + bw fields so we can emit a
            // matching `TraceKind::CgroupSetBandwidth` for each call
            // (TOP-1 of the cpu-bw-stall-bug critical-path TraceKind
            // easy-win bundle — tg `bundle-implement-cpu-bw-critical-...`).
            let bw_configs: Vec<_> = scenario
                .cgroups
                .iter()
                .filter_map(|cg_def| {
                    cg_def.bandwidth.as_ref().and_then(|bw| {
                        s.cgroup_registry.get_by_name(&cg_def.name).map(|info| {
                            (
                                info.cgid,
                                info.raw(),
                                bw.period_us,
                                bw.quota_us,
                                bw.burst_us,
                            )
                        })
                    })
                })
                .collect();
            for (cgid, raw, period_us, quota_us, burst_us) in bw_configs {
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    self.scheduler.cgroup_set_bandwidth(
                        TaskPtr::new(raw),
                        period_us,
                        quota_us,
                        burst_us,
                    );
                });
                s.sim.trace.record(
                    __local_t,
                    cpu,
                    TraceKind::CgroupSetBandwidth {
                        cgid,
                        period_us,
                        quota_us,
                        burst_us,
                    },
                );
                charge_sched_time(&mut s.sim, CpuId(0), "cgroup_set_bandwidth");
            }

            // (Stage E shrink: pre-shrink there was a separate engine-side
            // BandwidthManager configure_from_cgroup_defs() bridge here +
            // a per-cgroup CgroupBwRefill event chain. Both deleted -- the
            // library's own scx_cgroup_bw_init (called via the
            // cgroup_set_bandwidth callback above) configures its own
            // per-cgroup state, and the library's bpf_timer-driven
            // replenish_timerfn handles refill autonomously. The engine
            // does no parallel bookkeeping.)
        }

        // Pre-assign tasks to cgroups before init_task.
        // Build a map from pid to cgroup raw pointer for tasks with cgroup_name.
        let mut task_cgroup_map: HashMap<Pid, *mut c_void> = HashMap::new();
        for def in &scenario.tasks {
            if let Some(ref cg_name) = def.cgroup_name {
                let info = s.cgroup_registry.get_by_name(cg_name).unwrap_or_else(|| {
                    panic!("cgroup '{cg_name}' not found for task {:?}", def.pid)
                });
                let cgrp_raw = info.raw();
                let cgid = info.cgid;
                ffi::task_set_cgroup(s.tasks[&def.pid].raw(), cgrp_raw);
                task_cgroup_map.insert(def.pid, cgrp_raw);
                // Diff 3 wiring: record PID → cgroup-id for engine-side
                // bandwidth charging. Tasks without a cgroup_name belong to
                // the root cgroup (which has no tracked cpu.max) and are
                // simply absent from this map.
                s.task_to_cgid.insert(def.pid, cgid);
            }
        }

        // Call init_task for each task (after scheduler init + cgroup assignment).
        // Tasks are registered in task_pid_to_raw AFTER init_task completes.
        // This ensures self-referencing real_parent pointers don't cause
        // bpf_task_from_pid to return the task itself during init.
        //
        // IMPORTANT: Sort PIDs for deterministic init_task order. HashMap
        // iteration is non-deterministic, and schedulers like LAVD have
        // state that depends on which tasks are already registered (via
        // bpf_task_from_pid parent lookups).
        let mut sorted_pids: Vec<Pid> = s.tasks.keys().copied().collect();
        sorted_pids.sort_by_key(|p| p.0);
        {
            let cpu = s.sim.current_cpu;
            // Snapshot nr_cpus once: it's stable for the duration of the
            // init loop and we use it as the cpumask-hex bit-width.
            let nr_cpus_for_mask = s.sim.cpus.len() as u32;
            for pid in sorted_pids {
                let task_raw = s.tasks[&pid].raw();
                let task_pid = s.tasks[&pid].pid;
                let cgrp_raw = task_cgroup_map.get(&task_pid).copied();
                start_rbc(&mut s.sim);
                #[allow(unused_assignments)]
                let mut rc = 0i32;
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    rc = if let Some(cgrp_raw) = cgrp_raw {
                        self.scheduler
                            .init_task_in_cgroup(TaskPtr::new(task_raw), TaskPtr::new(cgrp_raw))
                    } else {
                        self.scheduler.init_task(TaskPtr::new(task_raw))
                    };
                });
                // TOP-5 of secondary TraceKind easy-win bundle (tg
                // `bundle-implement-secondary-tracekind-easy-wins`):
                // emit the init_task entry+exit pair via JSONL so the
                // live-vs-sim diff harness can validate that sim and
                // live agree on per-task fixture-load handshake order
                // and rc.
                s.sim
                    .trace
                    .record(__local_t, cpu, TraceKind::InitTask { pid: task_pid, rc });
                charge_sched_time(&mut s.sim, CpuId(0), "init_task");
                assert!(rc == 0, "init_task failed for pid={} rc={rc}", task_pid.0);

                // Register task in task_pid_to_raw AFTER init_task completes.
                s.sim.task_pid_to_raw.insert(task_pid, task_raw as usize);

                // Notify scheduler of initial cpumask (mirrors kernel enumeration)
                let cpus_ptr = ffi::task_get_cpus_ptr(task_raw);
                let cpumask_hex = ffi::cpumask_to_hex(cpus_ptr, nr_cpus_for_mask);
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    self.scheduler.set_cpumask(TaskPtr::new(task_raw), cpus_ptr);
                });
                // TOP-7 of secondary TraceKind easy-win bundle: surface
                // the per-task initial cpumask the engine tells the
                // scheduler about. Required for affinity-parity checks
                // (migration-disabled / cpumask-violation bug classes).
                s.sim.trace.record(
                    __local_t,
                    cpu,
                    TraceKind::SetCpumask {
                        pid: task_pid,
                        cpumask_hex,
                    },
                );
                charge_sched_time(&mut s.sim, CpuId(0), "set_cpumask");
            }
        }

        // All CPUs start idle — notify the scheduler so it can begin
        // tracking idle time (e.g., LAVD's idle_start_clk). We
        // temporarily set local_clock to 1 because LAVD treats
        // idle_start_clk == 0 as a sentinel for "not idle".
        for cpu_id in 0..nr_cpus {
            let cpu = CpuId(cpu_id);
            s.sim.cpus[cpu.0 as usize].local_clock = 1;
            set_ops_context(&mut s.sim, OpsContext::UpdateIdle);
            sim_callback!(s, s, sim_arc, cpu, {
                self.scheduler.update_idle(cpu.0 as i32, true);
            });
            // TOP-3 of cpu-bw-stall-bug TraceKind easy-win bundle: surface
            // every ops.update_idle invocation in JSONL. ts is the synthetic
            // local_clock=1 (matches the pre-call local_clock above; matches
            // the sentinel-avoidance comment 5 lines earlier).
            s.sim.trace.record(
                /*ts=*/ 1,
                cpu,
                TraceKind::UpdateIdle { cpu, idle: true },
            );
            s.sim.cpus[cpu.0 as usize].local_clock = 0;
        }

        // Drain any pending timers from scheduler init (e.g. deferred
        // wakeup timer). The CPU is captured by `sim_timer_start_slot`
        // during the init callback. Phase 1 BPF infra scale-up items
        // 1+2: drain ALL slots in ascending slot order so multi-timer
        // schedulers (Phase 2's compiled-in cgroup_bw library) don't
        // lose arms even if init fires multiple slots.
        //
        // Two-step: first snapshot + clear the slot array (mut-borrow
        // of `s.sim`), then push events (mut-borrow of `s.events`).
        // Avoids the `s` re-borrow conflict that the single-step
        // iter_mut + s.events.push form triggers. Stack array sized
        // to MAX_BPF_TIMERS keeps this allocation-free.
        let mut drained: [Option<(crate::types::TimeNs, CpuId)>; crate::kfuncs::MAX_BPF_TIMERS] =
            [None; crate::kfuncs::MAX_BPF_TIMERS];
        for (slot, entry) in s.sim.pending_timers.iter_mut().enumerate() {
            drained[slot] = entry.take();
        }
        for (slot, entry) in drained.iter().enumerate() {
            if let Some((fire_at, cpu)) = *entry {
                s.events.push(
                    fire_at,
                    EventKind::TimerFired {
                        cpu,
                        slot: slot as u8,
                    },
                );
            }
        }

        // Schedule initial TaskWake events for all tasks
        for def in &scenario.tasks {
            // Initial wakes have no waker; use the task's initial prev_cpu
            // (first allowed CPU from cpumask, or CpuId(0) if unrestricted).
            s.events.push(
                def.start_time_ns,
                EventKind::TaskWake {
                    pid: def.pid,
                    waker: None,
                    cpu: def.initial_cpu(),
                },
            );
        }

        // Seed per-CPU tick streams. Each CPU gets a single perpetual tick
        // chain: tick fires → handle_tick → schedule next tick. This matches
        // the kernel's periodic timer interrupt (HZ=250 → 4ms).
        for cpu_id in 0..nr_cpus {
            s.events
                .push(TICK_INTERVAL_NS, EventKind::Tick { cpu: CpuId(cpu_id) });
        }

        // Seed CPU hotplug events from the scenario
        for hp in &scenario.hotplug_events {
            let kind = if hp.online {
                EventKind::CpuOnline { cpu: hp.cpu }
            } else {
                EventKind::CpuOffline { cpu: hp.cpu }
            };
            s.events.push(hp.time_ns, kind);
        }

        // Seed CPU preemption events (higher-priority scheduler class)
        for pe in &scenario.cpu_preempt_events {
            s.events
                .push(pe.release_at_ns, EventKind::CpuRelease { cpu: pe.cpu });
            s.events
                .push(pe.acquire_at_ns, EventKind::CpuAcquire { cpu: pe.cpu });
        }

        // Seed cgroup migration events
        for me in &scenario.cgroup_migrate_events {
            s.events.push(
                me.at_ns,
                EventKind::CgroupMigrate {
                    pid: me.pid,
                    from_cgroup: me.from_cgroup.clone(),
                    to_cgroup: me.to_cgroup.clone(),
                    cpu: CpuId(0),
                },
            );
        }

        // Seed cgroup lifecycle events
        for ce in &scenario.cgroup_create_events {
            s.events.push(
                ce.at_ns,
                EventKind::CgroupCreate {
                    event: ce.clone(),
                    cpu: CpuId(0),
                },
            );
        }
        for de in &scenario.cgroup_destroy_events {
            s.events.push(
                de.at_ns,
                EventKind::CgroupDestroy {
                    event: de.clone(),
                    cpu: CpuId(0),
                },
            );
        }
        for cse in &scenario.cgroup_cpuset_change_events {
            s.events.push(
                cse.at_ns,
                EventKind::CgroupCpusetChange {
                    event: cse.clone(),
                    cpu: CpuId(0),
                },
            );
        }

        // Seed IRQ events from the scenario
        for irq in &scenario.irq_events {
            s.events.push(
                irq.at_ns,
                EventKind::IrqStart {
                    cpu: irq.cpu,
                    irq_type: irq.irq_type,
                    duration_ns: irq.duration_ns,
                    wake_pids: irq.wake_pids.clone(),
                },
            );
        }

        // Seed futex events from the scenario (LAVD lock-holder boosting).
        for fx in &scenario.futex_events {
            s.events.push(
                fx.at_ns,
                EventKind::FutexOp {
                    pid: fx.pid,
                    op: fx.op,
                },
            );
        }

        // Track cgroup resource limit
        let max_cgroups = scenario.max_cgroups;

        // Track the exit kind (may be set by error detection)
        let mut exit_kind = ExitKind::Normal;
        let watchdog_timeout = scenario.watchdog_timeout_ns;
        let ignore_bpf_errors = scenario.ignore_bpf_errors;

        // Log interleaving mode
        if let Some(ref cfg) = s.sim.preemptive {
            if cfg.preempt_mode == PreemptMode::E9patch {
                info!(
                    timeslice_min = cfg.timeslice_min,
                    timeslice_max = cfg.timeslice_max,
                    break_on = %cfg.break_on,
                    "preemptive interleaving enabled (e9patch software RBC, deterministic)"
                );
            } else {
                if !cfg.cooperative_only && scenario.replay_trace.is_none() {
                    warn!(
                        timeslice_min = cfg.timeslice_min,
                        timeslice_max = cfg.timeslice_max,
                        break_on = %cfg.break_on,
                        "preemptive mode: PMU preemption is NONDETERMINISTIC \
                         (use --record-preemptions / --replay-preemptions for deterministic replay)"
                    );
                }
                info!(
                    timeslice_min = cfg.timeslice_min,
                    timeslice_max = cfg.timeslice_max,
                    cooperative_only = cfg.cooperative_only,
                    break_on = %cfg.break_on,
                    "preemptive interleaving enabled (PMU {} timer)", cfg.break_on
                );
            }
        } else if s.sim.interleave {
            info!("cooperative interleaving enabled (kfunc boundaries only)");
        }

        // When preemptive mode is active, install the signal handler once
        // here (at simulation start) instead of per-round. The handler
        // persists until uninstalled at simulation end.
        let preemptive_at_start = s.sim.preemptive.is_some();
        if preemptive_at_start {
            crate::preempt::install_signal_handler();
        }

        // Drop the outer guard before entering the event loop.
        // The event loop manages its own guard lifecycle.
        drop(s);
        'event_loop: loop {
            let mut s = sim_arc.lock().unwrap();
            let t = match s.events.peek_time() {
                Some(t) => t,
                None => break,
            };
            if t > scenario.duration_ns {
                break;
            }
            s.sim.clock = t;

            // Pop one event at a time and process it.
            let event = s.events.pop().expect("peek succeeded but pop failed");
            drop(s);
            if let Some(err) = self.process_event(
                event,
                &sim_arc,
                watchdog_timeout,
                scenario.duration_ns,
                max_cgroups,
                monitor,
            ) {
                exit_kind = err;
                break 'event_loop;
            }
            {
                let mut s = sim_arc.lock().unwrap();
                if let Some(err) = check_bpf_error(&mut s.sim, ignore_bpf_errors) {
                    exit_kind = err;
                    break 'event_loop;
                }
            }
        }

        // Uninstall the signal handler after the event loop.
        // This must happen after all worker threads are joined to avoid stray SIGSTKFLT.
        if preemptive_at_start {
            crate::preempt::uninstall_signal_handler();
        }

        // Flush running tasks: emit SimulationEnd for any task still on-CPU.
        // The guard `s` may or may not be held depending on loop exit path.
        // Shadow `s` with a fresh lock to avoid self-deadlock.
        let mut s = sim_arc.lock().unwrap();
        for cpu_idx in 0..s.sim.cpus.len() {
            if let Some(pid) = s.sim.cpus[cpu_idx].current_task {
                s.sim.trace.record(
                    scenario.duration_ns,
                    CpuId(cpu_idx as u32),
                    TraceKind::SimulationEnd { pid },
                );
            }
        }

        // Call scheduler dump before exit (mirrors kernel dump on scheduler unload)
        // IMPORTANT: Sort PIDs for deterministic order. HashMap iteration
        // is non-deterministic, and scheduler callbacks (dump_task, exit_task)
        // charge RBC costs that accumulate on the CPU clock.
        let mut shutdown_pids: Vec<Pid> = s.tasks.keys().copied().collect();
        shutdown_pids.sort_by_key(|p| p.0);
        {
            let cpu = s.sim.current_cpu;
            start_rbc(&mut s.sim);
            sim_callback!(s, s, sim_arc, cpu, {
                self.scheduler.dump(OptionalPtr::null());
            });
            charge_sched_time(&mut s.sim, CpuId(0), "dump");

            // Snapshot task raw pointers for dump_task/exit_task.
            let task_raws: Vec<(Pid, *mut c_void)> = shutdown_pids
                .iter()
                .map(|&pid| (pid, s.tasks[&pid].raw()))
                .collect();

            for &(pid, raw) in &task_raws {
                start_rbc(&mut s.sim);
                sim_callback!(s, s, sim_arc, cpu, {
                    self.scheduler
                        .dump_task(OptionalPtr::null(), TaskPtr::new(raw));
                });
                charge_sched_time(&mut s.sim, CpuId(0), "dump_task");
                let _ = pid; // used for deterministic ordering
            }
        }

        // Call exit_task for each task (mirrors kernel scheduler unload)
        {
            let cpu = s.sim.current_cpu;
            let task_raws: Vec<(Pid, *mut c_void)> = shutdown_pids
                .iter()
                .map(|&pid| (pid, s.tasks[&pid].raw()))
                .collect();
            for &(pid, raw) in &task_raws {
                debug!(pid = pid.0, "enter:structop exit_task");
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    self.scheduler.exit_task(TaskPtr::new(raw));
                });
                // TOP-5 of secondary TraceKind easy-win bundle: emit
                // the per-task exit_task structop so the live-vs-sim
                // diff harness validates fixture-shutdown handshake
                // ordering matches.
                s.sim
                    .trace
                    .record(__local_t, cpu, TraceKind::ExitTask { pid });
                charge_sched_time(&mut s.sim, CpuId(0), "exit_task");
            }
        }

        // Phase 2 Stage E diagnostic probe (PRE-cgroup_exit). See full
        // rationale in the second probe block below; a copy is run here
        // because lavd_cgroup_exit -> cbw_del_cgroup_ctx clears the
        // library's cbw_cgrp_map, so any probe AFTER the loop sees
        // empty maps regardless of what the run produced.
        if std::env::var_os("SCXSIM_DEBUG_CBW_PROBE").is_some() {
            eprintln!("[SCXSIM-CBW-PROBE-PRE-EXIT] (before cgroup_exit cleanup)");
            let cgids: Vec<CgroupId> = s
                .cgroup_registry
                .all_cgids_preorder()
                .into_iter()
                .filter(|c| *c != CgroupId::ROOT)
                .collect();
            for cgid in cgids {
                let cpu = s.sim.current_cpu;
                let mut out = crate::ffi::CbwProbeResult::default();
                start_rbc(&mut s.sim);
                let mut probed = false;
                let mut rc_out: i32 = 0;
                sim_callback!(s, s, sim_arc, cpu, {
                    if let Some(rc) = self.scheduler.probe_cbw_state(cgid.0, 0, &mut out) {
                        probed = true;
                        rc_out = rc;
                    }
                });
                let s = &mut *s;
                charge_sched_time(&mut s.sim, CpuId(0), "probe_cbw_state");
                if probed {
                    eprintln!(
                        "[SCXSIM-CBW-PROBE-PRE-EXIT] cgid={} llc=0 rc={} cgrp_id_seen={} \
                         cgrp_ptr={:?} cgx={:?} llcx_helper={:?} llcx_direct={:?} \
                         has_llcx={} is_throttled={} runtime_total_sloppy={} \
                         runtime_total_in_llcx={} consume_count_pre={} \
                         cbw_cgrp_map_nr={} cbw_cgrp_map_first_key={:?} \
                         cbw_cgrp_llc_map_nr={}",
                        cgid.0,
                        rc_out,
                        out.cgrp_id_seen,
                        out.cgrp_ptr,
                        out.cgx,
                        out.llcx_via_helper,
                        out.llcx_via_direct_map,
                        out.has_llcx,
                        out.is_throttled,
                        out.runtime_total_sloppy,
                        out.runtime_total_in_llcx,
                        out.consumed_count_pre,
                        out.cbw_cgrp_map_nr,
                        out.cbw_cgrp_map_first_key,
                        out.cbw_cgrp_llc_map_nr,
                    );
                }
            }
        }

        // Call cgroup_exit for each cgroup (reverse order: children before root)
        {
            let cpu = s.sim.current_cpu;
            // Capture cgid alongside raw so we can emit the matching
            // `TraceKind::CgroupExit` (TOP-2 of cpu-bw-stall-bug
            // TraceKind easy-win bundle).
            let cg_exit_list: Vec<(CgroupId, *mut c_void)> = s
                .cgroup_registry
                .all_cgids_preorder()
                .into_iter()
                .rev()
                .filter_map(|cgid| s.cgroup_registry.get_raw(cgid).map(|raw| (cgid, raw)))
                .collect();
            for (cgid, raw) in cg_exit_list {
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, s, sim_arc, cpu, {
                    self.scheduler.cgroup_exit(TaskPtr::new(raw));
                });
                s.sim
                    .trace
                    .record(__local_t, cpu, TraceKind::CgroupExit { cgid });
                charge_sched_time(&mut s.sim, CpuId(0), "cgroup_exit");
            }
        }

        // Phase 2 Stage E diagnostic (tg
        // `investigate-scxsim-engine-throttles-before-scheduler-cgroup-bw`):
        // probe the cgroup_bw library state for every non-root cgroup
        // BEFORE calling scheduler.exit(). Reports both the library's
        // internal helper lookup and a direct map lookup for the same
        // key so the engine can tell whether the breakdown is in the
        // library wrapper, the map impl, or somewhere downstream.
        //
        // The probe is silently skipped when the loaded scheduler does
        // not export `scxsim_probe_cbw_state` (e.g. simple, tickless).
        // Output is opt-in: enable with SCXSIM_DEBUG_CBW_PROBE env var.
        //
        // The probe MUST run inside `sim_callback!` because the wrapper
        // calls `bpf_cgroup_from_id` -> `sim_cgroup_lookup_by_id` which
        // try_lock()s the SimArc. The outer engine code holds that
        // lock; only sim_callback!'s drop(guard) makes try_lock
        // succeed. Without the macro the probe gets the root cgroup
        // back instead of the requested one.
        //
        // NOTE: deliberately wired AFTER cgroup_exit too late was the
        // first cut -- maps get freed there. This block must remain
        // BEFORE the cgroup_exit loop above. (Earlier checkpoint:
        // probe at end-of-run reported nr=0 / cgx=NULL because
        // lavd_cgroup_exit -> cbw_del_cgroup_ctx had already cleared
        // the cbw_cgrp_map.)
        if std::env::var_os("SCXSIM_DEBUG_CBW_PROBE").is_some() {
            let cgids: Vec<CgroupId> = s
                .cgroup_registry
                .all_cgids_preorder()
                .into_iter()
                .filter(|c| *c != CgroupId::ROOT)
                .collect();
            for cgid in cgids {
                let cpu = s.sim.current_cpu;
                let mut out = crate::ffi::CbwProbeResult::default();
                start_rbc(&mut s.sim);
                let mut probed = false;
                let mut rc_out: i32 = 0;
                sim_callback!(s, s, sim_arc, cpu, {
                    if let Some(rc) = self.scheduler.probe_cbw_state(cgid.0, 0, &mut out) {
                        probed = true;
                        rc_out = rc;
                    }
                });
                let s = &mut *s;
                charge_sched_time(&mut s.sim, CpuId(0), "probe_cbw_state");
                if probed {
                    eprintln!(
                        "[SCXSIM-CBW-PROBE] cgid={} llc=0 rc={} cgrp_id_seen={} \
                         cgrp_ptr={:?} cgx={:?} llcx_helper={:?} llcx_direct={:?} \
                         has_llcx={} is_throttled={} runtime_total_sloppy={} \
                         runtime_total_in_llcx={} consume_count_pre={} \
                         cbw_cgrp_map_nr={} cbw_cgrp_map_first_key={:?} \
                         cbw_cgrp_llc_map_nr={}",
                        cgid.0,
                        rc_out,
                        out.cgrp_id_seen,
                        out.cgrp_ptr,
                        out.cgx,
                        out.llcx_via_helper,
                        out.llcx_via_direct_map,
                        out.has_llcx,
                        out.is_throttled,
                        out.runtime_total_sloppy,
                        out.runtime_total_in_llcx,
                        out.consumed_count_pre,
                        out.cbw_cgrp_map_nr,
                        out.cbw_cgrp_map_first_key,
                        out.cbw_cgrp_llc_map_nr,
                    );
                }
            }
        }

        // Call scheduler exit
        {
            let cpu = s.sim.current_cpu;
            start_rbc(&mut s.sim);
            sim_callback!(s, s, sim_arc, cpu, {
                self.scheduler.exit();
            });
            charge_sched_time(&mut s.sim, CpuId(0), "exit");
        }

        // (Cgroup registry is now part of SimState, no separate cleanup needed.)

        // The synthetic idle task is freed automatically when `idle_task_handle`
        // goes out of scope (RAII Drop). No manual unsafe free needed.
        drop(idle_task_handle);

        // Set the exit kind on the trace
        s.sim.trace.set_exit_kind(exit_kind);

        // Print end-of-simulation summary.
        print_simulation_summary(&s.sim.trace, &s.tasks, s.sim.clock);

        // Print structop summary (per-CPU ops callbacks, RBC, kfuncs).
        crate::preempt::print_structop_summary(&s.sim.structop_accum);

        // Print preemption stats (longest structop and interval RBC counts).
        print_preemption_stats(
            s.sim.longest_structop_rbc,
            s.sim.longest_rbc_interval,
            s.sim.e9_fns.is_some(),
        );

        // Clear ENGINE_SIM_ARC.
        kfuncs::clear_engine_sim_arc();

        // Drop the MutexGuard and extract the SimState from the Arc.
        drop(s);
        let sim_state = match Arc::try_unwrap(sim_arc) {
            Ok(mutex) => match mutex.into_inner() {
                Ok(state) => state,
                Err(poison) => poison.into_inner(),
            },
            Err(_) => panic!("Arc<Mutex<SimState>> still has multiple owners at end of simulation"),
        };

        SimulationResult {
            trace: sim_state.sim.trace,
            tasks: sim_state.tasks,
        }
    }

    /// Process a single event: advance clocks and dispatch to the handler.
    ///
    /// Returns `Some(ExitKind)` if the simulation should stop (watchdog stall,
    /// cgroup exhaustion). The caller checks BPF errors separately.
    #[allow(clippy::too_many_arguments)]
    #[allow(unused_assignments)]
    fn process_event(
        &self,
        event: Event,
        sim_arc: &SimArc,
        watchdog_timeout: Option<TimeNs>,
        duration_ns: TimeNs,
        max_cgroups: u32,
        monitor: &mut dyn Monitor,
    ) -> Option<ExitKind> {
        // Inhibit PMU preemption for the entire event processing.
        // The signal handler will skip yield_token() while this is set,
        // preventing deadlocks where the handler parks a worker that
        // holds the SIM_ARC mutex.  Preemption is briefly allowed
        // inside sim_callback! during scheduler C code execution.
        crate::preempt::inhibit_preemption();
        let result = self.process_event_inner(
            event,
            sim_arc,
            watchdog_timeout,
            duration_ns,
            max_cgroups,
            monitor,
        );
        crate::preempt::allow_preemption();
        result
    }

    #[allow(unused_assignments)]
    fn process_event_inner(
        &self,
        event: Event,
        sim_arc: &SimArc,
        watchdog_timeout: Option<TimeNs>,
        duration_ns: TimeNs,
        max_cgroups: u32,
        monitor: &mut dyn Monitor,
    ) -> Option<ExitKind> {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Advance per-CPU clock for CPU-specific events
        match &event.kind {
            EventKind::SliceExpired { cpu }
            | EventKind::TaskPhaseComplete { cpu }
            | EventKind::Tick { cpu }
            | EventKind::CpuOffline { cpu }
            | EventKind::CpuOnline { cpu }
            | EventKind::CpuRelease { cpu }
            | EventKind::CpuAcquire { cpu }
            | EventKind::IrqStart { cpu, .. }
            | EventKind::IrqEnd { cpu }
            | EventKind::TaskWake { cpu, .. }
            | EventKind::DsqConsume { cpu }
            | EventKind::StartRunning { cpu, .. }
            | EventKind::KickDelivered { cpu, .. }
            | EventKind::TimerFired { cpu, .. }
            | EventKind::CgroupMigrate { cpu, .. }
            | EventKind::CgroupCreate { cpu, .. }
            | EventKind::CgroupDestroy { cpu, .. } => {
                s.sim.advance_cpu_clock(*cpu);
                kfuncs::set_sim_clock(s.sim.cpus[cpu.0 as usize].local_clock, Some(*cpu));
            }
            EventKind::CgroupCpusetChange { cpu, .. } => {
                s.sim.advance_cpu_clock(*cpu);
                kfuncs::set_sim_clock(s.sim.cpus[cpu.0 as usize].local_clock, Some(*cpu));
            }
            // FutexOp derives its CPU from the running task at fire time, so no
            // pre-dispatch per-CPU clock advance is done here (the handler
            // advances the derived CPU's clock itself).
            EventKind::FutexOp { .. } => {}
        }

        match event.kind {
            EventKind::TaskWake { pid, waker, .. } => {
                drop(guard);
                self.handle_task_wake(pid, waker, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::SliceExpired { cpu } => {
                drop(guard);
                self.handle_slice_expired(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::TaskPhaseComplete { cpu } => {
                drop(guard);
                self.handle_task_phase_complete(cpu, sim_arc, duration_ns, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::TimerFired { cpu, slot } => {
                drop(guard);
                self.handle_timer_fired(cpu, slot, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::Tick { cpu } => {
                if let Some(timeout) = watchdog_timeout {
                    if let Some(stall_error) = Self::check_watchdog(&s.tasks, s.sim.clock, timeout)
                    {
                        return Some(stall_error);
                    }
                }
                drop(guard);
                self.handle_tick(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CpuOffline { cpu } => {
                drop(guard);
                self.handle_cpu_offline(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CpuOnline { cpu } => {
                drop(guard);
                self.handle_cpu_online(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CpuRelease { cpu } => {
                drop(guard);
                self.handle_cpu_release(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CpuAcquire { cpu } => {
                drop(guard);
                self.handle_cpu_acquire(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CgroupMigrate {
                pid,
                from_cgroup,
                to_cgroup,
                ..
            } => {
                drop(guard);
                self.handle_cgroup_migrate(pid, &from_cgroup, &to_cgroup, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CgroupCreate { event, .. } => {
                drop(guard);
                if let Some(err) = self.handle_cgroup_create(&event, sim_arc, max_cgroups) {
                    return Some(err);
                }
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CgroupDestroy { event, .. } => {
                drop(guard);
                self.handle_cgroup_destroy(&event, sim_arc);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::CgroupCpusetChange { event, .. } => {
                drop(guard);
                self.handle_cgroup_cpuset_change(&event, sim_arc);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::IrqStart {
                cpu,
                irq_type,
                duration_ns,
                wake_pids,
            } => {
                drop(guard);
                self.handle_irq_start(cpu, irq_type, duration_ns, &wake_pids, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::IrqEnd { cpu } => {
                drop(guard);
                self.handle_irq_end(cpu, sim_arc);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::FutexOp { pid, op } => {
                drop(guard);
                self.handle_futex_op(pid, op, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::DsqConsume { cpu } => {
                drop(guard);
                self.handle_dsq_consume(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::StartRunning { cpu, pid } => {
                drop(guard);
                self.handle_start_running_event(cpu, pid, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
            EventKind::KickDelivered { cpu, flags } => {
                drop(guard);
                self.handle_kick_delivered(cpu, flags, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
        }
        None
    }

    /// Handle a BPF timer firing on a specific CPU.
    ///
    /// In the kernel, BPF timer callbacks fire in softirq context on the
    /// CPU that armed the timer (with `BPF_F_TIMER_CPU_PIN`). The `cpu`
    /// parameter carries the CPU where `bpf_timer_start()` was called.
    ///
    /// Calls the scheduler's `fire_timer(slot)` callback, which invokes the
    /// stored BPF timer callback for the given slot (e.g., `wakeup_timerfn`
    /// in COSMOS at slot 0; `cbw_replenish_timerfn` at a Phase-2 slot).
    /// The callback may kick CPUs and re-arm any number of timers via
    /// `bpf_timer_start_slot`. Phase 1 BPF infra scale-up items 1+2 (tg
    /// `scxsim-bpf-infra-scale-up-phase1`).
    fn handle_timer_fired(
        &self,
        cpu: CpuId,
        slot: u8,
        sim_arc: &SimArc,
        _monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Advance the per-CPU clock so scx_bpf_now() inside the callback
        // returns a value consistent with (or later than) all CPU local clocks.
        s.sim.advance_cpu_clock(cpu);
        set_ops_context(&mut s.sim, OpsContext::FireTimer);
        // Populate the CSS iterator so bpf_for_each(css, ...) inside the
        // timer callback can discover all cgroups (e.g. mitosis
        // update_timer_cb configures cells from the cgroup tree).
        s.cgroup_registry.prepare_css_iter_from_root();

        // Source (cgid, raw cgrp ptr) pairs from cgroup_registry. The
        // raw ptr is what the lib's CGRP_STORAGE map is keyed by --
        // we hand it directly to `snapshot_by_raw_cgrp` so we don't
        // need `bpf_cgroup_from_id` at all (which would deadlock on
        // SIM_ARC try_lock at this snapshot point).
        // tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`.
        let cbw_pairs: Vec<(u64, *mut c_void)> = s
            .cgroup_registry
            .all_cgids_preorder()
            .into_iter()
            .filter_map(|cid| s.cgroup_registry.get_raw(cid).map(|raw| (cid.0, raw)))
            .collect();
        let cbw_before = crate::cgroup_bw_replenish::snapshot_via(&cbw_pairs, |cgid, raw, out| {
            self.scheduler.snapshot_by_raw_cgrp(cgid, raw, out)
        });
        let cbw_observer_active = cbw_before.is_some();
        let cbw_before = cbw_before.unwrap_or_default();

        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.fire_timer(slot);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "fire_timer");

        // Take the matching AFTER snapshot, diff against BEFORE, and
        // emit one event per cgroup the lib replenished. Cheap when
        // the loaded scheduler doesn't link cgroup_bw (the dlsym
        // symbol is absent so snapshot_via returns None on the first
        // call and we never even build the cgid list a second time).
        if cbw_observer_active {
            let cbw_after =
                crate::cgroup_bw_replenish::snapshot_via(&cbw_pairs, |cgid, raw, out| {
                    self.scheduler.snapshot_by_raw_cgrp(cgid, raw, out)
                })
                .unwrap_or_default();
            let now_ns = s.sim.cpus[cpu.0 as usize].local_clock;
            let events = crate::cgroup_bw_replenish::diff_snapshots(&cbw_before, &cbw_after);
            // For each cgroup that just replenished AND is no longer
            // throttled (`keep_throttled == false`), drain any tasks the
            // EAGER admission gate stashed in `bw_blocked[cgid]` and
            // schedule a TaskWake event for each — re-runnabling them
            // through the wakeup path (ops.runnable + ops.select_cpu +
            // ops.enqueue) so LAVD's `can_direct_dispatch` can fire
            // and (probably) take the simple-insert direct-dispatch fast
            // path. tg
            // `scxsim-eager-cgroup-bw-throttle-via-dequeue-wakeup-cycle`.
            for kind in events {
                let replenish_info = if let TraceKind::CgroupBwReplenish {
                    cgid,
                    keep_throttled,
                    ..
                } = &kind
                {
                    Some((*cgid, *keep_throttled))
                } else {
                    None
                };
                s.sim.trace.record(now_ns, cpu, kind);
                if let Some((cgid, keep_throttled)) = replenish_info {
                    if !keep_throttled {
                        // Drain bw_blocked[cgid] (FIFO).
                        if let Some(queue) = s.sim.bw_blocked.remove(&cgid) {
                            for blocked_pid in queue {
                                // Reset task state so handle_task_wake's
                                // "skip if already runnable" guard
                                // accepts the wake.
                                if let Some(task) = s.tasks.get_mut(&blocked_pid) {
                                    task.state = TaskState::Sleeping;
                                }
                                // Schedule a fresh wake at "now". cpu =
                                // the CPU on which fire_timer is
                                // running; the wake will pass this as
                                // prev_cpu fallback.
                                s.events.push(
                                    now_ns,
                                    EventKind::TaskWake {
                                        pid: blocked_pid,
                                        waker: None,
                                        cpu,
                                    },
                                );
                                s.sim.trace.record(
                                    now_ns,
                                    cpu,
                                    TraceKind::CgroupBwReenqueueOnReplenish {
                                        pid: blocked_pid,
                                        cgid,
                                    },
                                );
                                debug!(
                                    pid = blocked_pid.0,
                                    cgid = cgid.0,
                                    cpu = cpu.0,
                                    "cgroup_bw: eager-replenish wake (drained bw_blocked)"
                                );
                            }
                        }
                    }
                }
            }
        }

        // Drain ALL re-armed timer slots in ascending slot order. Each
        // slot's CPU is captured by `sim_timer_start_slot` inside the
        // callback (which may differ from `cpu` if the callback re-arms
        // a timer in a different CPU context, though typically it stays
        // on the same CPU). A timer re-arming itself overwrites its own
        // slot's pending entry -- matches kernel `bpf_timer_start`
        // semantics. Single-slot draining preserves byte-for-byte the
        // pre-Phase-1 single-timer behavior because slot 0 is the only
        // one ever populated for legacy schedulers.
        //
        // Two-step pattern (snapshot then push) sidesteps the
        // borrow-checker conflict between `s.sim.pending_timers` (the
        // source) and `s.events` (the sink) that share an ancestor
        // mut-borrow on `s`.
        let mut drained: [Option<(crate::types::TimeNs, CpuId)>; crate::kfuncs::MAX_BPF_TIMERS] =
            [None; crate::kfuncs::MAX_BPF_TIMERS];
        for (s_idx, entry) in s.sim.pending_timers.iter_mut().enumerate() {
            drained[s_idx] = entry.take();
        }
        for (s_idx, entry) in drained.iter().enumerate() {
            if let Some((fire_at, timer_cpu)) = *entry {
                s.events.push(
                    fire_at,
                    EventKind::TimerFired {
                        cpu: timer_cpu,
                        slot: s_idx as u8,
                    },
                );
            }
        }

        // Flush staged events (e.g. KickDelivered) from the timer callback
        {
            let s = &mut *guard;
            flush_staged_events(&mut s.sim, &mut s.events);
        }
    }

    /// Handle a periodic scheduler tick on a CPU.
    ///
    /// Ticks are per-CPU periodic timer interrupts, independent of which task
    /// is running. Each tick unconditionally schedules the next tick at
    /// `now + TICK_INTERVAL_NS`, maintaining a single perpetual chain per CPU.
    ///
    /// If a task is running, calls `ops.tick(p)` and detects self-preemption
    /// via two patterns:
    /// 1. Scheduler called `scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT)` on self
    /// 2. Scheduler zeroed `p->scx.slice` (slice changed to 0 during tick)
    fn handle_tick(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Don't schedule further ticks on offline CPUs
        if !s.sim.cpus[cpu.0 as usize].is_online {
            return;
        }

        // Always schedule the next tick — ticks are unconditional per-CPU timers
        let jitter = s.sim.tick_jitter();
        let interval = (TICK_INTERVAL_NS as i64 + jitter).max(1) as TimeNs;
        let next_tick = s.sim.cpus[cpu.0 as usize].local_clock + interval;
        s.events.push(next_tick, EventKind::Tick { cpu });

        let pid = match s.sim.cpus[cpu.0 as usize].current_task {
            Some(pid) => pid,
            None => return, // No task running — nothing to tick
        };

        let raw = match s.tasks.get(&pid) {
            Some(task) => task.raw(),
            None => return,
        };

        // Record tick in trace
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim.trace.record(__local_t, cpu, TraceKind::Tick { pid });

        // Sample all non-builtin DSQ lengths at tick time
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim.trace.sample_dsq_lengths(
            __local_t,
            &s.sim.dsqs,
            DsqSampleTrigger::Tick,
            None, // Sample all DSQs
        );

        // Save pre-tick slice to detect if scheduler zeroed it
        let pre_tick_slice = ffi::task_get_slice(raw);

        // Update sum_exec_runtime before tick (LAVD reads it via
        // task_exec_time in account_task_runtime).
        {
            let task = s.tasks.get(&pid).unwrap();
            let started_at = s.sim.cpus[cpu.0 as usize].task_started_at.unwrap_or(0);
            let elapsed = s.sim.cpus[cpu.0 as usize]
                .local_clock
                .saturating_sub(started_at);
            update_sum_exec(raw, task.sum_exec_base, elapsed);
        }

        set_ops_context(&mut s.sim, OpsContext::Tick);
        debug!(pid = pid.0, "enter:structop tick");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.tick(TaskPtr::new(raw));
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "tick");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::Tick, cpu);

        // Check for self-preemption: look in staged_events for a
        // KickDelivered targeting this CPU with PREEMPT.
        let self_kick_preempt = s.sim.staged_events.iter().any(|(_, ev)| match ev {
            StagedEvent::KickDelivered { cpu: c, flags } => {
                *c == cpu && flags.contains(KickFlags::PREEMPT)
            }
        });
        let post_tick_slice = ffi::task_get_slice(raw);
        let slice_zeroed = pre_tick_slice > 0 && post_tick_slice == 0;
        let should_preempt = self_kick_preempt || slice_zeroed;

        // Remove self-kicks from staged events before flushing others.
        // Self-kicks are handled inline via should_preempt above.
        s.sim.staged_events.retain(
            |(_, ev)| !matches!(ev, StagedEvent::KickDelivered { cpu: c, .. } if *c == cpu),
        );

        // Flush remaining staged events (kicks to other CPUs)
        flush_staged_events(&mut s.sim, &mut s.events);

        if should_preempt && s.sim.cpus[cpu.0 as usize].current_task.is_some() {
            drop(guard);
            self.preempt_current(cpu, sim_arc, monitor);
        }
    }

    /// Handle a CPU going offline (hotplug remove).
    ///
    /// Preempts any running task, drains the local DSQ, calls
    /// `ops.cpu_offline`, and marks the CPU offline so ticks and
    /// dispatch stop targeting it.
    fn handle_cpu_offline(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        if !s.sim.cpus[cpu.0 as usize].is_online {
            return; // Already offline
        }

        info!(cpu = cpu.0, "CPU OFFLINE");

        // Preempt running task (if any) — it will be re-enqueued
        let needs_preempt = s.sim.cpus[cpu.0 as usize].current_task.is_some();
        if needs_preempt {
            drop(guard);
            self.preempt_current(cpu, sim_arc, monitor);
            guard = sim_arc.lock().unwrap();
        }
        let s = &mut *guard;

        // Drain local DSQ: re-enqueue each task so the scheduler places it
        // elsewhere. In the kernel, migrate_disabled tasks would stay, but
        // we don't model that.
        let local_pids: Vec<Pid> = s.sim.cpus[cpu.0 as usize].local_dsq.drain(..).collect();
        for pid in local_pids {
            let s = &mut *guard;
            if let Some(task) = s.tasks.get(&pid) {
                let raw = task.raw();
                sim_callback!(s, guard, sim_arc, cpu, {
                    self.scheduler.enqueue(TaskPtr::new(raw), 0);
                });
                let s = &mut *guard;
                s.sim.resolve_pending_dispatch(cpu);
            }
        }

        // Notify the scheduler
        let s = &mut *guard;
        set_ops_context(&mut s.sim, OpsContext::CpuOffline);
        debug!(cpu = cpu.0, "enter:structop cpu_offline");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.cpu_offline(cpu.0 as i32);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "cpu_offline");

        s.sim.cpus[cpu.0 as usize].is_online = false;
    }

    /// Handle a CPU coming online (hotplug add).
    ///
    /// Marks the CPU online, calls `ops.cpu_online`, notifies idle state,
    /// restarts ticks, and tries to dispatch work to the CPU.
    fn handle_cpu_online(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        if s.sim.cpus[cpu.0 as usize].is_online {
            return; // Already online
        }

        info!(cpu = cpu.0, "CPU ONLINE");
        s.sim.cpus[cpu.0 as usize].is_online = true;

        // Notify the scheduler
        set_ops_context(&mut s.sim, OpsContext::CpuOnline);
        debug!(cpu = cpu.0, "enter:structop cpu_online");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.cpu_online(cpu.0 as i32);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "cpu_online");

        // CPU starts idle after coming online
        ffi::cpumask_set_idle(cpu.0 as i32);
        set_ops_context(&mut s.sim, OpsContext::UpdateIdle);
        start_rbc(&mut s.sim);
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.update_idle(cpu.0 as i32, true);
        });
        let s = &mut *guard;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::UpdateIdle { cpu, idle: true });
        charge_sched_time(&mut s.sim, cpu, "update_idle");

        // Restart tick chain for this CPU
        let next_tick = s.sim.cpus[cpu.0 as usize].local_clock + TICK_INTERVAL_NS;
        s.events.push(next_tick, EventKind::Tick { cpu });

        // Try to dispatch work to the newly online CPU
        drop(guard);
        self.try_dispatch_and_run(cpu, sim_arc, monitor);
    }

    /// Handle a higher-priority scheduler class taking a CPU (cpu_release).
    ///
    /// Preempts any running SCX task, calls `ops.cpu_release`, and stops
    /// ticks on the CPU until `cpu_acquire` fires.
    fn handle_cpu_release(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        if !s.sim.cpus[cpu.0 as usize].is_online {
            return;
        }

        info!(cpu = cpu.0, "CPU RELEASE (higher-priority class)");

        // Preempt running task (if any)
        let __has_task = s.sim.cpus[cpu.0 as usize].current_task.is_some();

        if __has_task {
            drop(guard);
            self.preempt_current(cpu, sim_arc, monitor);
            guard = sim_arc.lock().unwrap();
        }

        let s = &mut *guard;

        // Call cpu_release
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler
                .cpu_release(cpu.0 as i32, OptionalPtr::null());
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "cpu_release");

        // Mark CPU as temporarily unavailable (reuse is_online)
        s.sim.cpus[cpu.0 as usize].is_online = false;
    }

    /// Handle sched_ext regaining a CPU from a higher-priority class (cpu_acquire).
    ///
    /// Calls `ops.cpu_acquire`, marks the CPU available, and tries to dispatch.
    fn handle_cpu_acquire(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        info!(
            cpu = cpu.0,
            "CPU ACQUIRE (regained from higher-priority class)"
        );

        s.sim.cpus[cpu.0 as usize].is_online = true;

        // Call cpu_acquire
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler
                .cpu_acquire(cpu.0 as i32, OptionalPtr::null());
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "cpu_acquire");

        // CPU starts idle after being reacquired
        ffi::cpumask_set_idle(cpu.0 as i32);
        set_ops_context(&mut s.sim, OpsContext::UpdateIdle);
        start_rbc(&mut s.sim);
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.update_idle(cpu.0 as i32, true);
        });
        let s = &mut *guard;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::UpdateIdle { cpu, idle: true });
        charge_sched_time(&mut s.sim, cpu, "update_idle");

        // Restart tick chain
        let next_tick = s.sim.cpus[cpu.0 as usize].local_clock + TICK_INTERVAL_NS;
        s.events.push(next_tick, EventKind::Tick { cpu });

        // Try to dispatch work
        drop(guard);
        self.try_dispatch_and_run(cpu, sim_arc, monitor);
    }

    /// Handle a cgroup migration: move a task between cgroups.
    ///
    /// In the kernel, this is triggered by writing a PID to cgroup.procs.
    ///
    /// Mirror kernel sched_change guard: dequeue before cgroup_move,
    /// enqueue after (kernel/sched/core.c:9119-9138). The kernel's
    /// `sched_move_task` wraps `cgroup_move` in a dequeue/enqueue bracket:
    /// if the task is queued, it is dequeued first, then after the BPF
    /// callback updates cell/DSQ metadata, the task is re-enqueued so it
    /// lands in the correct DSQ for its new cgroup.
    #[allow(clippy::too_many_arguments)]
    fn handle_cgroup_migrate(
        &self,
        pid: Pid,
        from_name: &str,
        to_name: &str,
        sim_arc: &SimArc,
        _monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let task = match s.tasks.get(&pid) {
            Some(t) => t,
            None => return,
        };

        let (from_cgid, from_raw) = {
            let info = s
                .cgroup_registry
                .get_by_name(from_name)
                .unwrap_or_else(|| panic!("cgroup '{from_name}' not found for migration"));
            (info.cgid, info.raw())
        };
        let (to_cgid, to_raw) = {
            let info = s
                .cgroup_registry
                .get_by_name(to_name)
                .unwrap_or_else(|| panic!("cgroup '{to_name}' not found for migration"));
            (info.cgid, info.raw())
        };

        info!(
            pid = pid.0,
            from = from_name,
            to = to_name,
            "CGROUP MIGRATE"
        );

        let raw = task.raw();
        let was_queued = task.state == TaskState::Runnable;
        let cpu = s.sim.current_cpu;

        // --- sched_change_begin: dequeue if queued ---
        if was_queued {
            drop(guard);
            self.cgroup_migrate_dequeue(pid, raw, cpu, sim_arc);
            guard = sim_arc.lock().unwrap();
        }
        let s = &mut *guard;

        // Update the task's cgroup in C-side
        ffi::task_set_cgroup(raw, to_raw);

        // Call cgroup_move
        start_rbc(&mut s.sim);
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.cgroup_move(
                TaskPtr::new(raw),
                TaskPtr::new(from_raw),
                TaskPtr::new(to_raw),
            );
        });
        let s = &mut *guard;
        // TOP-6 of cpu-bw-stall-bug TraceKind easy-win bundle.
        s.sim.trace.record(
            __local_t,
            cpu,
            TraceKind::CgroupMove {
                pid,
                from_cgid,
                to_cgid,
            },
        );
        charge_sched_time(&mut s.sim, cpu, "cgroup_move");

        // --- sched_change_end: re-enqueue if was queued ---
        if was_queued {
            drop(guard);
            self.cgroup_migrate_enqueue(pid, raw, cpu, sim_arc);
            guard = sim_arc.lock().unwrap();
        }

        // Flush staged events from cgroup_move or re-enqueue callbacks
        {
            let s = &mut *guard;
            flush_staged_events(&mut s.sim, &mut s.events);
        }
    }

    /// Dequeue a task before cgroup migration (sched_change_begin).
    ///
    /// Removes the task from its current DSQ (global, per-cell, or local)
    /// and calls `ops.dequeue` if the task is still in the BPF scheduler's
    /// queue (OpsTaskState::Queued).
    fn cgroup_migrate_dequeue(&self, pid: Pid, raw: *mut c_void, cpu: CpuId, sim_arc: &SimArc) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Remove from global/per-cell DSQs
        s.sim.dsqs.remove_pid_from_all(pid);

        // Remove from any CPU's local DSQ
        for sim_cpu in &mut s.sim.cpus {
            if let Some(pos) = sim_cpu.local_dsq.iter().position(|&p| p == pid) {
                sim_cpu.local_dsq.remove(pos);
                break;
            }
        }

        // If still in BPF scheduler queue, call ops.dequeue (flags=0, not sleep)
        let ops_state = s.sim.task_ops_state.get(&pid).copied().unwrap_or_default();
        if ops_state == OpsTaskState::Queued {
            set_ops_context(&mut s.sim, OpsContext::Dequeue);
            debug!(pid = pid.0, "dequeue (cgroup_migrate)");
            start_rbc(&mut s.sim);
            s.sim.set_task_ops_state(pid, OpsTaskState::None);
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            sim_callback!(s, guard, sim_arc, cpu, {
                self.scheduler.dequeue(TaskPtr::new(raw), 0);
            });
            let s = &mut *guard;
            // TOP-4 (Dequeue) of cpu-bw-stall-bug TraceKind easy-win bundle.
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::Dequeue { pid, deq_flags: 0 });
            charge_sched_time(&mut s.sim, cpu, "dequeue");
        }
    }

    /// Re-enqueue a task after cgroup migration (sched_change_end).
    ///
    /// Calls `ops.enqueue` so the BPF scheduler dispatches the task to
    /// the correct DSQ based on its updated cgroup/cell metadata.
    fn cgroup_migrate_enqueue(&self, pid: Pid, raw: *mut c_void, cpu: CpuId, sim_arc: &SimArc) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        set_ops_context(&mut s.sim, OpsContext::Enqueue);
        s.sim.set_task_ops_state(pid, OpsTaskState::Queued);
        debug!(pid = pid.0, "enqueue (cgroup_migrate)");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.enqueue(TaskPtr::new(raw), 0);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "enqueue");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::Enqueue, cpu);
        s.sim.resolve_pending_dispatch(cpu);
    }

    /// Handle runtime cgroup creation.
    ///
    /// Creates a new cgroup in the registry and calls `cgroup_init`.
    /// If the `max_cgroups` limit would be exceeded, returns an error
    /// instead of creating the cgroup.
    fn handle_cgroup_create(
        &self,
        event: &CgroupCreateEvent,
        sim_arc: &SimArc,
        max_cgroups: u32,
    ) -> Option<ExitKind> {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Check resource limit before creating
        let current_count = s.cgroup_registry.len() as u32;
        if current_count >= max_cgroups {
            info!(
                name = %event.name,
                current = current_count,
                max = max_cgroups,
                "CGROUP EXHAUSTED (ENOMEM)"
            );
            return Some(ExitKind::ErrorCgroupExhausted {
                cgroup_name: event.name.clone(),
                active_count: current_count,
                max_cgroups,
            });
        }

        let parent_cgid = match &event.parent_name {
            Some(parent) => {
                s.cgroup_registry
                    .get_by_name(parent)
                    .unwrap_or_else(|| panic!("parent cgroup '{parent}' not found"))
                    .cgid
            }
            None => CgroupId::ROOT,
        };

        let cgid = s
            .cgroup_registry
            .create(&event.name, parent_cgid, event.cpuset.clone());

        info!(
            name = %event.name,
            cgid = cgid.0,
            count = s.cgroup_registry.len(),
            "CGROUP CREATE"
        );

        // Call cgroup_init (refresh CSS iterator so the callback can use
        // bpf_for_each(css, ...) with the newly created cgroup included).
        let cpu = s.sim.current_cpu;
        let raw = s.cgroup_registry.get_raw(cgid).unwrap();
        s.cgroup_registry.prepare_css_iter_from_root();
        start_rbc(&mut s.sim);
        let rc;
        // Phase 2: pass real default args (see comment at the equivalent
        // call site near engine.rs:1601) -- compiled-in cgroup_bw library
        // requires non-null args.
        let args_ptr = OptionalPtr::new(default_cgroup_init_args());
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, cpu, {
            rc = self.scheduler.cgroup_init(TaskPtr::new(raw), args_ptr);
        });
        let s = &mut *guard;
        // TOP-2 of cpu-bw-stall-bug TraceKind easy-win bundle.
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::CgroupInit { cgid, rc });
        charge_sched_time(&mut s.sim, cpu, "cgroup_init");

        if rc != 0 {
            info!(
                name = %event.name,
                rc,
                "CGROUP INIT FAILED"
            );
            // Scheduler returned an error - treat as exhaustion
            return Some(ExitKind::ErrorCgroupExhausted {
                cgroup_name: event.name.clone(),
                active_count: current_count,
                max_cgroups,
            });
        }

        None
    }

    /// Handle runtime cgroup destruction.
    ///
    /// Calls `cgroup_exit` and removes the cgroup from the registry.
    fn handle_cgroup_destroy(&self, event: &CgroupDestroyEvent, sim_arc: &SimArc) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Capture cgid BEFORE `destroy_by_name` consumes the registry entry;
        // we need it for the matching `TraceKind::CgroupExit` emit (TOP-2).
        let cgid = match s.cgroup_registry.get_by_name(&event.name) {
            Some(info) => info.cgid,
            None => {
                debug!(name = %event.name, "cgroup not found for destruction");
                return;
            }
        };
        let raw = match s.cgroup_registry.destroy_by_name(&event.name) {
            Some(r) => r,
            None => {
                // Should be unreachable given the get_by_name above, but
                // bail safely if the registry got mutated between calls.
                debug!(name = %event.name, "cgroup not found for destruction");
                return;
            }
        };

        info!(
            name = %event.name,
            remaining = s.cgroup_registry.len(),
            "CGROUP DESTROY"
        );

        // Call cgroup_exit
        let cpu = s.sim.current_cpu;
        start_rbc(&mut s.sim);
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.cgroup_exit(TaskPtr::new(raw));
        });
        let s = &mut *guard;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::CgroupExit { cgid });
        charge_sched_time(&mut s.sim, cpu, "cgroup_exit");
        // Free the C-side cgroup struct
        s.cgroup_registry.free_raw(raw);
    }

    /// Handle a runtime cgroup cpuset change.
    ///
    /// Updates the cgroup's cpuset in the registry and C-side struct,
    /// then calls `cgroup_init` to notify the scheduler of the change
    /// (mirroring what the kernel does when cpuset.cpus is modified).
    fn handle_cgroup_cpuset_change(&self, event: &CgroupCpusetChangeEvent, sim_arc: &SimArc) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        if !s
            .cgroup_registry
            .update_cpuset(&event.cgroup_name, event.new_cpuset.clone())
        {
            debug!(
                name = %event.cgroup_name,
                "cgroup not found for cpuset change"
            );
            return;
        }

        info!(
            name = %event.cgroup_name,
            new_cpuset = ?event.new_cpuset,
            "CGROUP CPUSET CHANGE"
        );

        // Call cgroup_init to notify the scheduler of the cpuset change
        if let Some(cgrp_info) = s.cgroup_registry.get_by_name(&event.cgroup_name) {
            let raw = cgrp_info.raw();
            let cgid = cgrp_info.cgid;
            let cpu = s.sim.current_cpu;
            s.cgroup_registry.prepare_css_iter_from_root();
            start_rbc(&mut s.sim);
            // Phase 2: pass real default args (see comment at the
            // equivalent call site near engine.rs:1601).
            let args_ptr = OptionalPtr::new(default_cgroup_init_args());
            #[allow(unused_assignments)]
            let mut rc = 0i32;
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            sim_callback!(s, guard, sim_arc, cpu, {
                rc = self.scheduler.cgroup_init(TaskPtr::new(raw), args_ptr);
            });
            let s = &mut *guard;
            // TOP-2 of cpu-bw-stall-bug TraceKind easy-win bundle.
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::CgroupInit { cgid, rc });
            charge_sched_time(&mut s.sim, cpu, "cgroup_init");
        }
    }

    /// Handle an interrupt starting on a CPU.
    ///
    /// Sets the IRQ context, records stolen time, processes wakeups inline
    /// (so `bpf_in_hardirq()` returns true during `select_cpu`), and
    /// schedules the `IrqEnd` event.
    #[allow(clippy::too_many_arguments)]
    fn handle_irq_start(
        &self,
        cpu: CpuId,
        irq_type: IrqType,
        duration_ns: TimeNs,
        wake_pids: &[Pid],
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let irq_context = match irq_type {
            IrqType::HardIrq => IrqContext::HardIrq,
            IrqType::SoftIrq => IrqContext::ServingSoftIrq,
        };

        // Set IRQ context on the CPU
        s.sim.cpus[cpu.0 as usize].irq_context = irq_context;

        let irq_label = match irq_type {
            IrqType::HardIrq => "hardirq",
            IrqType::SoftIrq => "softirq",
        };
        info!(
            cpu = cpu.0,
            duration_ns,
            wakes = wake_pids.len(),
            "IRQ START ({irq_label})"
        );

        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::IrqStart { cpu, irq_type });

        // Accumulate stolen time if a task is running
        if s.sim.cpus[cpu.0 as usize].current_task.is_some() {
            s.sim.cpus[cpu.0 as usize].irq_stolen_ns += duration_ns;
        }
        // Always accumulate cumulative IRQ time (for scx_clock_task)
        s.sim.cpus[cpu.0 as usize].irq_cumulative_ns += duration_ns;

        // Process wakeups inline — the IRQ context is active, so
        // bpf_in_hardirq()/bpf_in_serving_softirq() returns true during
        // select_cpu. This matches kernel behavior: wake_up_process()
        // called inside an IRQ handler runs synchronously with the
        // interrupt context still active.
        for &pid in wake_pids {
            drop(guard);
            self.handle_task_wake(pid, None, sim_arc, monitor);
            guard = sim_arc.lock().unwrap();
        }
        let s = &mut *guard;

        // Schedule IrqEnd
        let end_time = s.sim.cpus[cpu.0 as usize].local_clock + duration_ns;
        s.events.push(end_time, EventKind::IrqEnd { cpu });
    }

    /// Handle an interrupt completing on a CPU.
    fn handle_irq_end(&self, cpu: CpuId, sim_arc: &SimArc) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        s.sim.cpus[cpu.0 as usize].irq_context = IrqContext::None;

        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::IrqEnd { cpu });

        info!(cpu = cpu.0, "IRQ END");
    }

    /// Deliver a scheduled futex transition to the scheduler's real futex
    /// hooks (LAVD lock-holder boosting). Mirrors `handle_timer_fired`: it
    /// runs a non-struct_ops C entry (`<prefix>_futex_hook`, resolved as the
    /// `futex_op` op) inside `sim_callback!` with the running task's CPU
    /// context installed, so the boost is attributed to the correct task.
    ///
    /// Per the No-Stub rule, the boost decision runs entirely in the real
    /// `lock.bpf.c`; the engine only *delivers* the event, exactly as the
    /// kernel delivers a futex tracepoint. The returned task flags are read
    /// only for the `FutexBoost` trace observation.
    ///
    /// The CPU is derived from whichever CPU `pid` is running on. If `pid` is
    /// not currently the running task on any CPU, the hooks would attribute to
    /// the wrong task, so we log and skip (No-Silent-Failures) rather than
    /// mis-attribute the boost — use a workload where `pid` is on-CPU at the
    /// event time (see `ai_docs/FUTEX_SIM_DESIGN.md`).
    fn handle_futex_op(&self, pid: Pid, op: FutexOp, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let _ = monitor;
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;

        // Derive the CPU `pid` is running on.
        let cpu = match (0..s.sim.cpus.len())
            .map(|i| CpuId(i as u32))
            .find(|c| s.sim.cpus[c.0 as usize].current_task == Some(pid))
        {
            Some(c) => c,
            None => {
                warn!(
                    pid = pid.0,
                    "futex op for a task not currently running on any CPU; skipped"
                );
                return;
            }
        };

        s.sim.advance_cpu_clock(cpu);
        set_ops_context(&mut s.sim, OpsContext::None);
        start_rbc(&mut s.sim);

        let (op_i32, ret_i64) = op.to_op_ret();
        // Assigned exactly once inside the callback below (sim_callback! always
        // runs its block); read afterwards for the FutexBoost observation.
        let flags: i64;
        sim_callback!(s, guard, sim_arc, cpu, {
            flags = self.scheduler.futex_op(op_i32, ret_i64);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "futex_op");

        // LAVD_FLAG_FUTEX_BOOST == 0x1 (lavd.bpf.h). flags == -1 means the
        // task had no scheduler task_ctx (boost not applicable).
        let boosted = flags >= 0 && (flags & 0x1) != 0;
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::FutexBoost { pid, op, boosted });
        info!(pid = pid.0, ?op, boosted, "FUTEX OP");
    }

    /// Handle a task waking up.
    ///
    /// If `waker` is provided (from a `Phase::Wake`), the waker's context is
    /// set so that `bpf_get_current_task_btf()` and `bpf_get_smp_processor_id()`
    /// return the waker's state during `select_cpu` (kernel semantics).
    fn handle_task_wake(
        &self,
        pid: Pid,
        waker: Option<WakerInfo>,
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        {
            let s = &mut *guard;
            let task = match s.tasks.get_mut(&pid) {
                Some(t) => t,
                None => return,
            };

            // Skip if task is already runnable or running
            if matches!(task.state, TaskState::Runnable | TaskState::Running { .. }) {
                return;
            }

            if matches!(task.state, TaskState::Exited) {
                return;
            }

            task.state = TaskState::Runnable;
            // Track when task became runnable for watchdog stall detection.
            // Only set if not already set (kernel semantics: only reset when task runs).
            if task.runnable_at_ns.is_none() {
                task.runnable_at_ns = Some(s.sim.clock);
            }
        }

        // Make sure the current phase is a Run phase
        // (skip over Wake phases, handle Sleep->Run transitions)
        {
            let s = &mut *guard;
            let task = s.tasks.get_mut(&pid).unwrap();
            self.advance_to_run_phase(task, &mut s.sim, &mut s.events);
        }

        let s = &mut *guard;
        let task = s.tasks.get(&pid).unwrap();
        if task.state == TaskState::Exited {
            // Task completed all its phases; record the completion event.
            let wake_cpu = waker.as_ref().map_or(task.prev_cpu, |w| w.cpu);
            s.sim
                .trace
                .record(s.sim.clock, wake_cpu, TraceKind::TaskCompleted { pid });
            tracing::info!(
                task = task.name.as_str(),
                pid = pid.0,
                "COMPLETED (from wake)"
            );
            return;
        }
        if task.state == TaskState::Sleeping {
            return;
        }

        // Extract values from task before dropping the borrow.
        let prev_cpu = task.prev_cpu;
        let raw = task.raw();

        // Set waker context: in the kernel, runnable/select_cpu/enqueue all
        // run in the waker's context, so bpf_get_current_task_btf() and
        // bpf_get_smp_processor_id() return the waker's state.
        let waker_raw = waker
            .as_ref()
            .and_then(|w| s.sim.task_pid_to_raw.get(&w.pid).copied());

        // CPU where the wakeup originates (waker's CPU or prev_cpu as fallback)
        let wake_cpu = waker.as_ref().map_or(prev_cpu, |w| w.cpu);

        s.sim
            .trace
            .record(s.sim.clock, wake_cpu, TraceKind::TaskWoke { pid });

        // Track wakeup time for wakeup latency floor enforcement.
        // Set here (before select_cpu/enqueue) to capture the full kernel
        // wake→schedule path, including direct dispatch via select_cpu.
        s.tasks.get_mut(&pid).unwrap().enqueued_at_ns = Some(s.sim.clock);

        // Call runnable callback
        // Sync wakeup: kernel sets WF_SYNC when the waker explicitly
        // requests it (e.g. pipe write, futex unlock). In the simulator,
        // a waker-driven wake (Phase::Wake) models this pattern.
        let enq_flags = if waker_raw.is_some() {
            SCX_ENQ_WAKEUP | SCX_WAKE_SYNC
        } else {
            SCX_ENQ_WAKEUP
        };
        // wake_flags for ops.select_cpu() are a DISTINCT namespace from the
        // SCX_ENQ_* flags passed to runnable/enqueue: in the kernel every
        // wakeup routed through try_to_wake_up() carries SCX_WAKE_TTWU, plus
        // SCX_WAKE_SYNC for synchronous (waker-yielding) wakeups. Build them
        // separately so wakeup-gated scheduler logic runs (cosmos is_wakeup()
        // hybrid-core migration), while leaving the traced enq_flags — which
        // enqueue_flags.rs pins — untouched. See mb sim-e10316.
        let wake_flags = if waker_raw.is_some() {
            SCX_WAKE_TTWU | SCX_WAKE_SYNC
        } else {
            SCX_WAKE_TTWU
        };
        set_ops_context(&mut s.sim, OpsContext::Runnable);
        s.sim.waker_task_raw = waker_raw;
        debug!(pid = pid.0, "enter:structop runnable");
        start_rbc(&mut s.sim);
        let __local_t = s.sim.cpus[wake_cpu.0 as usize].local_clock;
        sim_callback!(s, guard, sim_arc, wake_cpu, {
            self.scheduler.runnable(TaskPtr::new(raw), enq_flags);
        });
        let s = &mut *guard;
        // TOP-4 (Runnable) of cpu-bw-stall-bug TraceKind easy-win bundle.
        s.sim
            .trace
            .record(__local_t, wake_cpu, TraceKind::Runnable { pid, enq_flags });
        charge_sched_time(&mut s.sim, wake_cpu, "runnable");

        // Call select_cpu
        // Set ops_state to Queued before select_cpu — kernel sets QUEUED in
        // do_enqueue_task before either select_cpu or enqueue.
        s.sim.set_task_ops_state(pid, OpsTaskState::Queued);

        // select_cpu: release lock, call C, reacquire
        s.sim.pending_dispatches.clear();
        set_ops_context(&mut s.sim, OpsContext::SelectCpu);
        s.sim.waker_task_raw = waker_raw;
        start_rbc(&mut s.sim);
        let selected_cpu_raw;
        sim_callback!(s, guard, sim_arc, wake_cpu, {
            selected_cpu_raw =
                self.scheduler
                    .select_cpu(TaskPtr::new(raw), prev_cpu.0 as i32, wake_flags);
        });
        let s = &mut *guard;

        // Kernel clamping: if select_cpu returns >= nr_cpu_ids, the
        // kernel falls back to prev_cpu (select_task_rq_scx semantics).
        let nr_cpus = s.sim.cpus.len() as u32;
        let selected_cpu = if (selected_cpu_raw as u32) >= nr_cpus {
            debug!(
                pid = pid.0,
                returned = selected_cpu_raw,
                nr_cpus,
                prev_cpu = prev_cpu.0,
                "select_cpu returned out-of-range CPU, falling back to prev_cpu"
            );
            prev_cpu
        } else {
            CpuId(selected_cpu_raw as u32)
        };
        charge_sched_time(&mut s.sim, selected_cpu, "select_cpu");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::SelectCpu, selected_cpu);
        s.sim.waker_task_raw = None;
        s.sim.current_cpu = selected_cpu;
        // Update task_last_cpu after select_cpu (kernel sets task_cpu
        // in set_task_cpu after select_task_rq, before enqueue).
        s.sim.task_last_cpu.insert(pid, selected_cpu);
        kfuncs::set_sim_clock(
            s.sim.cpus[selected_cpu.0 as usize].local_clock,
            Some(selected_cpu),
        );
        debug!(
            pid = pid.0,
            prev_cpu = prev_cpu.0,
            selected_cpu = selected_cpu.0,
            "enter:structop select_cpu"
        );

        // Resolve deferred dispatch: SCX_DSQ_LOCAL -> selected_cpu
        // (kernel semantics: LOCAL resolves to the CPU select_cpu returned)
        let direct_dispatched = s.sim.resolve_pending_dispatch(selected_cpu);

        s.sim.trace.record(
            s.sim.clock,
            wake_cpu,
            TraceKind::SelectTaskRq {
                pid,
                prev_cpu,
                selected_cpu,
            },
        );

        let task = s.tasks.get_mut(&pid).unwrap();
        task.prev_cpu = selected_cpu;

        if let Some(dd_cpu) = direct_dispatched {
            // Task was directly dispatched — skip enqueue (kernel semantics)
            s.sim.current_cpu = dd_cpu;
            kfuncs::set_sim_clock(s.sim.cpus[dd_cpu.0 as usize].local_clock, Some(dd_cpu));
            debug!(pid = pid.0, target_cpu = dd_cpu.0, "direct dispatch");
            drop(guard);
            self.try_dispatch_and_run(dd_cpu, sim_arc, monitor);
        } else {
            // Task was not directly dispatched; call enqueue
            debug!(pid = pid.0, enq_flags, "enter:structop enqueue");
            sim_callback!(s, guard, sim_arc, selected_cpu, {
                self.scheduler.enqueue(TaskPtr::new(raw), enq_flags);
            });
            let s = &mut *guard;
            s.sim.resolve_pending_dispatch(selected_cpu);

            s.sim.trace.record(
                s.sim.clock,
                selected_cpu,
                TraceKind::EnqueueTask {
                    pid,
                    enq_flags: SCX_ENQ_WAKEUP,
                },
            );

            // Try to dispatch on idle CPUs
            let idle_cpus: Vec<CpuId> = s
                .sim
                .cpus
                .iter()
                .filter(|c| c.is_idle())
                .map(|c| c.id)
                .collect();

            // Dispatch each idle CPU sequentially. Interleaving happens
            // naturally via the engine's pop-one-at-a-time event loop —
            // no concurrent dispatch batching needed.
            for cpu in idle_cpus {
                drop(guard);
                self.try_dispatch_and_run(cpu, sim_arc, monitor);
                guard = sim_arc.lock().unwrap();
            }
        }
    }
    fn handle_slice_expired(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // If IRQ time was stolen, re-schedule the event later.
        let stolen = s.sim.cpus[cpu.0 as usize].irq_stolen_ns;
        if stolen > 0 {
            s.sim.cpus[cpu.0 as usize].irq_stolen_ns = 0;
            let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            s.events
                .push(local_t + stolen, EventKind::SliceExpired { cpu });
            return;
        }

        let pid = match s.sim.cpus[cpu.0 as usize].current_task {
            Some(pid) => pid,
            None => return,
        };

        let task = match s.tasks.get_mut(&pid) {
            Some(t) => t,
            None => return,
        };

        // Deduct the slice from remaining work
        let slice = task.get_slice();
        task.run_remaining_ns = task.run_remaining_ns.saturating_sub(slice);

        info!(
            task = task.name.as_str(),
            pid = pid.0,
            ran_ns = %FmtN(slice),
            "PREEMPTED"
        );

        // Stop the task - entire slice was consumed
        let raw = task.raw();
        task.state = TaskState::Runnable;

        // Drop guard before calling stop_and_reenqueue (which locks internally)
        drop(guard);

        // Shared stop -> re-enqueue -> dispatch spine. The PutPrevTask and
        // EnqueueTask trace records are specific to the slice-expired path
        // so we emit them via the `extra_traces` callback.
        self.stop_and_reenqueue(
            cpu,
            pid,
            raw,
            slice,
            0, // remaining_slice = 0: full slice consumed
            sim_arc,
            monitor,
            |st, cp, p| {
                // Trace records between stopping and enqueue
                st.trace.record(
                    st.cpus[cp.0 as usize].local_clock,
                    cp,
                    TraceKind::PutPrevTask {
                        pid: p,
                        still_runnable: true,
                    },
                );
            },
            |st, cp, p| {
                // Trace records after enqueue
                st.trace.record(
                    st.cpus[cp.0 as usize].local_clock,
                    cp,
                    TraceKind::EnqueueTask {
                        pid: p,
                        enq_flags: 0,
                    },
                );
                // High-level event: task is now fully off-CPU and re-enqueued
                st.trace.record(
                    st.cpus[cp.0 as usize].local_clock,
                    cp,
                    TraceKind::TaskPreempted { pid: p },
                );
            },
        );
    }

    /// Handle a task completing its current Run phase.
    fn handle_task_phase_complete(
        &self,
        cpu: CpuId,
        sim_arc: &SimArc,
        duration_ns: TimeNs,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // If IRQ time was stolen, re-schedule the event later.
        let stolen = s.sim.cpus[cpu.0 as usize].irq_stolen_ns;
        if stolen > 0 {
            s.sim.cpus[cpu.0 as usize].irq_stolen_ns = 0;
            let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            s.events
                .push(local_t + stolen, EventKind::TaskPhaseComplete { cpu });
            return;
        }

        let pid = match s.sim.cpus[cpu.0 as usize].current_task {
            Some(pid) => pid,
            None => return,
        };

        let task = match s.tasks.get_mut(&pid) {
            Some(t) => t,
            None => return,
        };

        let raw = task.raw();
        let task_name = task.name.clone(); // Clone for use after sim_callback!

        // Save the time consumed before advance_phase resets run_remaining_ns
        let time_consumed = task.run_remaining_ns;
        let original_slice = task.get_slice();

        // Advance to the next phase
        let has_next = task.advance_phase();
        let next_phase = task.current_phase().cloned();

        // Stop the running task
        let still_runnable = has_next && matches!(next_phase, Some(Phase::Run(_)));

        // Determine stop reason: Run→Run is a voluntary yield, Sleep/Wake/Complete are voluntary
        let stop_reason = LastStopReason::Voluntary;

        s.sim.cpus[cpu.0 as usize].current_task = None;
        s.sim.cpus[cpu.0 as usize].prev_task = Some(pid);
        s.sim.cpus[cpu.0 as usize].task_started_at = None;
        s.sim.cpus[cpu.0 as usize].task_original_slice = None;

        // Apply CSW overhead directly to local_clock (see #NOTE TIMING_MODEL)
        let overhead = s.sim.csw_overhead(stop_reason);
        s.sim.cpus[cpu.0 as usize].local_clock += overhead;
        kfuncs::clock_window_check(cpu, s.sim.cpus[cpu.0 as usize].local_clock);

        // Set slice to reflect consumed time (used by stopping() for vtime)
        let remaining_slice = original_slice.saturating_sub(time_consumed);
        crate::ffi::task_set_slice(raw, remaining_slice);

        // Update sum_exec_runtime: task consumed time_consumed ns on-CPU
        {
            let task = s.tasks.get(&pid).unwrap();
            update_sum_exec(raw, task.sum_exec_base, time_consumed);
        }

        // Charge consumed CPU time against this task's cgroup cpu.max quota
        // (Diff 3 wiring). See `charge_cgroup_bw` for layering rationale.
        {
            let now_ns = s.sim.cpus[cpu.0 as usize].local_clock;
            let mut f = s.fields();
            charge_cgroup_bw(&mut f, pid, time_consumed, now_ns, cpu);
        }

        set_ops_context(&mut s.sim, OpsContext::Stopping);
        debug!(pid = pid.0, still_runnable, "enter:structop stopping");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.stopping(TaskPtr::new(raw), still_runnable);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "stopping");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::Stopping, cpu);

        // Monitor: Stopping probe
        monitor.sample(&ProbeContext {
            point: ProbePoint::Stopping,
            pid,
            cpu,
            time_ns: s.sim.cpus[cpu.0 as usize].local_clock,
            task_raw: raw,
            trace: &s.sim.trace,
        });
        let s = &mut *guard;

        if !still_runnable {
            // Kernel clears SCX_TASK_QUEUED when a task goes to sleep.
            s.sim.clear_task_queued(pid);
            let ops_state = s.sim.task_ops_state.get(&pid).copied().unwrap_or_default();
            if ops_state == OpsTaskState::Queued {
                set_ops_context(&mut s.sim, OpsContext::Dequeue);
                debug!(pid = pid.0, "enter:structop dequeue");
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, guard, sim_arc, cpu, {
                    self.scheduler.dequeue(TaskPtr::new(raw), SCX_DEQ_SLEEP);
                });
                let s = &mut *guard;
                // TOP-4 (Dequeue) of cpu-bw-stall-bug TraceKind easy-win bundle.
                s.sim.trace.record(
                    __local_t,
                    cpu,
                    TraceKind::Dequeue {
                        pid,
                        deq_flags: SCX_DEQ_SLEEP,
                    },
                );
                charge_sched_time(&mut s.sim, cpu, "dequeue");
                s.sim.set_task_ops_state(pid, OpsTaskState::None);
            }
            // Rebind after potential sim_callback! in the if block
            let s = &mut *guard;
            set_ops_context(&mut s.sim, OpsContext::Quiescent);
            debug!(pid = pid.0, "enter:structop quiescent");
            start_rbc(&mut s.sim);
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            sim_callback!(s, guard, sim_arc, cpu, {
                self.scheduler.quiescent(TaskPtr::new(raw), SCX_DEQ_SLEEP);
            });
            let s = &mut *guard;
            // TOP-4 (Quiescent) of cpu-bw-stall-bug TraceKind easy-win bundle.
            s.sim.trace.record(
                __local_t,
                cpu,
                TraceKind::Quiescent {
                    pid,
                    deq_flags: SCX_DEQ_SLEEP,
                },
            );
            charge_sched_time(&mut s.sim, cpu, "quiescent");

            // Monitor: Quiescent probe
            monitor.sample(&ProbeContext {
                point: ProbePoint::Quiescent,
                pid,
                cpu,
                time_ns: s.sim.cpus[cpu.0 as usize].local_clock,
                task_raw: raw,
                trace: &s.sim.trace,
            });
        }

        let s = &mut *guard;
        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim.trace.record(
            __local_t,
            cpu,
            TraceKind::PutPrevTask {
                pid,
                still_runnable,
            },
        );

        if !has_next {
            // Task has completed all phases
            let task = s.tasks.get_mut(&pid).unwrap();
            task.state = TaskState::Exited;
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::TaskCompleted { pid });
            info!(task = task_name.as_str(), pid = pid.0, "COMPLETED");
        } else {
            match next_phase {
                Some(Phase::Sleep(sleep_ns)) => {
                    let task = s.tasks.get_mut(&pid).unwrap();
                    task.state = TaskState::Sleeping;
                    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                    s.sim
                        .trace
                        .record(local_t, cpu, TraceKind::TaskSlept { pid });
                    info!(task = task_name.as_str(), pid = pid.0, "SLEEPING");

                    // Schedule wake event on the CPU the task last ran on
                    let wake_time = local_t.saturating_add(sleep_ns);
                    if wake_time <= duration_ns {
                        s.events.push(
                            wake_time,
                            EventKind::TaskWake {
                                pid,
                                waker: None,
                                cpu: task.prev_cpu,
                            },
                        );
                    }
                }
                Some(Phase::Run(_)) => {
                    // Task goes directly to the next Run phase (still runnable)
                    let task = s.tasks.get_mut(&pid).unwrap();
                    task.state = TaskState::Runnable;

                    // Re-enqueue (part of put_prev_task for runnable tasks)
                    let raw = task.raw();
                    // Set ops state BEFORE start_rbc — set_task_ops_state
                    // does a HashMap::get() which has non-deterministic
                    // branch count due to random hash seeds.
                    s.sim.set_task_ops_state(pid, OpsTaskState::Queued);
                    debug!(pid = pid.0, "enqueue (yield re-enqueue)");
                    sim_callback!(s, guard, sim_arc, cpu, {
                        self.scheduler.enqueue(TaskPtr::new(raw), 0);
                    });
                    let s = &mut *guard;
                    s.sim.resolve_pending_dispatch(cpu);

                    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                    s.sim.trace.record(
                        __local_t,
                        cpu,
                        TraceKind::EnqueueTask { pid, enq_flags: 0 },
                    );

                    // High-level event: task is now fully off-CPU and re-enqueued
                    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                    s.sim
                        .trace
                        .record(__local_t, cpu, TraceKind::TaskYielded { pid });
                    info!(task = task_name.as_str(), pid = pid.0, "YIELDED");
                }
                Some(Phase::Wake(target_pid)) => {
                    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;

                    // Queue the wake for the target task (with waker context)
                    s.events.push(
                        local_t,
                        EventKind::TaskWake {
                            pid: target_pid,
                            waker: Some(WakerInfo { pid, cpu }),
                            cpu,
                        },
                    );

                    // Phase::Wake is instantaneous — advance to the next phase
                    // and handle it inline. The waker does NOT sleep during a wake.
                    let task = s.tasks.get_mut(&pid).unwrap();
                    if !task.advance_phase() {
                        task.state = TaskState::Exited;
                        s.sim
                            .trace
                            .record(local_t, cpu, TraceKind::TaskCompleted { pid });
                    } else {
                        // Process chained Wake phases (e.g. wake A, wake B, run)
                        loop {
                            match task.current_phase() {
                                Some(Phase::Wake(next_target)) => {
                                    let next_target = *next_target;
                                    s.events.push(
                                        local_t,
                                        EventKind::TaskWake {
                                            pid: next_target,
                                            waker: Some(WakerInfo { pid, cpu }),
                                            cpu,
                                        },
                                    );
                                    if !task.advance_phase() {
                                        task.state = TaskState::Exited;
                                        s.sim.trace.record(
                                            local_t,
                                            cpu,
                                            TraceKind::TaskCompleted { pid },
                                        );
                                        break;
                                    }
                                }
                                Some(Phase::Run(_)) => {
                                    // Still runnable — re-enqueue (same as yield)
                                    task.state = TaskState::Runnable;
                                    let raw = task.raw();
                                    // Set ops state BEFORE start_rbc — HashMap::get()
                                    // in set_scx_flag has non-deterministic branch count.
                                    s.sim.set_task_ops_state(pid, OpsTaskState::Queued);
                                    sim_callback!(s, guard, sim_arc, cpu, {
                                        self.scheduler.enqueue(TaskPtr::new(raw), 0);
                                    });
                                    let s = &mut *guard;
                                    s.sim.resolve_pending_dispatch(cpu);
                                    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                                    s.sim.trace.record(
                                        __local_t,
                                        cpu,
                                        TraceKind::EnqueueTask { pid, enq_flags: 0 },
                                    );
                                    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                                    s.sim.trace.record(
                                        __local_t,
                                        cpu,
                                        TraceKind::TaskYielded { pid },
                                    );
                                    info!(
                                        task = task_name.as_str(),
                                        pid = pid.0,
                                        "YIELDED (after wake)"
                                    );
                                    break;
                                }
                                Some(Phase::Sleep(ns)) => {
                                    let ns = *ns;
                                    task.state = TaskState::Sleeping;
                                    s.sim
                                        .trace
                                        .record(local_t, cpu, TraceKind::TaskSlept { pid });
                                    info!(
                                        task = task_name.as_str(),
                                        pid = pid.0,
                                        "SLEEPING (after wake)"
                                    );
                                    let wake_time = local_t.saturating_add(ns);
                                    if wake_time <= duration_ns {
                                        s.events.push(
                                            wake_time,
                                            EventKind::TaskWake {
                                                pid,
                                                waker: None,
                                                cpu: task.prev_cpu,
                                            },
                                        );
                                    }
                                    break;
                                }
                                None => {
                                    task.state = TaskState::Exited;
                                    s.sim.trace.record(
                                        local_t,
                                        cpu,
                                        TraceKind::TaskCompleted { pid },
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                None => {
                    let task = s.tasks.get_mut(&pid).unwrap();
                    task.state = TaskState::Exited;
                    let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                    s.sim
                        .trace
                        .record(__local_t, cpu, TraceKind::TaskCompleted { pid });
                }
            }
        }

        // Flush staged events from enqueue callbacks
        {
            let s = &mut *guard;
            flush_staged_events(&mut s.sim, &mut s.events);
        }

        // Dispatch next task on this CPU
        drop(guard);
        self.try_dispatch_and_run(cpu, sim_arc, monitor);
    }

    /// Try to dispatch and run a task on the given CPU.
    fn try_dispatch_and_run(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // If CPU is already running something, nothing to do
        if s.sim.cpus[cpu.0 as usize].current_task.is_some() {
            return;
        }

        // Don't dispatch to offline CPUs
        if !s.sim.cpus[cpu.0 as usize].is_online {
            return;
        }

        // Advance this CPU's clock to at least the event queue time
        s.sim.advance_cpu_clock(cpu);

        // Check if local DSQ has tasks
        if s.sim.cpus[cpu.0 as usize].local_dsq.is_empty() {
            // Look up the previously-running task's raw pointer for dispatch
            let prev_pid = s.sim.cpus[cpu.0 as usize].prev_task;
            let prev_raw = prev_pid
                .and_then(|pid| s.sim.task_pid_to_raw.get(&pid).copied())
                .map_or(std::ptr::null_mut(), |raw| raw as *mut c_void);

            // Call scheduler dispatch to try to fill the local DSQ
            set_ops_context(&mut s.sim, OpsContext::Dispatch);
            debug!("enter:structop dispatch");
            start_rbc(&mut s.sim);
            sim_callback!(s, guard, sim_arc, cpu, {
                self.scheduler
                    .dispatch(cpu.0 as i32, OptionalPtr::new(prev_raw));
            });
            let s = &mut *guard;
            charge_sched_time(&mut s.sim, cpu, "dispatch");
            maybe_record_checkpoint(&s.sim, CheckpointEvent::Dispatch, cpu);
            // Flush any deferred dispatch from dispatch() callback
            // (SCX_DSQ_LOCAL resolves to the dispatching CPU)
            s.sim.resolve_pending_dispatch(cpu);

            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::Balance { prev_pid });

            // Monitor: Dispatched probe (after ops.dispatch() completed)
            if let Some(ppid) = prev_pid {
                if let Some(task) = s.tasks.get(&ppid) {
                    monitor.sample(&ProbeContext {
                        point: ProbePoint::Dispatched,
                        pid: ppid,
                        cpu,
                        time_ns: s.sim.cpus[cpu.0 as usize].local_clock,
                        task_raw: task.raw(),
                        trace: &s.sim.trace,
                    });
                }
            }

            // Flush staged events from dispatch callback
            {
                let s = &mut *guard;
                flush_staged_events(&mut s.sim, &mut s.events);
            }
        }

        // Kernel fallback: if local DSQ is still empty after dispatch(),
        // automatically consume from the global DSQ (SCX_DSQ_GLOBAL).
        // This matches pick_next_task_scx() which tries the global DSQ
        // before going idle.
        drop(guard);
        self.post_dispatch_run(cpu, true, sim_arc, monitor);
    }

    /// Post-dispatch: try global DSQ fallback, then start running or go idle.
    ///
    /// Called after dispatch() has had a chance to fill the local DSQ.
    /// When `notify_idle` is true, calls `ops.update_idle(true)` if the CPU
    /// goes idle (sequential path). The concurrent path skips this because
    /// `update_idle` is not safe to call from the concurrent phase.
    fn post_dispatch_run(
        &self,
        cpu: CpuId,
        notify_idle: bool,
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // Global DSQ fallback
        if s.sim.cpus[cpu.0 as usize].local_dsq.is_empty() {
            let consumed = s.sim.consume_dsq_to_local(DsqId::GLOBAL, cpu);
            if consumed {
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                s.sim.trace.record(
                    __local_t,
                    cpu,
                    TraceKind::DsqMoveToLocal {
                        dsq_id: DsqId::GLOBAL,
                        success: true,
                    },
                );
            }
        }

        // EAGER cgroup_bw throttle: peek the front of the local DSQ. If
        // that task's cgroup is currently throttled by cpu.max, eagerly
        // remove the task from the DSQ + ops.dequeue + ops.quiescent +
        // stash in `bw_blocked[cgid]`. The cgroup's replenish (detected
        // post-fire_timer) will drain `bw_blocked[cgid]` and re-runnable
        // each task via the wakeup path (ops.runnable + ops.select_cpu +
        // ops.enqueue), letting LAVD's `can_direct_dispatch` decide
        // afresh.
        //
        // This is the kernel-faithful model: the kernel's bandwidth
        // controller dequeues throttled tasks entirely (calls
        // dequeue_task_scx → ops.dequeue) rather than head-of-line
        // blocking the dispatch path. The previous LAZY admission gate
        // (`CgroupBwDenied` then leave-in-DSQ) prevented scxsim's
        // throttled tasks from re-traversing the wakeup→select_cpu→
        // can_direct_dispatch chain that real LAVD takes on every
        // replenish, masking the simple-insert direct-dispatch path
        // entirely. See tg
        // `investigate-dsq-insert-vs-vtime-path-divergence` for the
        // 16,000× ratio inversion that motivated this fix.
        //
        // Single-cgroup head-of-line behavior preserved: we only check
        // the front of the local DSQ, not all queued tasks. Bug-1
        // reproducer is single-cgroup so this is sufficient; multi-
        // cgroup fairness is still out of scope here.
        let bw_blocked_pid = s.sim.cpus[cpu.0 as usize]
            .local_dsq
            .front()
            .copied()
            .and_then(|front_pid| {
                pid_is_bw_throttled(&self.scheduler, &s.fields(), front_pid)
                    .map(|cgid| (front_pid, cgid))
            });

        if let Some((pid, cgid)) = bw_blocked_pid {
            eager_stash_throttled(s, pid, cgid, cpu);
            // Fall through to the CPU-idle handling below.
        } else if let Some(pid) = s.sim.cpus[cpu.0 as usize].local_dsq.pop_front() {
            // Try to pull a task from the local DSQ
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::PickTask { pid });
            drop(guard);
            self.start_running(cpu, pid, sim_arc, monitor);
            return;
        }
        {
            // CPU is idle — update the C idle cpumask so
            // scx_bpf_test_and_clear_cpu_idle works correctly
            ffi::cpumask_set_idle(cpu.0 as i32);
            // Check if all siblings are idle too (full-idle core)
            s.sim.update_smt_mask_idle(cpu);
            let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            kfuncs::set_sim_clock(local_t, Some(cpu));

            // V4-C fix: clear `prev_task` once we've decided the CPU is
            // genuinely idle. Otherwise the next `lavd_dispatch(cpu, prev)`
            // call passes the stale prev_task to the scheduler, and LAVD's
            // fall-through path `consume_prev(prev, ...)` →
            // `update_stat_for_refill(prev)` →
            // `account_task_runtime(prev)` →
            // `scx_cgroup_bw_consume(prev->cgroup, task_time_wall)` charges
            // prev's cgroup with full inter-tick wall time despite prev
            // being off-CPU. That's the V4-A "engine over-charge" bug —
            // 100M ns/period accumulates as debt → keep_throttled forever
            // → drain bails → V3 stall pattern. Real kernel never sees
            // this because pick_next_task during idle returns the IDLE
            // TASK (root cgroup), not the last user task.
            //
            // tg `scxsim-fix-cbw-debt-runaway-or-document-as-known-cpu-bw-stall-bug`
            s.sim.cpus[cpu.0 as usize].prev_task = None;

            if notify_idle {
                // Notify scheduler that CPU is entering idle (ops.update_idle)
                set_ops_context(&mut s.sim, OpsContext::UpdateIdle);
                debug!("enter:structop update_idle(idle=true)");
                start_rbc(&mut s.sim);
                let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
                sim_callback!(s, guard, sim_arc, cpu, {
                    self.scheduler.update_idle(cpu.0 as i32, true);
                });
                let s = &mut *guard;
                s.sim
                    .trace
                    .record(__local_t, cpu, TraceKind::UpdateIdle { cpu, idle: true });
                charge_sched_time(&mut s.sim, cpu, "update_idle");
            }

            let s = &mut *guard;
            s.sim.trace.record(local_t, cpu, TraceKind::CpuIdle);
            info!(cpu = cpu.0, "IDLE");
        }
    }

    /// Handle a `DsqConsume` event: consume from global DSQ into local DSQ.
    ///
    /// If the local DSQ is still empty, tries to move a task from the global
    /// DSQ. If successful, schedules a `StartRunning` event for the consumed
    /// task. If nothing is available, the CPU goes idle.
    fn handle_dsq_consume(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let cpu_idx = cpu.0 as usize;

        // If local DSQ already has tasks (e.g., another CPU dispatched here),
        // skip the global DSQ consume and go straight to picking.
        if s.sim.cpus[cpu_idx].local_dsq.is_empty() {
            let consumed = s.sim.consume_dsq_to_local(DsqId::GLOBAL, cpu);
            if consumed {
                let __local_t = s.sim.cpus[cpu_idx].local_clock;
                s.sim.trace.record(
                    __local_t,
                    cpu,
                    TraceKind::DsqMoveToLocal {
                        dsq_id: DsqId::GLOBAL,
                        success: true,
                    },
                );
            }
        }

        // EAGER cgroup_bw throttle (mirror of post_dispatch_run's gate).
        // See `eager_stash_throttled` for the kernel-faithful rationale.
        let bw_blocked_pid = s.sim.cpus[cpu_idx]
            .local_dsq
            .front()
            .copied()
            .and_then(|front_pid| {
                pid_is_bw_throttled(&self.scheduler, &s.fields(), front_pid)
                    .map(|cgid| (front_pid, cgid))
            });

        if let Some((pid, cgid)) = bw_blocked_pid {
            eager_stash_throttled(s, pid, cgid, cpu);
            // Fall through to CPU-idle handling below.
        } else if let Some(pid) = s.sim.cpus[cpu_idx].local_dsq.pop_front() {
            let __local_t = s.sim.cpus[cpu_idx].local_clock;
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::PickTask { pid });
            drop(guard);
            self.start_running(cpu, pid, sim_arc, monitor);
            return;
        }
        {
            // CPU is idle
            ffi::cpumask_set_idle(cpu.0 as i32);
            s.sim.update_smt_mask_idle(cpu);
            let local_t = s.sim.cpus[cpu_idx].local_clock;
            kfuncs::set_sim_clock(local_t, Some(cpu));
            // V4-C fix (mirror of the post_dispatch_run path): clear
            // prev_task on idle so the next dispatch doesn't fall through
            // to consume_prev with a stale prev. See V4-C commentary in
            // post_dispatch_run for the full rationale.
            s.sim.cpus[cpu_idx].prev_task = None;
            s.sim.trace.record(local_t, cpu, TraceKind::CpuIdle);
            info!(cpu = cpu.0, "IDLE (dsq_consume)");
        }
    }

    /// Handle a `StartRunning` event: start a specific task on a CPU.
    ///
    /// Delegates to the existing `start_running` method. If the task has
    /// become invalid (exited, or CPU is no longer idle), safely skips.
    fn handle_start_running_event(
        &self,
        cpu: CpuId,
        pid: Pid,
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        // If CPU already has a task running (e.g., a kick caused preemption
        // and dispatch before this event), skip.
        if s.sim.cpus[cpu.0 as usize].current_task.is_some() {
            return;
        }
        drop(guard);
        self.start_running(cpu, pid, sim_arc, monitor);
    }

    /// Handle a `KickDelivered` event: process a delivered IPI on the target CPU.
    ///
    /// Matches the logic from the former `process_kicked_cpus` but for a
    /// single CPU+flags pair delivered as a timed event.
    fn handle_kick_delivered(
        &self,
        cpu: CpuId,
        flags: KickFlags,
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        if flags.contains(KickFlags::PREEMPT) && s.sim.cpus[cpu.0 as usize].current_task.is_some() {
            drop(guard);
            self.preempt_current(cpu, sim_arc, monitor);
        } else if flags.contains(KickFlags::IDLE) {
            let is_idle = s.sim.cpus[cpu.0 as usize].current_task.is_none();
            if is_idle {
                drop(guard);
                self.try_dispatch_and_run(cpu, sim_arc, monitor);
            }
        } else {
            drop(guard);
            self.try_dispatch_and_run(cpu, sim_arc, monitor);
        }
    }

    /// Preempt the currently running task on `cpu` mid-slice.
    ///
    /// Computes how much of the slice was consumed, deducts it from
    /// `run_remaining_ns`, calls `stopping()` + `enqueue()`, then
    /// dispatches the next task via `try_dispatch_and_run()`.
    fn preempt_current(&self, cpu: CpuId, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let pid = match s.sim.cpus[cpu.0 as usize].current_task {
            Some(pid) => pid,
            None => return,
        };

        // Read CPU state BEFORE borrowing tasks (split borrow through MutexGuard)
        let local_clock = s.sim.cpus[cpu.0 as usize].local_clock;
        let started_at = s.sim.cpus[cpu.0 as usize]
            .task_started_at
            .unwrap_or(local_clock);
        let original_slice = s.sim.cpus[cpu.0 as usize].task_original_slice.unwrap_or(0);
        let consumed = local_clock.saturating_sub(started_at);

        let task = match s.tasks.get_mut(&pid) {
            Some(t) => t,
            None => return,
        };

        task.run_remaining_ns = task.run_remaining_ns.saturating_sub(consumed);

        s.sim
            .trace
            .record(local_clock, cpu, TraceKind::TaskPreempted { pid });

        let task_name = task.name.as_str();
        info!(
            task = task_name,
            pid = pid.0,
            ran_ns = %FmtN(consumed),
            "PREEMPTED (tick)"
        );

        let raw = task.raw();
        task.state = TaskState::Runnable;

        // Set remaining slice on raw task (used by stopping() for vtime accounting)
        let remaining_slice = original_slice.saturating_sub(consumed);

        drop(guard);
        self.stop_and_reenqueue(
            cpu,
            pid,
            raw,
            consumed,
            remaining_slice,
            sim_arc,
            monitor,
            |_, _, _| {}, // no extra traces before enqueue
            |_, _, _| {}, // no extra traces after enqueue
        );
    }

    /// Stop a running task, re-enqueue it, and dispatch the next task.
    ///
    /// Common spine for both `handle_slice_expired` and `preempt_current`.
    /// Callers must have already set `task.state = Runnable` and computed
    /// `consumed` / `remaining_slice`. The two callbacks allow callers to
    /// inject path-specific trace records:
    /// - `pre_enqueue`: called after stopping(), before enqueue()
    /// - `post_enqueue`: called after enqueue()
    #[allow(clippy::too_many_arguments)]
    fn stop_and_reenqueue(
        &self,
        cpu: CpuId,
        pid: Pid,
        raw: *mut c_void,
        consumed: TimeNs,
        remaining_slice: TimeNs,
        sim_arc: &SimArc,
        monitor: &mut dyn Monitor,
        pre_enqueue: impl FnOnce(&mut SimulatorState, CpuId, Pid),
        post_enqueue: impl FnOnce(&mut SimulatorState, CpuId, Pid),
    ) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let cpu_idx = cpu.0 as usize;

        // Clear CPU state
        s.sim.cpus[cpu_idx].current_task = None;
        s.sim.cpus[cpu_idx].prev_task = Some(pid);
        s.sim.cpus[cpu_idx].task_started_at = None;
        s.sim.cpus[cpu_idx].task_original_slice = None;

        // Apply CSW overhead (see #NOTE TIMING_MODEL)
        let overhead = s.sim.csw_overhead(LastStopReason::Involuntary);
        s.sim.cpus[cpu_idx].local_clock += overhead;
        kfuncs::clock_window_check(cpu, s.sim.cpus[cpu_idx].local_clock);

        // Set remaining slice on raw task (used by stopping() for vtime)
        crate::ffi::task_set_slice(raw, remaining_slice);

        // Update sum_exec_runtime
        {
            let task = s.tasks.get(&pid).unwrap();
            update_sum_exec(raw, task.sum_exec_base, consumed);
        }

        // Charge consumed CPU time against this task's cgroup cpu.max quota
        // (Diff 3 wiring). Done BEFORE the scheduler's stopping() callback so
        // any LAVD-side accounting (Diff 4) sees a coherent post-charge view.
        {
            let now_ns = s.sim.cpus[cpu_idx].local_clock;
            let mut f = s.fields();
            charge_cgroup_bw(&mut f, pid, consumed, now_ns, cpu);
        }

        // stopping()
        set_ops_context(&mut s.sim, OpsContext::Stopping);
        debug!(pid = pid.0, runnable = true, "enter:structop stopping");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.stopping(TaskPtr::new(raw), true);
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "stopping");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::Stopping, cpu);

        // Monitor: Stopping probe
        monitor.sample(&ProbeContext {
            point: ProbePoint::Stopping,
            pid,
            cpu,
            time_ns: s.sim.cpus[cpu_idx].local_clock,
            task_raw: raw,
            trace: &s.sim.trace,
        });

        // Caller-specific traces before enqueue
        pre_enqueue(&mut s.sim, cpu, pid);

        // Re-enqueue
        s.sim.set_task_ops_state(pid, OpsTaskState::Queued);
        debug!(pid = pid.0, "enqueue (re-enqueue)");
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.enqueue(TaskPtr::new(raw), 0);
        });
        let s = &mut *guard;

        // Resolve deferred dispatch from enqueue callback
        s.sim.resolve_pending_dispatch(cpu);

        // Caller-specific traces after enqueue
        post_enqueue(&mut s.sim, cpu, pid);

        // Flush staged events + dispatch next task
        flush_staged_events(&mut s.sim, &mut s.events);
        drop(guard);
        self.try_dispatch_and_run(cpu, sim_arc, monitor);
    }

    /// Start running a task on a CPU.
    fn start_running(&self, cpu: CpuId, pid: Pid, sim_arc: &SimArc, monitor: &mut dyn Monitor) {
        let mut guard = sim_arc.lock().unwrap();
        let s = &mut *guard;
        let task = match s.tasks.get_mut(&pid) {
            Some(t) => t,
            None => return,
        };

        // Skip exited tasks that are still lingering in DSQs
        if matches!(task.state, TaskState::Exited) {
            drop(guard);
            self.try_dispatch_and_run(cpu, sim_arc, monitor);
            return;
        }

        task.state = TaskState::Running { cpu };
        // Track whether this is a migration (for migration penalty).
        let migrated = task.prev_cpu != cpu;
        let cross_llc = migrated
            && s.sim.cpus[task.prev_cpu.0 as usize].llc_id != s.sim.cpus[cpu.0 as usize].llc_id;
        task.prev_cpu = cpu;
        // Clear runnable_at_ns: task is now running (watchdog reset).
        task.runnable_at_ns = None;
        s.sim.cpus[cpu.0 as usize].current_task = Some(pid);
        s.sim.cpus[cpu.0 as usize].prev_task = None;
        // Reset IRQ stolen time for this new run period.
        s.sim.cpus[cpu.0 as usize].irq_stolen_ns = 0;
        s.sim.task_last_cpu.insert(pid, cpu);
        // Kernel clears SCX_TASK_QUEUED when a task is picked to run.
        s.sim.clear_task_queued(pid);
        // Clear idle bit in the C cpumask (in case scheduler didn't call
        // scx_bpf_test_and_clear_cpu_idle for this CPU)
        let was_idle = ffi::test_and_clear_cpu_idle(cpu.0 as i32);
        // CPU is now busy — core is no longer fully idle
        s.sim.update_smt_mask_busy(cpu);

        // Save task data before potential sim_callback! (which drops guard)
        let raw = task.raw();
        task.sum_exec_base = ffi::task_get_sum_exec_runtime(raw);
        let task_enabled = task.enabled;
        if !task_enabled {
            task.enabled = true;
        }
        // NLL: task is no longer used after this point.

        // Notify scheduler that CPU is exiting idle (ops.update_idle)
        if was_idle {
            set_ops_context(&mut s.sim, OpsContext::UpdateIdle);
            debug!("enter:structop update_idle(idle=false)");
            start_rbc(&mut s.sim);
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            sim_callback!(s, guard, sim_arc, cpu, {
                self.scheduler.update_idle(cpu.0 as i32, false);
            });
            let s = &mut *guard;
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::UpdateIdle { cpu, idle: false });
            charge_sched_time(&mut s.sim, cpu, "update_idle");
        }
        let s = &mut *guard;
        if !task_enabled {
            set_ops_context(&mut s.sim, OpsContext::Enable);
            debug!(pid = pid.0, "enter:structop enable");
            start_rbc(&mut s.sim);
            let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
            sim_callback!(s, guard, sim_arc, cpu, {
                self.scheduler.enable(TaskPtr::new(raw));
            });
            let s = &mut *guard;
            // TOP-5 of secondary TraceKind easy-win bundle: emit
            // ops.enable so the live-vs-sim diff harness can validate
            // the per-task one-shot enable handshake order matches.
            s.sim
                .trace
                .record(__local_t, cpu, TraceKind::Enable { pid });
            charge_sched_time(&mut s.sim, cpu, "enable");
        }

        // Call running
        let s = &mut *guard;
        set_ops_context(&mut s.sim, OpsContext::Running);
        debug!(pid = pid.0, "enter:structop running");
        start_rbc(&mut s.sim);
        sim_callback!(s, guard, sim_arc, cpu, {
            self.scheduler.running(TaskPtr::new(raw));
        });
        let s = &mut *guard;
        charge_sched_time(&mut s.sim, cpu, "running");
        maybe_record_checkpoint(&s.sim, CheckpointEvent::Running, cpu);

        // Monitor: Running probe
        monitor.sample(&ProbeContext {
            point: ProbePoint::Running,
            pid,
            cpu,
            time_ns: s.sim.cpus[cpu.0 as usize].local_clock,
            task_raw: raw,
            trace: &s.sim.trace,
        });

        // Enforce wakeup latency floor with heavy-tailed distribution.
        // Models kernel overhead (IPI, context switch, cache warming) that
        // exists even with zero queuing delay. Uses log-normal base with
        // rare heavy-tail spikes matching production p99/p50 ratios.
        if s.sim.overhead.enabled {
            let floor = s.sim.overhead.wakeup_latency_floor_ns;
            let mig_penalty = s.sim.overhead.migration_penalty_ns;

            if let Some(enq_t) = s.tasks.get(&pid).and_then(|t| t.enqueued_at_ns) {
                // Heavy-tailed wakeup latency (log-normal + Pareto spikes)
                let effective_floor = if floor > 0 {
                    s.sim.sample_wakeup_latency_ns(floor)
                } else {
                    0
                };

                // Migration penalty: extra cache/TLB warming cost.
                // Cross-LLC migrations incur additional penalty for remote
                // LLC fetch and full TLB flush.
                let effective_floor = if migrated && mig_penalty > 0 {
                    let cross_llc_extra = if cross_llc {
                        s.sim.overhead.cross_llc_migration_penalty_ns
                    } else {
                        0
                    };
                    effective_floor + mig_penalty + cross_llc_extra
                } else {
                    effective_floor
                };

                let min_scheduled_at = enq_t + effective_floor;
                if s.sim.cpus[cpu.0 as usize].local_clock < min_scheduled_at {
                    s.sim.cpus[cpu.0 as usize].local_clock = min_scheduled_at;
                }
            }
        }
        // Clear enqueued_at_ns now that the floor has been applied.
        if let Some(task) = s.tasks.get_mut(&pid) {
            task.enqueued_at_ns = None;
        }

        let __local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        s.sim
            .trace
            .record(__local_t, cpu, TraceKind::SetNextTask { pid });

        let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
        kfuncs::set_sim_clock(local_t, Some(cpu));

        s.sim
            .trace
            .record(local_t, cpu, TraceKind::TaskScheduled { pid });

        // Determine how long this task will run.
        // Apply run-time jitter: models compute variability from cache misses,
        // branch mispredictions, TLB misses, and memory bandwidth contention.
        {
            let task = s.tasks.get_mut(&pid).unwrap();
            if task.run_remaining_ns > 0
                && s.sim.noise.enabled
                && s.sim.noise.run_jitter
                && s.sim.noise.run_jitter_cv_ppm > 0
            {
                let base = task.run_remaining_ns;
                let stddev =
                    (base as u128 * s.sim.noise.run_jitter_cv_ppm as u128 / 1_000_000) as u64;
                let noise = s.sim.sample_normal_ns(stddev);
                task.run_remaining_ns = (base as i64 + noise).max(1) as u64;
            }
        }
        // Cap the slice by the cgroup's remaining `cpu.max` budget. If
        // the budget is shorter than the scheduler's slice, the task
        // preempts at the budget boundary so the cgroup transitions to
        // throttled (the library's accounting timer + replenish chain
        // handle the actual throttle decision -- see Stage E).
        // Returns None (no cap) when (a) task not in tracked cgroup,
        // (b) scheduler does not link cgroup_bw, or (c) library reports
        // unlimited.
        let bw_budget = pid_bw_max_run_ns(&self.scheduler, &s.fields(), pid);
        let task = s.tasks.get(&pid).unwrap();
        let raw_slice = task.get_slice();
        let remaining = task.run_remaining_ns;
        let slice = match (raw_slice, bw_budget) {
            (0, Some(budget)) => budget.max(1),
            (sl, Some(budget)) => sl.min(budget.max(1)),
            (sl, None) => sl,
        };

        // Track when task started for mid-slice preemption accounting
        s.sim.cpus[cpu.0 as usize].task_started_at = Some(local_t);
        s.sim.cpus[cpu.0 as usize].task_original_slice = Some(slice);

        info!(
            task = task.name.as_str(),
            pid = pid.0,
            slice_ns = %FmtN(slice),
            "STARTED"
        );

        if remaining == 0 {
            // Task has no remaining work -- complete immediately
            s.events.push(local_t, EventKind::TaskPhaseComplete { cpu });
        } else if slice > 0 && slice <= remaining {
            // Slice expires before the phase completes
            s.events
                .push(local_t + slice, EventKind::SliceExpired { cpu });
        } else {
            // Phase completes before the slice
            s.events
                .push(local_t + remaining, EventKind::TaskPhaseComplete { cpu });
        }
    }

    /// Advance a task past completed Sleep/Wake phases to the next Run phase.
    ///
    /// Called when a wake event fires. The current phase (Sleep) is considered
    /// complete (the wake timer fired), so we advance past it. We also skip
    /// any Wake phases (triggering wakes for other tasks) until we reach a
    /// Run phase that the task can execute.
    fn advance_to_run_phase(
        &self,
        task: &mut SimTask,
        sim: &mut SimulatorState,
        events: &mut EventQueue,
    ) {
        let pid = task.pid;
        loop {
            match task.current_phase() {
                Some(Phase::Run(ns)) => {
                    if task.run_remaining_ns == 0 {
                        task.run_remaining_ns = *ns;
                    }
                    break;
                }
                Some(Phase::Wake(target_pid)) => {
                    let target = *target_pid;
                    events.push(
                        sim.clock,
                        EventKind::TaskWake {
                            pid: target,
                            waker: None,
                            cpu: task.prev_cpu,
                        },
                    );
                    if !task.advance_phase() {
                        task.state = TaskState::Exited;
                        sim.trace.record(
                            sim.clock,
                            sim.current_cpu,
                            TraceKind::TaskCompleted { pid },
                        );
                        info!(task = task.name.as_str(), pid = pid.0, "COMPLETED");
                        return;
                    }
                }
                Some(Phase::Sleep(_)) => {
                    if !task.advance_phase() {
                        task.state = TaskState::Exited;
                        sim.trace.record(
                            sim.clock,
                            sim.current_cpu,
                            TraceKind::TaskCompleted { pid },
                        );
                        info!(task = task.name.as_str(), pid = pid.0, "COMPLETED");
                        return;
                    }
                    // Continue looping to handle Wake phases or verify Run
                }
                None => {
                    task.state = TaskState::Exited;
                    sim.trace
                        .record(sim.clock, sim.current_cpu, TraceKind::TaskCompleted { pid });
                    info!(task = task.name.as_str(), pid = pid.0, "COMPLETED");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nm_text_symbol_from_helper_source() {
        let line = "0000000000000900 t get_cpu_ctx\t/some/path/util.bpf.c:71";
        assert_eq!(parse_nm_helper_symbol(line), Some("get_cpu_ctx".to_owned()));
    }

    #[test]
    fn parse_nm_global_text_symbol_from_wrapper() {
        let line = "000000000001efc0 T bpf_cgroup_from_id\t/some/path/wrapper.c:385";
        assert_eq!(
            parse_nm_helper_symbol(line),
            Some("bpf_cgroup_from_id".to_owned())
        );
    }

    #[test]
    fn parse_nm_skips_non_text_symbols() {
        // Data symbol (V) from util.bpf.c — should be ignored.
        let line = "000000000001f000 V cpu_ctx_stor\t/some/path/util.bpf.c:62";
        assert_eq!(parse_nm_helper_symbol(line), None);
    }

    #[test]
    fn parse_nm_skips_underscore_prefix() {
        let line = "0000000000000900 T _internal_func\t/some/path/wrapper.c:10";
        assert_eq!(parse_nm_helper_symbol(line), None);
    }

    #[test]
    fn parse_nm_skips_dot_prefix() {
        let line = "0000000000000900 t .hidden_func\t/some/path/util.bpf.c:5";
        assert_eq!(parse_nm_helper_symbol(line), None);
    }

    #[test]
    fn parse_nm_skips_non_helper_source() {
        let line = "0000000000000490 T simple_dispatch\t/some/path/scx_simple.bpf.c:98";
        assert_eq!(parse_nm_helper_symbol(line), None);
    }

    #[test]
    fn parse_nm_skips_lines_without_tab() {
        let line = "                 U calloc";
        assert_eq!(parse_nm_helper_symbol(line), None);
    }

    #[test]
    fn parse_nm_source_without_line_number() {
        // Some nm outputs omit the line number.
        let line = "0000000000000900 t helper_fn\t/some/path/wrapper.c";
        assert_eq!(parse_nm_helper_symbol(line), Some("helper_fn".to_owned()));
    }

    #[test]
    fn lldb_helper_commands_empty_names() {
        assert_eq!(DebuggerFlavor::Lldb.build_helper_commands(&[]), None);
    }

    #[test]
    fn gdb_helper_commands_empty_names() {
        assert_eq!(DebuggerFlavor::Gdb.build_helper_commands(&[]), None);
    }

    #[test]
    fn lldb_helper_commands_single_name() {
        let names = vec!["get_cpu_ctx".to_owned()];
        let cmds = DebuggerFlavor::Lldb.build_helper_commands(&names).unwrap();
        assert!(cmds.contains("command alias skip-helpers"));
        assert!(cmds.contains("command alias unskip-helpers"));
        assert!(cmds.contains("\"^(get_cpu_ctx)\""));
        assert!(cmds.contains("settings clear target.process.thread.step-avoid-regexp"));
    }

    #[test]
    fn lldb_helper_commands_multiple_names() {
        let names = vec![
            "calc_avg".to_owned(),
            "get_cpu_ctx".to_owned(),
            "stat_inc".to_owned(),
        ];
        let cmds = DebuggerFlavor::Lldb.build_helper_commands(&names).unwrap();
        assert!(cmds.contains("command alias skip-helpers"));
        assert!(cmds.contains("\"^(calc_avg|get_cpu_ctx|stat_inc)\""));
        assert!(cmds.contains("command alias unskip-helpers"));
    }

    #[test]
    fn gdb_helper_commands_single_name() {
        let names = vec!["get_cpu_ctx".to_owned()];
        let cmds = DebuggerFlavor::Gdb.build_helper_commands(&names).unwrap();
        assert!(cmds.contains("define skip-helpers"));
        assert!(cmds.contains("skip -rfu \"^(get_cpu_ctx)\""));
        assert!(cmds.contains("define unskip-helpers"));
        assert!(cmds.contains("info skip"));
    }

    #[test]
    fn gdb_helper_commands_multiple_names() {
        let names = vec![
            "calc_avg".to_owned(),
            "get_cpu_ctx".to_owned(),
            "stat_inc".to_owned(),
        ];
        let cmds = DebuggerFlavor::Gdb.build_helper_commands(&names).unwrap();
        assert!(cmds.contains("define skip-helpers"));
        assert!(cmds.contains("skip -rfu \"^(calc_avg|get_cpu_ctx|stat_inc)\""));
        assert!(cmds.contains("define unskip-helpers"));
    }

    #[test]
    fn lldb_fmt_breakpoint() {
        assert_eq!(
            DebuggerFlavor::Lldb.fmt_breakpoint("simple_init"),
            "breakpoint set --name simple_init\n"
        );
    }

    #[test]
    fn gdb_fmt_breakpoint() {
        assert_eq!(
            DebuggerFlavor::Gdb.fmt_breakpoint("simple_init"),
            "break simple_init\n"
        );
    }

    #[test]
    fn gdb_skip_simulator_contains_rust_mangled_skip() {
        let skip = DebuggerFlavor::Gdb.fmt_skip_simulator();
        assert!(skip.contains("skip -rfu \"^_ZN\""));
        assert!(skip.contains("skip -rfu \"^scx_bpf_\""));
        assert!(skip.contains("skip -rfu \"^scx_test_\""));
        assert!(skip.contains("skip -rfu \"^sim_\""));
    }

    #[test]
    fn lldb_skip_simulator_uses_step_avoid_libraries() {
        let skip = DebuggerFlavor::Lldb.fmt_skip_simulator();
        assert!(skip.contains("settings set target.process.thread.step-avoid-libraries"));
    }

    #[test]
    fn debugger_flavor_extensions() {
        assert_eq!(DebuggerFlavor::Lldb.extension(), "lldb");
        assert_eq!(DebuggerFlavor::Gdb.extension(), "gdb");
    }

    #[test]
    fn gdb_signal_handling_contains_sigfpe() {
        let cmds = DebuggerFlavor::Gdb.fmt_signal_handling();
        assert!(cmds.contains("handle SIGFPE nostop noprint pass"));
    }

    #[test]
    fn lldb_signal_handling_contains_sigfpe() {
        let cmds = DebuggerFlavor::Lldb.fmt_signal_handling();
        assert!(cmds.contains("process handle SIGFPE -s false -n false -p true"));
    }
}
