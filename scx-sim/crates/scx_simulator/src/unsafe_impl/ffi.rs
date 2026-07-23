//! FFI declarations for scheduler ops and task_struct accessors.
//!
//! # Safety
//!
//! This module is almost entirely `unsafe` by nature: it declares `extern "C"`
//! blocks for C functions (`sim_task_*`, scheduler ops), performs `dlopen`/
//! `dlsym` to load scheduler `.so` files at runtime, and manipulates raw
//! `*mut c_void` pointers to opaque `task_struct` and `scx_init_task_args`
//! objects. Callers must ensure that pointers passed to these functions are
//! valid, non-null, and point to objects of the correct type.

use std::ffi::c_void;
use std::path::Path;

// ---------------------------------------------------------------------------
// task_struct accessors (implemented in csrc/sim_task.c)
// ---------------------------------------------------------------------------
extern "C" {
    pub fn sim_task_alloc() -> *mut c_void;
    pub fn sim_task_free(p: *mut c_void);
    pub fn sim_task_struct_size() -> usize;

    pub fn sim_task_set_pid(p: *mut c_void, pid: i32);
    pub fn sim_task_get_pid(p: *mut c_void) -> i32;
    pub fn sim_task_set_comm(p: *mut c_void, comm: *const i8);

    pub fn sim_task_set_weight(p: *mut c_void, weight: u32);
    pub fn sim_task_get_weight(p: *mut c_void) -> u32;
    pub fn sim_task_set_static_prio(p: *mut c_void, prio: i32);
    pub fn sim_task_set_flags(p: *mut c_void, flags: u32);
    pub fn sim_task_set_nr_cpus_allowed(p: *mut c_void, nr: i32);
    pub fn sim_task_get_nr_cpus_allowed(p: *mut c_void) -> i32;

    pub fn sim_task_get_dsq_vtime(p: *mut c_void) -> u64;
    pub fn sim_task_set_dsq_vtime(p: *mut c_void, vtime: u64);
    pub fn sim_task_get_slice(p: *mut c_void) -> u64;
    pub fn sim_task_set_slice(p: *mut c_void, slice: u64);
    pub fn sim_task_get_scx_weight(p: *mut c_void) -> u32;
    pub fn sim_task_set_scx_weight(p: *mut c_void, weight: u32);

    pub fn sim_task_setup_cpus_ptr(p: *mut c_void);
    pub fn sim_task_clear_cpumask(p: *mut c_void);
    pub fn sim_task_set_cpumask_cpu(p: *mut c_void, cpu: i32);
    pub fn sim_task_get_cpus_ptr(p: *mut c_void) -> *const c_void;
    pub fn sim_task_get_scx_flags(p: *mut c_void) -> u32;
    pub fn sim_task_set_scx_flags(p: *mut c_void, flags: u32);

    // Execution time accounting (se.sum_exec_runtime)
    pub fn sim_task_get_sum_exec_runtime(p: *mut c_void) -> u64;
    pub fn sim_task_set_sum_exec_runtime(p: *mut c_void, ns: u64);

    // Address space (mm_struct pointer)
    pub fn sim_task_set_mm(p: *mut c_void, mm: *mut c_void);
    pub fn sim_task_get_mm(p: *mut c_void) -> *mut c_void;

    // Parent-child relationship
    pub fn sim_task_set_real_parent(child: *mut c_void, parent: *mut c_void);

    // Migration disabled counter
    pub fn sim_task_set_migration_disabled(p: *mut c_void, val: u16);
    pub fn sim_task_get_migration_disabled(p: *mut c_void) -> u16;

    // Cpumask management (implemented in scx_test_cpumask.c)
    pub fn scx_test_set_all_cpumask(cpu: i32);
    pub fn scx_test_set_idle_cpumask(cpu: i32);
    pub fn scx_test_clear_idle_cpumask(cpu: i32);
    pub fn scx_test_set_idle_smtmask(cpu: i32);
    pub fn scx_test_clear_idle_smtmask(cpu: i32);
    pub fn scx_bpf_test_and_clear_cpu_idle(cpu: i32) -> bool;
    pub fn bpf_cpumask_test_cpu(cpu: u32, cpumask: *const c_void) -> bool;

    // Exit info for the exit callback (implemented in sim_task.c)
    pub fn sim_get_exit_info() -> *mut c_void;

    // Init task args for the init_task callback (implemented in sim_task.c)
    pub fn sim_get_init_task_args() -> *mut c_void;

    // Exit task args for the exit_task callback (implemented in sim_task.c)
    pub fn sim_get_exit_task_args() -> *mut c_void;

    // SDT / arena per-task storage (implemented in sim_sdt_stubs.c)
    pub fn scx_task_init(data_size: u64) -> i32;
    pub fn scx_task_alloc(p: *mut c_void) -> *mut c_void;
    pub fn scx_task_data(p: *mut c_void) -> *mut c_void;
    pub fn scx_task_free(p: *mut c_void);

    // Test-only fault injector for scx_task_alloc (scx GitHub #3564
    // reproducer). When nonzero, scx_task_alloc() returns NULL for the
    // task whose PID matches. Default 0. Defined in sim_sdt_stubs.c. See
    // the safe wrapper `set_task_alloc_fail_pid`.
    pub static mut sim_sdt_fail_pid: i32;

    // Cgroup allocation and management (implemented in sim_task.c)
    pub fn sim_cgroup_alloc(cgid: u64, level: u32, parent: *mut c_void) -> *mut c_void;
    pub fn sim_cgroup_free(cgrp: *mut c_void);
    pub fn sim_cgroup_get_kn_id(cgrp: *mut c_void) -> u64;
    pub fn sim_cgroup_set_cpuset(cgrp: *mut c_void, cpus: *const u32, nr_cpus: u32);
    pub fn sim_task_set_cgroup(p: *mut c_void, cgrp: *mut c_void);
    pub fn sim_task_get_cgroup(p: *mut c_void) -> *mut c_void;
    pub fn sim_set_init_task_cgroup(cgrp: *mut c_void);

    // Root cgroup accessor (implemented in sim_task.c)
    pub fn sim_get_root_cgroup() -> *mut c_void;

    // CSS iterator (implemented in sim_cgroup.c).
    //
    // The Rust side populates BOTH ordering buffers before any BPF
    // callback fires: `sim_css_iter_add` appends to the pre-order
    // list and `sim_css_iter_add_post` appends to the post-order
    // list. C-side `bpf_for_each(css, pos, root, flags)` then walks
    // whichever list matches the iteration mode (Phase 1 BPF infra
    // scale-up item 3).
    pub fn sim_css_iter_reset();
    pub fn sim_css_iter_add(cgrp: *mut c_void);
    pub fn sim_css_iter_add_post(cgrp: *mut c_void);
    pub fn sim_css_iter_set_root(root: *mut c_void);

    // Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
    // returns a pointer to a C-side static `struct scx_cgroup_init_args`
    // populated with default values (weight=100, period=100ms,
    // quota=-1=unlimited, burst=0). Used by `scheduler.cgroup_init` call
    // sites that previously passed `OptionalPtr::null()` -- the
    // pre-Phase-2 weak shim accepted NULL but the compiled-in cgroup_bw
    // library dereferences `args->bw_period_us` and SIGSEGVs on NULL.
    pub fn sim_get_default_cgroup_init_args() -> *mut c_void;

    // Global state reset functions for deterministic re-runs.
    // These reset lazy-initialization flags and static tables that
    // persist in the main binary across simulation runs.
    pub fn sim_task_reset();
    pub fn sim_sdt_reset();

    // BPF map registry reset (implemented in scx_test_map.c).
    // Clears the thread-local map registration arrays to prevent
    // duplicate registrations on subsequent simulation runs.
    pub fn scx_test_map_clear_all();
}

// ---------------------------------------------------------------------------
// Safe wrappers for global-state FFI functions
// ---------------------------------------------------------------------------
//
// These wrap the raw `extern "C"` functions above so that engine.rs (and
// other safe modules) can call them without `unsafe` blocks. The safety
// invariants are documented once here rather than at every call site.
//
// The functions that accept raw `*mut c_void` parameters suppress the
// `clippy::not_unsafe_ptr_arg_deref` lint because they are *intentionally*
// safe wrappers: the `unsafe` block is encapsulated inside, and the engine
// guarantees that all raw pointers passed to these functions point to valid,
// live C structs (allocated via `sim_task_alloc` / `sim_cgroup_alloc`).

/// Reset global task-struct layout tables (lazy-init flags, etc.).
///
/// Must be called before each simulation run for deterministic behavior.
/// Safe because it only resets global statics in the linked C code.
pub fn reset_task_state() {
    // SAFETY: Resets global static variables in sim_task.c.
    // No pointers involved — just zeroing flags and tables.
    unsafe {
        sim_task_reset();
        sim_sdt_reset();
    }
}

