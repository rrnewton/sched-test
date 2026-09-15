//! Runtime simulated task with FFI-backed C `task_struct`.
//!
//! `SimTask` owns a heap-allocated C `task_struct` pointer and uses
//! `unsafe` FFI calls for allocation, field access, and deallocation.
//! The pure type definitions it depends on live in `safe::task`.

use std::ffi::c_void;

use crate::ffi;
use crate::task::{
    nice_to_weight, sched_weight_to_cgroup, Phase, RepeatMode, TaskBehavior, TaskDef, TaskState,
};
use crate::types::{CpuId, Pid, TimeNs, Vtime};

/// A simulated task at runtime.
pub struct SimTask {
    /// Raw pointer to the heap-allocated C task_struct.
    raw: *mut c_void,
    /// Task PID (also stored in the raw task_struct).
    pub pid: Pid,
    /// The task's name.
    pub name: String,
    /// Scripted behavior phases.
    pub behavior: TaskBehavior,
    /// Current phase index.
    pub phase_idx: usize,
    /// Current repeat iteration (0-based). Incremented each time the phase
    /// sequence wraps back to the beginning.
    pub repeat_iteration: u32,
    /// Remaining nanoseconds in the current Run phase (only meaningful
    /// when the current phase is `Phase::Run` or `Phase::SystemCpu`).
    pub run_remaining_ns: TimeNs,
    /// Current task state.
    pub state: TaskState,
    /// Whether `enable` has been called for this task.
    pub enabled: bool,
    /// The last CPU the task ran on (for select_cpu prev_cpu).
    pub prev_cpu: CpuId,
    /// Timestamp (simulated ns) when the task became Runnable.
    ///
    /// Used by the watchdog to detect stalled tasks. Set when the task
    /// transitions to Runnable, cleared (set to None) when the task starts
    /// Running or goes to Sleeping. Matches kernel semantics: only reset
    /// when the task actually runs, not when dequeued and re-enqueued.
    pub runnable_at_ns: Option<TimeNs>,
    /// Snapshot of `sum_exec_runtime` when the task last started running.
    ///
    /// The kernel's `update_curr()` increments `p->se.sum_exec_runtime`
    /// by CPU time consumed. The simulator mirrors this by saving the
    /// base value at `running()` and computing `base + elapsed` before
    /// `tick()` and `stopping()` callbacks.
    pub sum_exec_base: TimeNs,
    /// Timestamp (simulated ns) when the task was last enqueued.
    ///
    /// Used by the wakeup latency floor to ensure a minimum time between
    /// enqueue and schedule, modeling kernel overhead (IPI, context switch
    /// setup, cache warming). Set when EnqueueTask is recorded, consumed
    /// when the task starts running.
    pub enqueued_at_ns: Option<TimeNs>,
}