/// Arm/disarm the `scx_task_alloc()` fault injector (scx GitHub #3564
/// reproducer).
///
/// Pass a task PID to make `scx_task_alloc()` return NULL for that task —
/// simulating the real BPF-arena / SDT-storage allocation failure that
/// drives `scx_lavd`'s `lavd_init_task` into
/// `scx_bpf_error("task_ctx_stor first lookup failed")` + `-ENOMEM`. Pass
/// 0 to disable (the default).
///
/// Test-only. Production runs never arm this, so `scx_task_alloc()`
/// behaves identically; the C-side check is inside the
/// `sim_rbc_pause()`/`resume()` window, so it is RBC/determinism-neutral
/// when disabled. Callers must hold the global sim lock (as all
/// simulator tests do).
pub fn set_task_alloc_fail_pid(pid: i32) {
    // SAFETY: `sim_sdt_fail_pid` is a plain C `int` global in
    // sim_sdt_stubs.c. Writes are serialized by the simulator's global
    // test lock; no pointers involved.
    unsafe { sim_sdt_fail_pid = pid }
}

/// Mark a CPU present in the all-CPUs cpumask.
pub fn cpumask_set_all(cpu: i32) {
    // SAFETY: Sets a bit in a global cpumask. The CPU index is
    // validated by the C code (no out-of-bounds if within NR_CPUS).
    unsafe { scx_test_set_all_cpumask(cpu) }
}

/// Mark a CPU as idle in the C idle cpumask.
pub fn cpumask_set_idle(cpu: i32) {
    // SAFETY: Sets a bit in a global cpumask.
    unsafe { scx_test_set_idle_cpumask(cpu) }
}

/// Mark a CPU as idle in the SMT-level idle cpumask.
pub fn cpumask_set_idle_smt(cpu: i32) {
    // SAFETY: Sets a bit in a global cpumask.
    unsafe { scx_test_set_idle_smtmask(cpu) }
}

/// Test and clear a CPU's idle bit. Returns true if the CPU was idle.
pub fn test_and_clear_cpu_idle(cpu: i32) -> bool {
    // SAFETY: Atomically tests and clears a bit in a global cpumask.
    unsafe { scx_bpf_test_and_clear_cpu_idle(cpu) }
}

/// Allocate a new idle task (PF_IDLE=0x2, mm=NULL).
///
/// Returns a raw pointer that must eventually be freed with
/// [`free_task_raw`].
pub fn alloc_idle_task() -> *mut c_void {
    // SAFETY: sim_task_alloc returns a heap-allocated, zeroed task_struct.
    // sim_task_set_flags sets a u32 field on the struct.
    unsafe {
        let p = sim_task_alloc();
        assert!(!p.is_null(), "sim_task_alloc returned null");
        sim_task_set_flags(p, 0x2); // PF_IDLE
        p
    }
}

/// Free a raw task_struct pointer.
///
/// # Safety
/// `p` must have been obtained from `sim_task_alloc` and not yet freed.
pub unsafe fn free_task_raw(p: *mut c_void) {
    sim_task_free(p);
}

/// Get a task's time slice from the raw C struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_get_slice(raw: *mut c_void) -> u64 {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    unsafe { sim_task_get_slice(raw) }
}

/// Set a task's time slice on the raw C struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_set_slice(raw: *mut c_void, slice: u64) {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    unsafe { sim_task_set_slice(raw, slice) }
}

/// Set `p->se.sum_exec_runtime` on a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_set_sum_exec_runtime(raw: *mut c_void, ns: u64) {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    unsafe { sim_task_set_sum_exec_runtime(raw, ns) }
}

/// Get `p->se.sum_exec_runtime` from a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_get_sum_exec_runtime(raw: *mut c_void) -> u64 {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    unsafe { sim_task_get_sum_exec_runtime(raw) }
}

/// Set the `mm` pointer on a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_set_mm(raw: *mut c_void, mm: *mut c_void) {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    // `mm` may be null (kernel threads).
    unsafe { sim_task_set_mm(raw, mm) }
}

/// Set the `real_parent` pointer on a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_set_real_parent(child: *mut c_void, parent: *mut c_void) {
    // SAFETY: Both pointers must be valid task_struct pointers.
    unsafe { sim_task_set_real_parent(child, parent) }
}

/// Set the cgroup pointer on a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_set_cgroup(raw: *mut c_void, cgrp: *mut c_void) {
    // SAFETY: `raw` must be a valid task_struct, `cgrp` a valid cgroup.
    unsafe { sim_task_set_cgroup(raw, cgrp) }
}

/// Get the cpus_ptr (cpumask) from a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_get_cpus_ptr(raw: *mut c_void) -> *const c_void {
    // SAFETY: The caller guarantees `raw` is a valid task_struct pointer.
    unsafe { sim_task_get_cpus_ptr(raw) }
}

/// Render a cpumask as a stable hex string with LSB = cpu 0.
///
/// tg `bundle-implement-secondary-tracekind-easy-wins` (TOP-7
/// `TraceKind::SetCpumask`): the live-vs-sim diff harness needs an
/// affinity representation that does not depend on the in-memory
/// cpumask layout. We emit a string of the form `0xNNNNNNNN…`, where
/// each 16-hex-digit word covers 64 CPUs and words are separated by
/// `_` from low to high — i.e. word 0 is bit 0..63, word 1 is bit
/// 64..127, etc. For the typical scxsim fixture (≤64 CPUs) the
/// output is one word like `0xff` or `0x1`.
///
/// The `cpumask` pointer can be NULL (treated as "all unset"). The
/// `nr_cpus` argument is the engine's `s.sim.cpus.len()` snapshot at
/// emit time — we never query the cpumask beyond it.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn cpumask_to_hex(cpumask: *const c_void, nr_cpus: u32) -> String {
    if cpumask.is_null() || nr_cpus == 0 {
        return "0x0".to_string();
    }
    // Pack into u64 words, LSB = cpu 0.
    let nr_words = (nr_cpus as usize).div_ceil(64);
    let mut words: Vec<u64> = vec![0; nr_words];
    for cpu in 0..nr_cpus {
        // SAFETY: `cpumask` was obtained from `sim_task_get_cpus_ptr`
        // which the engine's task-init path produces; it remains valid
        // for the lifetime of the task_struct. `bpf_cpumask_test_cpu`
        // only reads the bit at `cpu`, which is bounded by `nr_cpus`.
        let set = unsafe { bpf_cpumask_test_cpu(cpu, cpumask) };
        if set {
            let w = (cpu / 64) as usize;
            let b = cpu % 64;
            words[w] |= 1u64 << b;
        }
    }
    // Emit low-word first; for ≤64-CPU fixtures this collapses to a
    // single `0xN` token, matching what bpftrace prints.
    let mut s = String::with_capacity(2 + nr_words * 17);
    s.push_str("0x");
    let mut first = true;
    for w in words.iter().rev() {
        // Skip leading zero words to keep the common case compact.
        if first && *w == 0 && words.len() > 1 {
            continue;
        }
        if first {
            s.push_str(&format!("{:x}", w));
            first = false;
        } else {
            s.push_str(&format!("_{:016x}", w));
        }
    }
    if first {
        // All-zero cpumask: words.iter().rev() never produced output.
        s.push('0');
    }
    s
}

/// Set up cpumask pointer, clear it, set individual CPUs, and set
/// nr_cpus_allowed on a raw task_struct.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn task_setup_cpumask(raw: *mut c_void, allowed_cpus: Option<&[crate::types::CpuId]>) {
    // SAFETY: `raw` is a valid task_struct pointer.
    unsafe {
        sim_task_setup_cpus_ptr(raw);
        if let Some(cpus) = allowed_cpus {
            sim_task_clear_cpumask(raw);
            for cpu in cpus {
                sim_task_set_cpumask_cpu(raw, cpu.0 as i32);
            }
            sim_task_set_nr_cpus_allowed(raw, cpus.len() as i32);
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler trait
// ---------------------------------------------------------------------------

/// Trait that wraps a compiled scheduler's ops functions.
///
/// Each method corresponds to one of the sched_ext_ops callbacks.
/// Default implementations are no-ops for optional callbacks.
pub trait Scheduler {
    /// Initialize the scheduler (ops.init). Called once before simulation.
    /// # Safety
    /// Calls into C code.
    unsafe fn init(&self) -> i32;

    /// Select a CPU for a waking task (ops.select_cpu).
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn select_cpu(&self, p: *mut c_void, prev_cpu: i32, wake_flags: u64) -> i32;

    /// Enqueue a task (ops.enqueue).
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn enqueue(&self, p: *mut c_void, enq_flags: u64);

    /// Dispatch: CPU is looking for work (ops.dispatch).
    /// # Safety
    /// Calls into C code. `prev` may be null.
    unsafe fn dispatch(&self, cpu: i32, prev: *mut c_void);

    /// A task started running (ops.running).
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn running(&self, p: *mut c_void);

    /// A task stopped running (ops.stopping).
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn stopping(&self, p: *mut c_void, runnable: bool);

    /// Enable a task for scheduling (ops.enable). Called once per task.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn enable(&self, p: *mut c_void);

    /// A task was dequeued (ops.dequeue). Optional.
    /// Called when a task leaves the runnable state.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn dequeue(&self, _p: *mut c_void, _deq_flags: u64) {}

    /// A task went to sleep (ops.quiescent). Optional.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn quiescent(&self, _p: *mut c_void, _deq_flags: u64) {}

    /// A task became runnable (ops.runnable). Optional.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn runnable(&self, _p: *mut c_void, _enq_flags: u64) {}

    /// Initialize a task (ops.init_task). Called once per task at creation.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn init_task(&self, _p: *mut c_void) -> i32 {
        0
    }

    /// Initialize a task in a specific cgroup (ops.init_task with cgroup override).
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    /// `cgrp` must be a valid cgroup pointer.
    unsafe fn init_task_in_cgroup(&self, _p: *mut c_void, _cgrp: *mut c_void) -> i32 {
        0
    }

    /// A CPU was released by a higher scheduling class (ops.cpu_release).
    /// # Safety
    /// Calls into C code.
    unsafe fn cpu_release(&self, _cpu: i32, _args: *mut c_void) {}

    /// A CPU was acquired back from a higher scheduling class (ops.cpu_acquire).
    /// # Safety
    /// Calls into C code.
    unsafe fn cpu_acquire(&self, _cpu: i32, _args: *mut c_void) {}

    /// Scheduler is being unloaded (ops.exit). Called once during shutdown.
    /// # Safety
    /// Calls into C code.
    unsafe fn exit(&self) {}

    /// Fire a pending BPF timer callback for the given `slot`. Optional.
    /// `slot` is in the range `0..MAX_BPF_TIMERS` (currently 8).
    /// # Safety
    /// Calls into C code.
    unsafe fn fire_timer(&self, _slot: u32) {}

    /// Periodic tick on the current CPU (ops.tick). Optional.
    /// `p` is the currently running task.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn tick(&self, _p: *mut c_void) {}

    /// Notify scheduler of task cpumask change (ops.set_cpumask). Optional.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn set_cpumask(&self, _p: *mut c_void, _cpumask: *const c_void) {}

    /// Dump scheduler state for debugging (ops.dump). Optional.
    /// # Safety
    /// Calls into C code. `dctx` may be null.
    unsafe fn dump(&self, _dctx: *mut c_void) {}

    /// Dump per-task state for debugging (ops.dump_task). Optional.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer. `dctx` may be null.
    unsafe fn dump_task(&self, _dctx: *mut c_void, _p: *mut c_void) {}

    /// CPU idle state changed (ops.update_idle). Optional.
    /// Called when a CPU enters (idle=true) or exits (idle=false) the idle state.
    /// # Safety
    /// Calls into C code.
    unsafe fn update_idle(&self, _cpu: i32, _idle: bool) {}

    /// A task is exiting scheduling (ops.exit_task). Optional.
    /// Called once per task when it is being removed from SCX.
    /// # Safety
    /// Calls into C code. `p` must be a valid task_struct pointer.
    unsafe fn exit_task(&self, _p: *mut c_void) -> i32 {
        0
    }

    /// Initialize a cgroup (ops.cgroup_init). Optional.
    /// Called when a cgroup is created and the scheduler should track it.
    /// # Safety
    /// Calls into C code. `cgrp` must be a valid cgroup pointer.
    unsafe fn cgroup_init(&self, _cgrp: *mut c_void, _args: *mut c_void) -> i32 {
        0
    }

    /// Exit a cgroup (ops.cgroup_exit). Optional.
    /// Called when a cgroup is being destroyed.
    /// # Safety
    /// Calls into C code. `cgrp` must be a valid cgroup pointer.
    unsafe fn cgroup_exit(&self, _cgrp: *mut c_void) {}

    /// A task moved between cgroups (ops.cgroup_move). Optional.
    /// Called when a task is migrated from one cgroup to another.
    /// # Safety
    /// Calls into C code. All pointers must be valid.
    unsafe fn cgroup_move(&self, _p: *mut c_void, _from: *mut c_void, _to: *mut c_void) {}

    /// Cgroup bandwidth was configured (ops.cgroup_set_bandwidth). Optional.
    /// Called when cpu.max is written for a cgroup.
    /// # Safety
    /// Calls into C code. `cgrp` must be a valid cgroup pointer.
    unsafe fn cgroup_set_bandwidth(
        &self,
        _cgrp: *mut c_void,
        _period_us: u64,
        _quota_us: u64,
        _burst_us: u64,
    ) {
    }

    /// A CPU came online (ops.cpu_online). Optional.
    /// Called when a CPU transitions from offline to online.
    /// # Safety
    /// Calls into C code.
    unsafe fn cpu_online(&self, _cpu: i32) {}

    /// A CPU went offline (ops.cpu_offline). Optional.
    /// Called when a CPU transitions from online to offline.
    /// # Safety
    /// Calls into C code.
    unsafe fn cpu_offline(&self, _cpu: i32) {}

    /// Phase 2 Stage C (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
    /// Ask the scheduler whether `cgrp_id` is throttled by `cpu.max`.
    /// Returns `Some(true)` / `Some(false)` if the scheduler links the
    /// cgroup_bw library and answered; `None` if the scheduler does not
    /// model cgroup_bw at all (e.g. simple, tickless).
    ///
    /// The engine's DSQ-pop admission gate (`pid_is_bw_throttled`)
    /// consults this to make the library the single source of truth
    /// for throttle state -- replacing the engine-side
    /// `BandwidthManager::is_throttled` direct read.
    fn is_cgroup_throttled(&self, _cgrp_id: u64) -> Option<bool> {
        None
    }

    /// Phase 2 Stage E diagnostic probe. Returns `None` when the
    /// loaded scheduler does not expose `scxsim_probe_cbw_state` (any
    /// scheduler that does not link cgroup_bw -- e.g. simple,
    /// tickless). When present, fills `out` with the library's
    /// internal state for `(cgrp_id, llc_id)` and returns a libc-style
    /// integer return code (0 success, negative errno).
    fn probe_cbw_state(
        &self,
        _cgrp_id: u64,
        _llc_id: i32,
        _out: &mut CbwProbeResult,
    ) -> Option<i32> {
        None
    }

    /// Library-driven slice-cap budget query. Returns the cgroup's
    /// remaining cpu.max budget for the current period in
    /// nanoseconds, or `u64::MAX` (the wrapper.c sentinel for
    /// unknown / untracked / unlimited) when no cap should apply.
    /// Returns `None` when the scheduler does not link the cgroup_bw
    /// library at all (engine treats as "no cap" -- the only honest
    /// answer for a scheduler that doesn't model cpu.max).
    fn cgroup_bw_budget_remaining(&self, _cgrp_id: u64) -> Option<u64> {
        None
    }

    /// Snapshot the cgroup_bw library state for a single cgroup
    /// identified by its RAW cgrp pointer (sourced from scxsim's
    /// `cgroup_registry`, NOT looked up via `bpf_cgroup_from_id`
    /// inside the lib). The `cgid` argument is purely informational
    /// (copied into `out->cgid` for downstream identification).
    ///
    /// Returns `None` when the loaded scheduler does not link the
    /// cgroup_bw library; `Some(rc)` otherwise:
    ///   *  0  → success, `out` filled
    ///   * -1  → caller error (NULL cgrp_raw or out)
    ///   * -2  → cgroup not registered with the lib's CGRP_STORAGE
    ///   * -3  → unlimited-quota cgroup (caller should skip)
    ///
    /// Why raw-pointer entry: the engine holds the SIM_ARC mutex at
    /// the snapshot points and `bpf_cgroup_from_id` -> `try_lock()`
    /// fails inside that critical section, returning the root cgroup
    /// for every cgid. Passing the raw cgrp pointer in directly
    /// bypasses that lookup.
    ///
    /// tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`.
    fn snapshot_by_raw_cgrp(
        &self,
        _cgid: u64,
        _cgrp_raw: *mut c_void,
        _out: &mut CbwCgroupSnapshot,
    ) -> Option<i32> {
        None
    }

    /// Resolve e9patch C trampoline function pointers from the loaded library.
    ///
    /// Returns `None` by default (no e9patch support). `DynamicScheduler`
    /// overrides this to probe the loaded `.so` for the trampoline symbols.
    fn resolve_e9_fns(&self) -> Option<crate::backend::e9patch::E9PatchFns> {
        None
    }

    /// Return debugger metadata for `--wait-debugger` support.
    ///
    /// The default implementation returns `None` (no debugger info available).
    /// `DynamicScheduler` overrides this with the actual `.so` path, symbol
    /// prefix, and list of defined ops symbol names.
    fn debugger_info(&self) -> Option<DebuggerInfo> {
        None
    }
}

/// Debugger metadata for a loaded scheduler, used by `--wait-debugger`.
///
/// Contains the information needed to generate debugger breakpoint scripts
/// (`.lldb` and `.gdb`): the `.so` file path, the symbol prefix (e.g.
/// "simple"), and the list of ops callback symbol names present in the
/// loaded scheduler.
pub struct DebuggerInfo {
    /// Absolute path to the loaded `.so` file.
    pub so_path: String,
    /// Symbol prefix (e.g. "simple" for `simple_init`, `simple_enqueue`).
    pub prefix: String,
    /// Symbol names for all defined ops callbacks (e.g. `["simple_init", ...]`).
    pub ops_symbol_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// LAVD power mode
// ---------------------------------------------------------------------------

/// LAVD power mode settings, matching the scx_lavd CLI flags.
///
/// # Warning: Duplicated Logic
///
/// The C wrapper (`schedulers/lavd/wrapper.c`) duplicates power mode logic
/// from the real scx_lavd userspace. If LAVD's behavior changes, the wrapper
/// must be updated to match.
///
/// **Source of truth** (check these if behavior seems wrong):
/// - `scheds/rust/scx_lavd/src/main.rs`: `Opts::proc()`, `init_globals()`
/// - `scheds/rust/scx_lavd/src/bpf/power.bpf.c`: `do_set_power_profile()`
/// - `scheds/rust/scx_lavd/src/bpf/intf.h`: `LAVD_PM_*` constants
///
/// These correspond to the LAVD_PM_* constants in intf.h:
/// - `LAVD_PM_PERFORMANCE = 0`
/// - `LAVD_PM_BALANCED = 1`
/// - `LAVD_PM_POWERSAVE = 2`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LavdPowerMode {
    /// Performance mode: no core compaction, maximum throughput.
    /// Equivalent to `--performance` flag.
    Performance = 0,
    /// Balanced mode: core compaction enabled, moderate power saving.
    /// Equivalent to `--balanced` flag.
    Balanced = 1,
    /// Powersave mode: aggressive core compaction, maximum power saving.
    /// Equivalent to `--powersave` flag.
    Powersave = 2,
}

// ---------------------------------------------------------------------------
// DynamicScheduler — loads scheduler .so via libloading
// ---------------------------------------------------------------------------

/// Function pointer types for scheduler ops.
type InitFn = unsafe extern "C" fn() -> i32;
type SelectCpuFn = unsafe extern "C" fn(*mut c_void, i32, u64) -> i32;
type EnqueueFn = unsafe extern "C" fn(*mut c_void, u64);
type DispatchFn = unsafe extern "C" fn(i32, *mut c_void);
type RunningFn = unsafe extern "C" fn(*mut c_void);
type StoppingFn = unsafe extern "C" fn(*mut c_void, bool);
type EnableFn = unsafe extern "C" fn(*mut c_void);
type RunnableFn = unsafe extern "C" fn(*mut c_void, u64);
type InitTaskFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type CpuReleaseFn = unsafe extern "C" fn(i32, *mut c_void);
type ExitFn = unsafe extern "C" fn(*mut c_void);
type SetupFn = unsafe extern "C" fn(u32);
type FireTimerFn = unsafe extern "C" fn(u32);
type QuiescentFn = unsafe extern "C" fn(*mut c_void, u64);
type DequeueFn = unsafe extern "C" fn(*mut c_void, u64);
type TickFn = unsafe extern "C" fn(*mut c_void);
type SetCpumaskFn = unsafe extern "C" fn(*mut c_void, *const c_void);
type DumpFn = unsafe extern "C" fn(*mut c_void);
type DumpTaskFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
type UpdateIdleFn = unsafe extern "C" fn(i32, bool);
type ExitTaskFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type CgroupInitFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type CgroupExitFn = unsafe extern "C" fn(*mut c_void);
type CgroupMoveFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type CpuAcquireFn = unsafe extern "C" fn(i32, *mut c_void);
/*
 * Phase 2 Stage C (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
 * type for `scx_cgroup_bw_is_cgroup_throttled(u64 cgrp_id) -> int`.
 * NOT prefixed by scheduler name -- the symbol comes from
 * `scx/lib/cgroup_bw.bpf.c` (when Phase 2 ON) or from wrapper.c's
 * compatibility forwarder (when Phase 2 OFF -- delegates to
 * `sim_cgroup_bw_is_cgroup_throttled` which reads the engine
 * BandwidthManager). Either way the symbol exists in the .so and
 * returns the right answer.
 */
type IsCgroupThrottledFn = unsafe extern "C" fn(u64) -> i32;
/*
 * Phase 2 Stage E diagnostic probe (tg
 * `investigate-scxsim-engine-throttles-before-scheduler-cgroup-bw`):
 * `int scxsim_probe_cbw_state(u64 cgrp_id, int llc_id, struct ProbeResult *out)`
 * exported by wrapper.c. Callers pass a `ProbeResult` buffer that
 * mirrors `struct scxsim_cbw_probe_result` in wrapper.c byte-for-byte.
 */
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct CbwProbeResult {
    pub cgx: *mut c_void,
    pub llcx_via_helper: *mut c_void,
    pub llcx_via_direct_map: *mut c_void,
    pub cgrp_id_seen: u64,
    pub has_llcx: i32,
    pub is_throttled: i32,
    pub runtime_total_sloppy: i64,
    pub runtime_total_in_llcx: i64,
    pub consumed_count_pre: i64,
    pub cgrp_ptr: *mut c_void,
    pub cbw_cgrp_map_nr: i32,
    pub cbw_cgrp_map_first_key: *mut c_void,
    pub cbw_cgrp_llc_map_nr: i32,
}
type ProbeCbwStateFn = unsafe extern "C" fn(u64, i32, *mut CbwProbeResult) -> i32;
/*
 * tg `shrink-rust-bandwidthmanager-518-to-30-lines-no-fake-approximation`:
 * `unsigned long long scxsim_cgroup_bw_budget_remaining(u64 cgrp_id)`
 * exported by wrapper.c. Returns the cgroup's remaining cpu.max budget
 * (period_budget - runtime_total_sloppy) in nanoseconds, or
 * `(u64)-1 = u64::MAX` when the cgroup is unknown / untracked /
 * unlimited (no cap should apply). Engine reads it from
 * `pid_bw_max_run_ns` to cap the scheduler-chosen slice.
 */
type CgroupBwBudgetRemainingFn = unsafe extern "C" fn(u64) -> u64;

/*
 * tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`: snapshot
 * of every finite-quota cgroup's library-internal cgroup_bw state. Mirrors
 * `struct scxsim_cbw_cgroup_snapshot` in `schedulers/lavd/wrapper.c`
 * byte-for-byte. The engine uses BEFORE/AFTER snapshots around each
 * `fire_timer` invocation to detect per-cgroup replenishments and emit
 * `TraceKind::CgroupBwReplenish` events with computed debt + burst credit.
 *
 * Field layout: 6 * 8 (i64/u64) + 2 * 4 (i32/i32 padding) = 56 bytes,
 * matching the C struct.
 */
#[repr(C)]
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CbwCgroupSnapshot {
    /// Cgroup id (`cgx->id`).
    pub cgid: u64,
    /// `cgx->runtime_total_last`. The lib's input to debt/burst-credit.
    pub runtime_total_last: i64,
    /// `cgx->period_budget`. Output of the previous replenishment.
    pub period_budget: i64,
    /// `cgx->burst_remaining`. Lib uses this to clamp burst_credit.
    pub burst_remaining: i64,
    /// `cgx->nquota`. Static (set at cgroup_set_bandwidth).
    pub nquota: i64,
    /// `cgx->nquota_ub`. Effective per-period quota; CBW_RUNTUME_INF
    /// for unlimited cgroups (those are skipped by the snapshotter).
    pub nquota_ub: i64,
    /// `cgx->is_throttled` (0/1). Reflects whether the cgroup is
    /// currently in a throttled state.
    pub is_throttled: i32,
    /// Aggregate Backup-Task-Queue length across all LLC contexts for
    /// this cgroup (`sum(scx_atq_nr_queued(llcx->btq))`). Sentinel
    /// `-1` means the lib has no LLC ctx for this cgroup so the BTQ
    /// length cannot be read; the Rust diff helper treats this as
    /// "BTQ unknown" and emits no `CbwPutAside` / `CbwDrainBtqBatch`
    /// event for the cgroup. tg
    /// `add-cbw-put-aside-and-drain-btq-batch-tracekinds`
    /// (A1+A2 from cgroup_bw audit). Replaces the prior `_pad` field
    /// (same byte offset, same size — 4 bytes — so the C struct
    /// layout is unchanged).
    pub btq_total_len: i32,
}
/// `int scxsim_cbw_snapshot_by_raw_cgrp(u64 cgid, void *cgrp_raw,
/// struct *out)`. Returns 0 on success (out filled), -1/-2 if the
/// cgroup is not registered in the lib, -3 if the cgroup has unlimited
/// quota (caller skips). The caller (Rust engine) sources both `cgid`
/// AND `cgrp_raw` from scxsim's `cgroup_registry`. Passing the raw
/// cgrp pointer in directly sidesteps two issues:
///
///   1. `cbw_cgroup_ids[]` (lib-internal) is only populated transiently
///      inside `replenish_timerfn`, so iterating it from outside the
///      timer is unreliable.
///   2. `bpf_cgroup_from_id` -> `sim_cgroup_lookup_by_id` uses
///      `try_lock()` on the SIM_ARC mutex, which the engine already
///      holds at the BEFORE/AFTER snapshot points. The lock fails,
///      lookup returns the root cgroup pointer for every cgid, and
///      every per-cgid snapshot collapses to the same cgx.
type SnapshotByRawCgrpFn = unsafe extern "C" fn(u64, *mut c_void, *mut CbwCgroupSnapshot) -> i32;

type CgroupSetBandwidthFn = unsafe extern "C" fn(*mut c_void, u64, u64, u64);
type CpuOnlineFn = unsafe extern "C" fn(i32);
type CpuOfflineFn = unsafe extern "C" fn(i32);

/// Resolved function pointers for a scheduler's ops.
struct SchedOps {
    init: InitFn,
    select_cpu: SelectCpuFn,
    enqueue: EnqueueFn,
    dispatch: DispatchFn,
    running: RunningFn,
    stopping: StoppingFn,
    enable: Option<EnableFn>,
    runnable: Option<RunnableFn>,
    init_task: Option<InitTaskFn>,
    cpu_release: Option<CpuReleaseFn>,
    exit: Option<ExitFn>,
    fire_timer: Option<FireTimerFn>,
    quiescent: Option<QuiescentFn>,
    dequeue: Option<DequeueFn>,
    tick: Option<TickFn>,
    set_cpumask: Option<SetCpumaskFn>,
    dump: Option<DumpFn>,
    dump_task: Option<DumpTaskFn>,
    update_idle: Option<UpdateIdleFn>,
    exit_task: Option<ExitTaskFn>,
    cgroup_init: Option<CgroupInitFn>,
    cgroup_exit: Option<CgroupExitFn>,
    cgroup_move: Option<CgroupMoveFn>,
    cgroup_set_bandwidth: Option<CgroupSetBandwidthFn>,
    cpu_acquire: Option<CpuAcquireFn>,
    cpu_online: Option<CpuOnlineFn>,
    cpu_offline: Option<CpuOfflineFn>,
    /*
     * Phase 2 Stage C: cross-binary call into the .so's
     * `scx_cgroup_bw_is_cgroup_throttled(u64 cgrp_id) -> int`. Loaded
     * via dlsym (NOT prefixed with the scheduler name -- it's a
     * library symbol provided by `scx/lib/cgroup_bw.bpf.c` when
     * Phase 2 ON, or by wrapper.c's compatibility forwarder when
     * Phase 2 OFF). Engine `pid_is_bw_throttled` consults this so the
     * library is the single source of truth for throttle state.
     * `None` for schedulers (e.g. simple, tickless) that don't link
     * the cgroup_bw library at all.
     */
    is_cgroup_throttled: Option<IsCgroupThrottledFn>,
    /*
     * tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`:
     * `scxsim_cbw_snapshot_one_cgroup(cgid, out)` exported by the LAVD
     * wrapper.c. `None` when the loaded scheduler does not compile the
     * cgroup_bw library in (e.g. simple, tickless, mitosis, cosmos).
     * The engine sources cgids from `cgroup_registry` and queries one
     * at a time so we don't depend on the lib-internal
     * `cbw_cgroup_ids[]` array (which is only populated transiently
     * inside replenish_timerfn).
     */
    snapshot_by_raw_cgrp: Option<SnapshotByRawCgrpFn>,
    /*
     * Phase 2 Stage E diagnostic: `scxsim_probe_cbw_state`.
     * Optional -- only present when wrapper.c is built with the probe
     * exported (which is unconditional today, but defensive None for
     * any future scheduler that does not link cgroup_bw).
     */
    probe_cbw_state: Option<ProbeCbwStateFn>,
    /*
     * Library-driven slice-cap budget query
     * (`scxsim_cgroup_bw_budget_remaining`). `None` for schedulers
     * that do not link the cgroup_bw library (simple, tickless) -- in
     * that case the engine treats the cgroup as having no bandwidth
     * cap (the only honest answer when the scheduler does not model
     * cpu.max).
     */
    bw_budget_remaining: Option<CgroupBwBudgetRemainingFn>,
}

/// Metadata about a discovered scheduler .so file.
pub struct SchedulerInfo {
    /// Scheduler name derived from the filename (e.g., "simple").
    pub name: String,
    /// Full path to the .so file.
    pub path: std::path::PathBuf,
}

/// Scan a directory for `libscx_*.so` files and return metadata for each.
///
/// Does NOT load the .so files — just discovers them. Loading happens
/// on demand via `DynamicScheduler::load`.
pub fn discover_schedulers(dir: &Path) -> Vec<SchedulerInfo> {
    let mut schedulers = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return schedulers,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(sched_name) = name
            .strip_prefix("libscx_")
            .and_then(|s| s.strip_suffix(".so"))
        {
            schedulers.push(SchedulerInfo {
                name: sched_name.to_owned(),
                path,
            });
        }
    }
    schedulers.sort_by(|a, b| a.name.cmp(&b.name));
    schedulers
}