impl SimTask {
    /// Create a new simulated task from a definition.
    pub fn new(def: &TaskDef, nr_cpus: u32) -> Self {
        // SAFETY: `sim_task_alloc` allocates a zeroed `task_struct` on the
        // heap. The returned pointer is non-null (asserted below).
        let raw = unsafe { ffi::sim_task_alloc() };
        assert!(!raw.is_null(), "sim_task_alloc returned null");

        let scx_weight = sched_weight_to_cgroup(nice_to_weight(def.nice));

        // SAFETY: `raw` is a valid, non-null task_struct pointer just
        // allocated above. All ffi setter functions operate on fields
        // within the allocation and are safe for any valid pointer.
        unsafe {
            ffi::sim_task_set_pid(raw, def.pid.0);
            // The kernel stores cgroup-weight-space [1..10000] in p->scx.weight,
            // not the raw sched_prio_to_weight value.
            ffi::sim_task_set_weight(raw, scx_weight);
            // Default: task can run on all CPUs
            ffi::sim_task_set_nr_cpus_allowed(raw, nr_cpus as i32);
            // static_prio = nice + 120
            ffi::sim_task_set_static_prio(raw, def.nice as i32 + 120);
            // Set task_struct flags (PF_KTHREAD, PF_WQ_WORKER, etc.)
            if def.task_flags != 0 {
                ffi::sim_task_set_flags(raw, def.task_flags);
            }
            // Set comm from the task name (scheduler code reads p->comm)
            let comm = std::ffi::CString::new(def.name.as_str()).unwrap_or_default();
            ffi::sim_task_set_comm(raw, comm.as_ptr());
            // Set migration_disabled counter
            if def.migration_disabled > 0 {
                ffi::sim_task_set_migration_disabled(raw, def.migration_disabled);
            }
            // Credentials. Written unconditionally rather than only for
            // non-default ids: `sim_task_alloc()` zeroes the cred, so the
            // default (0, 0) already holds, but writing it keeps the one
            // authority for the value in `TaskDef` rather than split between
            // Rust and a calloc.
            ffi::sim_task_set_cred_ids(raw, def.uid.0, def.gid.0);
        }

        // Initialize run_remaining from the first CPU-consuming phase.
        let run_remaining_ns = match def.behavior.phases.first() {
            Some(Phase::Run(ns) | Phase::SystemCpu(ns)) => *ns,
            _ => 0,
        };

        // In the kernel, a new task's cpu field is set to the CPU where
        // it was forked, which is always within its cpumask.
        let initial_cpu = def.initial_cpu();

        SimTask {
            raw,
            pid: def.pid,
            name: def.name.clone(),
            behavior: def.behavior.clone(),
            phase_idx: 0,
            repeat_iteration: 0,
            run_remaining_ns,
            state: TaskState::Sleeping,
            enabled: false,
            prev_cpu: initial_cpu,
            runnable_at_ns: None,
            sum_exec_base: 0,
            enqueued_at_ns: None,
        }
    }

    /// Get the raw C task_struct pointer (for passing to scheduler ops).
    pub fn raw(&self) -> *mut c_void {
        self.raw
    }

    /// Get the current phase, or None if the task has completed all phases.
    pub fn current_phase(&self) -> Option<&Phase> {
        self.behavior.phases.get(self.phase_idx)
    }

    /// Advance to the next phase. Returns true if there is a next phase.
    pub fn advance_phase(&mut self) -> bool {
        self.phase_idx += 1;
        if self.phase_idx >= self.behavior.phases.len() {
            match self.behavior.repeat {
                RepeatMode::Once => return false,
                RepeatMode::Forever => {
                    self.phase_idx = 0;
                    self.repeat_iteration += 1;
                }
                RepeatMode::Count(n) => {
                    self.repeat_iteration += 1;
                    if self.repeat_iteration >= n {
                        return false;
                    }
                    self.phase_idx = 0;
                }
            }
        }
        // Reset run_remaining for the new CPU-consuming phase.
        match self.current_phase() {
            Some(Phase::Run(ns) | Phase::SystemCpu(ns)) => self.run_remaining_ns = *ns,
            _ => self.run_remaining_ns = 0,
        }
        true
    }

    /// Read the task's current slice from the C task_struct.
    pub fn get_slice(&self) -> u64 {
        // SAFETY: `self.raw` is non-null and valid (invariant of SimTask).
        unsafe { ffi::sim_task_get_slice(self.raw) }
    }

    /// Read the task's dsq_vtime from the C task_struct.
    // Unused on the sequential path (unlike the sibling `get_slice`); the
    // SimTaskHandle counterpart is dormant too (that safe accessor API is not
    // yet wired into the engine). Kept for SimTask get/set symmetry.
    #[allow(dead_code)]
    pub fn get_dsq_vtime(&self) -> Vtime {
        // SAFETY: `self.raw` is non-null and valid (invariant of SimTask).
        Vtime(unsafe { ffi::sim_task_get_dsq_vtime(self.raw) })
    }
}

impl Drop for SimTask {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was allocated by `sim_task_alloc` in `new()` and
        // is freed exactly once here. No other code frees this pointer.
        unsafe {
            ffi::sim_task_free(self.raw);
        }
    }
}