/// A scheduler loaded dynamically from a `.so` shared library.
///
/// Each instance owns a `libloading::Library` handle. When the
/// `DynamicScheduler` is dropped, the library is unloaded via `dlclose`,
/// resetting all global state in the scheduler C code.
pub struct DynamicScheduler {
    /// Keep the library alive so function pointers remain valid.
    _lib: libloading::Library,
    ops: SchedOps,
    /// Symbol prefix (e.g. "simple") used to form ops symbol names.
    prefix: String,
    /// Absolute path to the loaded `.so` file.
    so_path: String,
}

impl DynamicScheduler {
    /// Look up a symbol in the loaded scheduler `.so`.
    ///
    /// Returns `None` if the symbol is not found. The returned `Symbol`
    /// borrows `self`, keeping the library alive.
    ///
    /// # Safety
    /// The caller must ensure `T` matches the actual symbol's type.
    pub unsafe fn get_symbol<T>(&self, name: &[u8]) -> Option<libloading::Symbol<'_, T>> {
        self._lib.get(name).ok()
    }

    /// Load a scheduler from a `.so` file.
    ///
    /// - `path`: path to the `.so` file
    /// - `prefix`: symbol prefix (e.g., "simple" or "tickless")
    /// - `nr_cpus`: passed to `{prefix}_setup()` if the symbol exists
    ///
    /// Mandatory ops (`init`, `select_cpu`, etc.) panic if missing.
    /// Optional ops (`runnable`, `init_task`) become `None` if missing.
    pub fn load(path: &str, prefix: &str, nr_cpus: u32) -> Self {
        // SAFETY: The .so is built by our build system from known-safe C source.
        // Use RTLD_NOW for eager binding so all PLT entries are resolved at
        // load time. Without this, lazy PLT resolution during simulation adds
        // variable dynamic-linker branches that perturb the RBC counter.
        let lib: libloading::Library = unsafe {
            libloading::os::unix::Library::open(
                Some(path),
                libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
            )
        }
        .unwrap_or_else(|e| panic!("failed to load {path}: {e}"))
        .into();

        // Probe for {prefix}_setup -- call it if present
        // SAFETY: The `.so` was loaded with RTLD_NOW. If the symbol exists,
        // it has the expected `SetupFn` signature (from our build system).
        unsafe {
            let setup_sym = format!("{prefix}_setup");
            if let Ok(sym) = lib.get::<SetupFn>(setup_sym.as_bytes()) {
                let setup_fn: SetupFn = *sym;
                setup_fn(nr_cpus);
            }
        }

        // SAFETY: The library contains the expected ops symbols with
        // correct signatures (built by our build system).
        let ops = unsafe { Self::load_ops(&lib, prefix) };
        Self {
            _lib: lib,
            ops,
            prefix: prefix.to_owned(),
            so_path: path.to_owned(),
        }
    }

    /// Load the scx_simple scheduler.
    pub fn simple() -> Self {
        let dir = env!("SCHEDULER_SO_DIR");
        Self::load(&format!("{dir}/libscx_simple.so"), "simple", 1)
    }

    /// Load the scx_tickless scheduler, configured for `nr_cpus` CPUs.
    pub fn tickless(nr_cpus: u32) -> Self {
        let dir = env!("SCHEDULER_SO_DIR");
        Self::load(&format!("{dir}/libscx_tickless.so"), "tickless", nr_cpus)
    }

    /// Load the scx_cosmos scheduler, configured for `nr_cpus` CPUs.
    pub fn cosmos(nr_cpus: u32) -> Self {
        let dir = env!("SCHEDULER_SO_DIR");
        Self::load(&format!("{dir}/libscx_cosmos.so"), "cosmos", nr_cpus)
    }

    /// Load the scx_mitosis scheduler, configured for `nr_cpus` CPUs.
    ///
    /// Mitosis is a dynamic affinity scheduler that assigns cgroups to
    /// cells with discrete CPU sets. In the simulator, all tasks belong
    /// to the root cgroup (cell 0).
    pub fn mitosis(nr_cpus: u32) -> Self {
        let dir = env!("SCHEDULER_SO_DIR");
        Self::load(&format!("{dir}/libscx_mitosis.so"), "mitosis", nr_cpus)
    }

    /// Load the scx_lavd scheduler, configured for `nr_cpus` CPUs.
    ///
    /// LAVD (Latency-criticality Aware Virtual Deadline) is a production
    /// scheduler that combines virtual deadline ordering with latency
    /// criticality tracking. In the simulator, complex features like
    /// cgroup bandwidth, autopilot, and core compaction are disabled.
    pub fn lavd(nr_cpus: u32) -> Self {
        let dir = env!("SCHEDULER_SO_DIR");
        Self::load(&format!("{dir}/libscx_lavd.so"), "lavd", nr_cpus)
    }

    /// Load the scx_lavd scheduler with multiple compute domains.
    ///
    /// CPUs are split evenly across `nr_domains` domains, each as a
    /// neighbor of all others. This enables cross-domain migration code
    /// paths in balance.bpf.c (`plan_x_cpdom_migration`,
    /// `try_to_steal_task`, `force_to_steal_task`).
    ///
    /// `nr_cpus` must be >= `nr_domains` and `nr_domains` must be >= 2.
    pub fn lavd_multi_domain(nr_cpus: u32, nr_domains: u32) -> Self {
        assert!(nr_domains >= 2, "need at least 2 domains");
        assert!(
            nr_cpus >= nr_domains,
            "nr_cpus ({nr_cpus}) must be >= nr_domains ({nr_domains})"
        );
        let sched = Self::lavd(nr_cpus);
        type SetupMultiDomainFn = unsafe extern "C" fn(u32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        // The function expects a u32 domain count.
        unsafe {
            let sym: libloading::Symbol<SetupMultiDomainFn> = sched
                ._lib
                .get(b"lavd_setup_multi_domain")
                .expect("lavd_setup_multi_domain not found");
            (sym)(nr_domains);
        }
        sched
    }

    /// Configure a LAVD scheduler's DSQ and migration mode.
    ///
    /// Must be called after construction and before `Simulator::run()`.
    /// - `per_cpu_dsq`: use per-CPU DSQs (enables `is_per_cpu_dsq_migratable`)
    /// - `pinned_slice_ns`: if non-zero, enables dual-DSQ mode (both per-CPU
    ///   and per-cpdom DSQs with vtime comparison)
    /// - `mig_delta_pct`: if non-zero, uses fixed migration threshold
    ///   percentage instead of dynamic `calc_mig_delta`
    pub fn lavd_configure(&self, per_cpu_dsq: bool, pinned_slice_ns: u64, mig_delta_pct: u8) {
        type SetU32Fn = unsafe extern "C" fn(u32);
        type SetU64Fn = unsafe extern "C" fn(u64);
        // SAFETY: Symbols resolved from a `.so` built by our build system.
        // The functions expect the declared parameter types.
        unsafe {
            let sym: libloading::Symbol<SetU32Fn> = self
                ._lib
                .get(b"lavd_set_per_cpu_dsq")
                .expect("lavd_set_per_cpu_dsq not found");
            (sym)(per_cpu_dsq as u32);

            let sym: libloading::Symbol<SetU64Fn> = self
                ._lib
                .get(b"lavd_set_pinned_slice_ns")
                .expect("lavd_set_pinned_slice_ns not found");
            (sym)(pinned_slice_ns);

            let sym: libloading::Symbol<SetU32Fn> = self
                ._lib
                .get(b"lavd_set_mig_delta_pct")
                .expect("lavd_set_mig_delta_pct not found");
            (sym)(mig_delta_pct as u32);
        }
    }

    /// Enable or disable LAVD's `is_monitored` flag.
    ///
    /// When enabled, `consume_dsq()` measures DSQ consume latency
    /// using `bpf_ktime_get_ns()`. This exercises the monitoring
    /// instrumentation paths in balance.bpf.c.
    pub fn lavd_set_monitored(&self, monitored: bool) {
        type SetU32Fn = unsafe extern "C" fn(u32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<SetU32Fn> = self
                ._lib
                .get(b"lavd_set_is_monitored")
                .expect("lavd_set_is_monitored not found");
            (sym)(monitored as u32);
        }
    }

    /// Enable or disable LAVD's `no_core_compaction` flag.
    ///
    /// When core compaction is enabled (`no_core_compaction = false`),
    /// `do_core_compaction()` can deactivate CPUs in domains, creating
    /// overflow domains that prevent the balanced load path from
    /// triggering. Disabling core compaction keeps all domains active.
    pub fn lavd_set_no_core_compaction(&self, no_compact: bool) {
        type SetU32Fn = unsafe extern "C" fn(u32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<SetU32Fn> = self
                ._lib
                .get(b"lavd_set_no_core_compaction")
                .expect("lavd_set_no_core_compaction not found");
            (sym)(no_compact as u32);
        }
    }

    /// Set LAVD power mode (performance, balanced, or powersave).
    ///
    /// This mimics the effect of the `--performance`, `--balanced`, and
    /// `--powersave` CLI flags in the real scx_lavd scheduler.
    ///
    /// Must be called after construction and before `Simulator::run()`.
    pub fn lavd_set_power_mode(&self, mode: LavdPowerMode) {
        type SetPowerModeFn = unsafe extern "C" fn(i32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<SetPowerModeFn> = self
                ._lib
                .get(b"lavd_set_power_mode")
                .expect("lavd_set_power_mode not found");
            (sym)(mode as i32);
        }
    }

    /// Enable or disable LAVD autopilot mode.
    ///
    /// When autopilot is enabled, the scheduler dynamically switches between
    /// power modes based on system load. Autopilot starts in balanced mode.
    ///
    /// This mimics the effect of the `--autopilot` CLI flag.
    ///
    /// Must be called after construction and before `Simulator::run()`.
    pub fn lavd_set_autopilot(&self, on: bool) {
        type SetAutopilotFn = unsafe extern "C" fn(i32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<SetAutopilotFn> = self
                ._lib
                .get(b"lavd_set_autopilot")
                .expect("lavd_set_autopilot not found");
            (sym)(on as i32);
        }
    }

    /// Reset LAVD to "vanilla" state with no special flags.
    ///
    /// This sets:
    /// - Power mode to balanced
    /// - Autopilot off
    /// - Core compaction enabled (no_core_compaction = false)
    ///
    /// Use this to test LAVD with default settings rather than the
    /// performance-optimized defaults from `lavd_setup()`.
    pub fn lavd_noflags(&self) {
        self.lavd_set_power_mode(LavdPowerMode::Balanced);
        self.lavd_set_autopilot(false);
        self.lavd_set_no_core_compaction(false);
    }

    /// Set the maximum number of cgroups for cgroup_bw exhaustion simulation.
    ///
    /// When `max > 0`, `scx_cgroup_bw_init()` returns `-ENOMEM` once the count
    /// exceeds this limit. This simulates `CBW_NR_CGRP_MAX = 2048` from production.
    ///
    /// Set to `0` to disable the limit (default).
    pub fn lavd_set_cgroup_bw_max(&self, max: u32) {
        type SetU32Fn = unsafe extern "C" fn(u32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<SetU32Fn> = self
                ._lib
                .get(b"lavd_set_cgroup_bw_max")
                .expect("lavd_set_cgroup_bw_max not found");
            (sym)(max);
        }
    }

    /// Get the current cgroup_bw count (number of active cgroups tracked).
    pub fn lavd_get_cgroup_bw_count(&self) -> u32 {
        type GetU32Fn = unsafe extern "C" fn() -> u32;
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<GetU32Fn> = self
                ._lib
                .get(b"lavd_get_cgroup_bw_count")
                .expect("lavd_get_cgroup_bw_count not found");
            (sym)()
        }
    }

    /// Load the scx_cosmos scheduler with NUMA topology.
    ///
    /// CPUs are grouped sequentially into `nr_nodes` NUMA nodes.
    /// `nr_cpus` must be divisible by `nr_nodes`.
    pub fn cosmos_with_numa(nr_cpus: u32, nr_nodes: u32) -> Self {
        assert!(nr_nodes > 0);
        assert!(nr_cpus >= nr_nodes);
        assert!(
            nr_cpus.is_multiple_of(nr_nodes),
            "nr_cpus ({nr_cpus}) must be divisible by nr_nodes ({nr_nodes})"
        );
        let sched = Self::cosmos(nr_cpus);
        // Call cosmos_configure_numa in the loaded .so
        type ConfigureNumaFn = unsafe extern "C" fn(u32, u32);
        // SAFETY: Symbol resolved from a `.so` built by our build system.
        unsafe {
            let sym: libloading::Symbol<ConfigureNumaFn> = sched
                ._lib
                .get(b"cosmos_configure_numa")
                .expect("cosmos_configure_numa not found");
            (sym)(nr_cpus, nr_nodes);
        }
        sched
    }

    /// Look up scheduler ops function pointers from the loaded library.
    ///
    /// Mandatory symbols panic if missing. Optional symbols become `None`.
    ///
    /// # Safety
    /// The library must contain the expected symbols with correct signatures.
    unsafe fn load_ops(lib: &libloading::Library, prefix: &str) -> SchedOps {
        macro_rules! get {
            ($name:expr) => {{
                let sym_name = format!("{}_{}", prefix, $name);
                let sym: libloading::Symbol<*const ()> = lib
                    .get(sym_name.as_bytes())
                    .unwrap_or_else(|e| panic!("{sym_name} not found: {e}"));
                // Copy the raw pointer out — it's valid as long as _lib lives.
                *sym
            }};
        }

        macro_rules! try_get {
            ($name:expr) => {{
                let sym_name = format!("{}_{}", prefix, $name);
                lib.get::<*const ()>(sym_name.as_bytes())
                    .ok()
                    .map(|sym| *sym)
            }};
        }

        SchedOps {
            init: std::mem::transmute::<*const (), InitFn>(get!("init")),
            select_cpu: std::mem::transmute::<*const (), SelectCpuFn>(get!("select_cpu")),
            enqueue: std::mem::transmute::<*const (), EnqueueFn>(get!("enqueue")),
            dispatch: std::mem::transmute::<*const (), DispatchFn>(get!("dispatch")),
            running: std::mem::transmute::<*const (), RunningFn>(get!("running")),
            stopping: std::mem::transmute::<*const (), StoppingFn>(get!("stopping")),
            enable: try_get!("enable").map(|p| std::mem::transmute::<*const (), EnableFn>(p)),
            runnable: try_get!("runnable").map(|p| std::mem::transmute::<*const (), RunnableFn>(p)),
            init_task: try_get!("init_task")
                .map(|p| std::mem::transmute::<*const (), InitTaskFn>(p)),
            cpu_release: try_get!("cpu_release")
                .map(|p| std::mem::transmute::<*const (), CpuReleaseFn>(p)),
            exit: try_get!("exit").map(|p| std::mem::transmute::<*const (), ExitFn>(p)),
            fire_timer: try_get!("fire_timer")
                .map(|p| std::mem::transmute::<*const (), FireTimerFn>(p)),
            quiescent: try_get!("quiescent")
                .map(|p| std::mem::transmute::<*const (), QuiescentFn>(p)),
            dequeue: try_get!("dequeue").map(|p| std::mem::transmute::<*const (), DequeueFn>(p)),
            tick: try_get!("tick").map(|p| std::mem::transmute::<*const (), TickFn>(p)),
            set_cpumask: try_get!("set_cpumask")
                .map(|p| std::mem::transmute::<*const (), SetCpumaskFn>(p)),
            dump: try_get!("dump").map(|p| std::mem::transmute::<*const (), DumpFn>(p)),
            dump_task: try_get!("dump_task")
                .map(|p| std::mem::transmute::<*const (), DumpTaskFn>(p)),
            update_idle: try_get!("update_idle")
                .map(|p| std::mem::transmute::<*const (), UpdateIdleFn>(p)),
            exit_task: try_get!("exit_task")
                .map(|p| std::mem::transmute::<*const (), ExitTaskFn>(p)),
            cgroup_init: try_get!("cgroup_init")
                .map(|p| std::mem::transmute::<*const (), CgroupInitFn>(p)),
            cgroup_exit: try_get!("cgroup_exit")
                .map(|p| std::mem::transmute::<*const (), CgroupExitFn>(p)),
            cgroup_move: try_get!("cgroup_move")
                .map(|p| std::mem::transmute::<*const (), CgroupMoveFn>(p)),
            cgroup_set_bandwidth: try_get!("cgroup_set_bandwidth")
                .map(|p| std::mem::transmute::<*const (), CgroupSetBandwidthFn>(p)),
            cpu_acquire: try_get!("cpu_acquire")
                .map(|p| std::mem::transmute::<*const (), CpuAcquireFn>(p)),
            cpu_online: try_get!("cpu_online")
                .map(|p| std::mem::transmute::<*const (), CpuOnlineFn>(p)),
            cpu_offline: try_get!("cpu_offline")
                .map(|p| std::mem::transmute::<*const (), CpuOfflineFn>(p)),
            // Phase 2 Stage E (tg `investigate-scxsim-engine-throttles-
            // before-scheduler-cgroup-bw`): the production cgroup_bw
            // library declares its public entry points with `__hidden`
            // (visibility("hidden")), so they are NOT reachable via
            // dlsym. The cleanest path is to dlsym wrapper.c's
            // default-visibility forwarder `scxsim_cgroup_bw_is_cgroup_
            // throttled`, which internally calls the still-`__hidden`
            // library function from inside the same translation unit.
            //
            // The historical probe for `cbw_alloc_llc_ctx` (a library
            // symbol that was supposed to indicate Phase 2 ON) became
            // unreliable after the library inlined that helper away,
            // and was redundant once the wrapper provides a stable
            // `scxsim_*` re-export name we can probe directly: if the
            // wrapper exports the forwarder, Phase 2 is ON; if not,
            // Phase 2 is OFF and we fall back to the engine
            // `BandwidthManager`. See the wrapper.c "Stage E" block
            // for the full forwarder set + diagnostic counter.
            is_cgroup_throttled: lib
                .get::<*const ()>(b"scxsim_cgroup_bw_is_cgroup_throttled")
                .ok()
                .map(|sym| std::mem::transmute::<*const (), IsCgroupThrottledFn>(*sym)),
            snapshot_by_raw_cgrp: lib
                .get::<*const ()>(b"scxsim_cbw_snapshot_by_raw_cgrp")
                .ok()
                .map(|sym| std::mem::transmute::<*const (), SnapshotByRawCgrpFn>(*sym)),
            probe_cbw_state: lib
                .get::<*const ()>(b"scxsim_probe_cbw_state")
                .ok()
                .map(|sym| std::mem::transmute::<*const (), ProbeCbwStateFn>(*sym)),
            bw_budget_remaining: lib
                .get::<*const ()>(b"scxsim_cgroup_bw_budget_remaining")
                .ok()
                .map(|sym| std::mem::transmute::<*const (), CgroupBwBudgetRemainingFn>(*sym)),
        }
    }

    /// Return the list of defined ops callback names (without prefix).
    ///
    /// Mandatory ops are always included. Optional ops are included only
    /// when the scheduler `.so` exported the corresponding symbol.
    /// This is only called once during `--wait-debugger` setup.
    fn defined_ops_names(&self) -> Vec<&'static str> {
        // Mandatory ops — always present
        let mut names = vec![
            "init",
            "select_cpu",
            "enqueue",
            "dispatch",
            "running",
            "stopping",
        ];
        // Optional ops — include only when present in the loaded .so
        let optional: [(&str, bool); 21] = [
            ("enable", self.ops.enable.is_some()),
            ("runnable", self.ops.runnable.is_some()),
            ("init_task", self.ops.init_task.is_some()),
            ("cpu_release", self.ops.cpu_release.is_some()),
            ("exit", self.ops.exit.is_some()),
            ("fire_timer", self.ops.fire_timer.is_some()),
            ("quiescent", self.ops.quiescent.is_some()),
            ("dequeue", self.ops.dequeue.is_some()),
            ("tick", self.ops.tick.is_some()),
            ("set_cpumask", self.ops.set_cpumask.is_some()),
            ("dump", self.ops.dump.is_some()),
            ("dump_task", self.ops.dump_task.is_some()),
            ("update_idle", self.ops.update_idle.is_some()),
            ("exit_task", self.ops.exit_task.is_some()),
            ("cgroup_init", self.ops.cgroup_init.is_some()),
            ("cgroup_exit", self.ops.cgroup_exit.is_some()),
            ("cgroup_move", self.ops.cgroup_move.is_some()),
            (
                "cgroup_set_bandwidth",
                self.ops.cgroup_set_bandwidth.is_some(),
            ),
            ("cpu_acquire", self.ops.cpu_acquire.is_some()),
            ("cpu_online", self.ops.cpu_online.is_some()),
            ("cpu_offline", self.ops.cpu_offline.is_some()),
        ];
        for (name, present) in &optional {
            if *present {
                names.push(name);
            }
        }
        names
    }
}

impl Scheduler for DynamicScheduler {
    unsafe fn init(&self) -> i32 {
        (self.ops.init)()
    }

    unsafe fn select_cpu(&self, p: *mut c_void, prev_cpu: i32, wake_flags: u64) -> i32 {
        (self.ops.select_cpu)(p, prev_cpu, wake_flags)
    }

    unsafe fn enqueue(&self, p: *mut c_void, enq_flags: u64) {
        (self.ops.enqueue)(p, enq_flags)
    }

    unsafe fn dispatch(&self, cpu: i32, prev: *mut c_void) {
        (self.ops.dispatch)(cpu, prev)
    }

    unsafe fn running(&self, p: *mut c_void) {
        (self.ops.running)(p)
    }

    unsafe fn stopping(&self, p: *mut c_void, runnable: bool) {
        (self.ops.stopping)(p, runnable)
    }

    unsafe fn enable(&self, p: *mut c_void) {
        if let Some(f) = self.ops.enable {
            f(p);
        }
    }

    unsafe fn runnable(&self, p: *mut c_void, enq_flags: u64) {
        if let Some(f) = self.ops.runnable {
            f(p, enq_flags);
        }
    }

    unsafe fn init_task(&self, p: *mut c_void) -> i32 {
        if let Some(f) = self.ops.init_task {
            let args = sim_get_init_task_args();
            f(p, args)
        } else {
            0
        }
    }

    /// Like `init_task` but overrides the cgroup in init_task_args.
    /// Used when the task belongs to a non-root cgroup.
    unsafe fn init_task_in_cgroup(&self, p: *mut c_void, cgrp: *mut c_void) -> i32 {
        if let Some(f) = self.ops.init_task {
            let args = sim_get_init_task_args();
            sim_set_init_task_cgroup(cgrp);
            f(p, args)
        } else {
            0
        }
    }

    unsafe fn cpu_release(&self, cpu: i32, args: *mut c_void) {
        if let Some(f) = self.ops.cpu_release {
            f(cpu, args);
        }
    }

    unsafe fn cpu_acquire(&self, cpu: i32, args: *mut c_void) {
        if let Some(f) = self.ops.cpu_acquire {
            f(cpu, args);
        }
    }

    unsafe fn exit(&self) {
        if let Some(f) = self.ops.exit {
            f(sim_get_exit_info());
        }
    }

    unsafe fn fire_timer(&self, slot: u32) {
        if let Some(f) = self.ops.fire_timer {
            f(slot);
        }
    }

    unsafe fn dequeue(&self, p: *mut c_void, deq_flags: u64) {
        if let Some(f) = self.ops.dequeue {
            f(p, deq_flags);
        }
    }

    unsafe fn quiescent(&self, p: *mut c_void, deq_flags: u64) {
        if let Some(f) = self.ops.quiescent {
            f(p, deq_flags);
        }
    }

    unsafe fn tick(&self, p: *mut c_void) {
        if let Some(f) = self.ops.tick {
            f(p);
        }
    }

    unsafe fn set_cpumask(&self, p: *mut c_void, cpumask: *const c_void) {
        if let Some(f) = self.ops.set_cpumask {
            f(p, cpumask);
        }
    }

    unsafe fn dump(&self, dctx: *mut c_void) {
        if let Some(f) = self.ops.dump {
            f(dctx);
        }
    }

    unsafe fn dump_task(&self, dctx: *mut c_void, p: *mut c_void) {
        if let Some(f) = self.ops.dump_task {
            f(dctx, p);
        }
    }

    unsafe fn update_idle(&self, cpu: i32, idle: bool) {
        if let Some(f) = self.ops.update_idle {
            f(cpu, idle);
        }
    }

    unsafe fn exit_task(&self, p: *mut c_void) -> i32 {
        if let Some(f) = self.ops.exit_task {
            f(p, sim_get_exit_task_args())
        } else {
            0
        }
    }

    unsafe fn cgroup_init(&self, cgrp: *mut c_void, args: *mut c_void) -> i32 {
        if let Some(f) = self.ops.cgroup_init {
            f(cgrp, args)
        } else {
            0
        }
    }

    unsafe fn cgroup_exit(&self, cgrp: *mut c_void) {
        if let Some(f) = self.ops.cgroup_exit {
            f(cgrp);
        }
    }

    unsafe fn cgroup_move(&self, p: *mut c_void, from: *mut c_void, to: *mut c_void) {
        if let Some(f) = self.ops.cgroup_move {
            f(p, from, to);
        }
    }

    unsafe fn cgroup_set_bandwidth(
        &self,
        cgrp: *mut c_void,
        period_us: u64,
        quota_us: u64,
        burst_us: u64,
    ) {
        if let Some(f) = self.ops.cgroup_set_bandwidth {
            f(cgrp, period_us, quota_us, burst_us);
        }
    }

    unsafe fn cpu_online(&self, cpu: i32) {
        if let Some(f) = self.ops.cpu_online {
            f(cpu);
        }
    }

    unsafe fn cpu_offline(&self, cpu: i32) {
        if let Some(f) = self.ops.cpu_offline {
            f(cpu);
        }
    }

    fn is_cgroup_throttled(&self, cgrp_id: u64) -> Option<bool> {
        // SAFETY: f is dlsym'd at scheduler load; pointer is valid for
        // the lifetime of self._lib. The library contract is `int
        // scx_cgroup_bw_is_cgroup_throttled(u64) -> 0 or 1`.
        self.ops
            .is_cgroup_throttled
            .map(|f| unsafe { f(cgrp_id) != 0 })
    }

    fn probe_cbw_state(&self, cgrp_id: u64, llc_id: i32, out: &mut CbwProbeResult) -> Option<i32> {
        // SAFETY: f is dlsym'd at scheduler load; out is a valid
        // mut ref to a #[repr(C)] struct that mirrors the C side.
        self.ops
            .probe_cbw_state
            .map(|f| unsafe { f(cgrp_id, llc_id, out as *mut CbwProbeResult) })
    }

    fn cgroup_bw_budget_remaining(&self, cgrp_id: u64) -> Option<u64> {
        // SAFETY: f is dlsym'd at scheduler load; pointer is valid
        // for the lifetime of self._lib. Library contract:
        // `u64 scxsim_cgroup_bw_budget_remaining(u64)` returns
        // remaining-ns or u64::MAX for "no cap".
        self.ops.bw_budget_remaining.map(|f| unsafe { f(cgrp_id) })
    }

    // The `cgrp_raw` arg is a cgroup pointer the caller obtained from
    // `cgroup_registry` (or NULL, which the C side rejects safely with
    // rc=-1). The C function never deref's it past the lib's
    // CGRP_STORAGE map lookup keyed by the pointer value. Marking the
    // method `unsafe` would propagate up to every Scheduler trait
    // implementer and the engine call site without buying anything --
    // the constraint is identical to `is_cgroup_throttled` /
    // `probe_cbw_state` (both take cgrp ids that the trait method
    // turns into pointers internally), but we expose the raw pointer
    // here only because the engine already holds the SIM_ARC mutex
    // and cannot safely call `bpf_cgroup_from_id` to do the lookup
    // C-side. The lint is acknowledged with allow.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn snapshot_by_raw_cgrp(
        &self,
        cgid: u64,
        cgrp_raw: *mut c_void,
        out: &mut CbwCgroupSnapshot,
    ) -> Option<i32> {
        let f = self.ops.snapshot_by_raw_cgrp?;
        // SAFETY: `out` is a valid mut ref to a #[repr(C)] struct that
        // mirrors the C side; `cgrp_raw` is a cgroup pointer from
        // `cgroup_registry` valid for the lifetime of the registry
        // entry. The C function only writes `out` on rc=0.
        Some(unsafe { f(cgid, cgrp_raw, out as *mut CbwCgroupSnapshot) })
    }

    fn resolve_e9_fns(&self) -> Option<crate::backend::e9patch::E9PatchFns> {
        // SAFETY: The library contains symbols from our build system.
        // `E9PatchFns::resolve` looks up `e9_arm`/`e9_disarm` symbols.
        unsafe { crate::backend::e9patch::E9PatchFns::resolve(&self._lib) }
    }

    fn debugger_info(&self) -> Option<DebuggerInfo> {
        let prefix = &self.prefix;
        let ops_symbol_names = self
            .defined_ops_names()
            .into_iter()
            .map(|name| format!("{prefix}_{name}"))
            .collect();
        Some(DebuggerInfo {
            so_path: self.so_path.clone(),
            prefix: prefix.clone(),
            ops_symbol_names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type IterNewFn = unsafe extern "C" fn(*mut c_void, u64, u64) -> i32;
    type IterNextFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type IterDestroyFn = unsafe extern "C" fn(*mut c_void);

    #[test]
    fn mitosis_exports_dsq_iterator_symbols() {
        let dir = env!("SCHEDULER_SO_DIR");
        let path = format!("{dir}/libscx_mitosis.so");

        // SAFETY: The test opens a scheduler `.so` built by this crate's
        // build script and verifies that the expected wrapper exports exist.
        unsafe {
            let lib = libloading::Library::new(&path)
                .unwrap_or_else(|err| panic!("failed to load {path}: {err}"));
            let _: libloading::Symbol<IterNewFn> = lib
                .get(b"bpf_iter_scx_dsq_new")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_new in {path}: {err}"));
            let _: libloading::Symbol<IterNextFn> = lib
                .get(b"bpf_iter_scx_dsq_next")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_next in {path}: {err}"));
            let _: libloading::Symbol<IterDestroyFn> = lib
                .get(b"bpf_iter_scx_dsq_destroy")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_destroy in {path}: {err}"));
        }
    }

    #[test]
    fn cosmos_exports_dsq_iterator_symbols() {
        let dir = env!("SCHEDULER_SO_DIR");
        let path = format!("{dir}/libscx_cosmos.so");

        // SAFETY: The test opens a scheduler `.so` built by this crate's
        // build script and verifies that the expected wrapper exports exist.
        unsafe {
            let lib = libloading::Library::new(&path)
                .unwrap_or_else(|err| panic!("failed to load {path}: {err}"));
            let _: libloading::Symbol<IterNewFn> = lib
                .get(b"bpf_iter_scx_dsq_new")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_new in {path}: {err}"));
            let _: libloading::Symbol<IterNextFn> = lib
                .get(b"bpf_iter_scx_dsq_next")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_next in {path}: {err}"));
            let _: libloading::Symbol<IterDestroyFn> = lib
                .get(b"bpf_iter_scx_dsq_destroy")
                .unwrap_or_else(|err| panic!("missing bpf_iter_scx_dsq_destroy in {path}: {err}"));
        }
    }
}
