//! Persistent worker thread pool with futex-based park/wake protocol.
//!
//! Replaces per-dispatch-round `std::thread::scope` with long-lived threads
//! that park between rounds. TLS (preempt context, interleave context,
//! `SIM_ARC`) is installed once at pool creation and persists across rounds.
//!
//! # Protocol
//!
//! ```text
//! Engine                              Worker
//! ------                              ------
//! wait_all_idle()                     (parked on cmd == IDLE)
//! write work_desc[i]
//! store DISPATCH/BATCH to cmd[i]
//! futex_wake(cmd[i])
//!                                     futex_wait(cmd[i], IDLE)
//!                                     read cmd → DISPATCH/BATCH
//!                                     read work_desc[i]
//!                                     execute work
//!                                     store DONE to completion[i]
//!                                     futex_wake(completion[i])
//!                                     store IDLE to cmd[i]
//!                                     futex_wake(cmd[i])
//!                                     loop → futex_wait(cmd[i], IDLE)
//! futex_wait(completion[i], PENDING)
//! (woken, reads DONE)
//! ```
//!
//! After each round the engine calls `wait_all_idle()` to ensure every
//! worker has re-parked before the next `set_work_desc`. This prevents
//! the race where the engine writes a new desc while the worker is still
//! resetting its command.
//!
//! # Safety
//!
//! This module uses `unsafe` for:
//! - Futex syscalls (async-signal-safe)
//! - `UnsafeCell<WorkDesc>` for lockless engine-to-worker communication
//!   (safe because the command futex synchronizes access: engine writes
//!   the desc before storing the command, worker reads after loading it)
//! - `Send` impl for `WorkDesc` (contains only `Copy` types, access is
//!   synchronized by the command futex)

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::thread::{self, JoinHandle, ThreadId};

use crate::interleave::WorkerId;

// ---------------------------------------------------------------------------
// Futex wrappers (async-signal-safe)
// ---------------------------------------------------------------------------
// Local copies identical to `engine_ring::{futex_wait, futex_wake}`.
// These will be deduplicated into a shared crate-level module in Phase E.

/// Atomically check `*futex == expected` and sleep until woken.
fn futex_wait(futex: &AtomicU32, expected: u32) {
    // SAFETY: `SYS_futex` with FUTEX_WAIT is async-signal-safe.
    // `futex` is a valid pointer to an AtomicU32.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex as *const AtomicU32,
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            expected,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
    // Return value intentionally ignored -- spurious wakeups handled by caller.
}

/// Wake up to `count` threads blocked on `futex`.
fn futex_wake(futex: &AtomicU32, count: i32) {
    // SAFETY: `SYS_futex` with FUTEX_WAKE is async-signal-safe.
    // `futex` is a valid pointer to an AtomicU32.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex as *const AtomicU32,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

// ---------------------------------------------------------------------------
// WorkerCommand — strongly typed command enum
// ---------------------------------------------------------------------------

/// Command sent from the engine to a worker thread.
///
/// Encoded as `u32` for futex-compatible atomic storage.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCommand {
    /// Worker is idle, parked on futex.
    Idle = 0,
    /// Execute a dispatch round.
    Dispatch = 1,
    /// Execute a batch round.
    Batch = 2,
    /// Shut down: exit the worker loop.
    Shutdown = 3,
}

impl WorkerCommand {
    /// Convert from raw `u32`. Returns `None` for invalid values.
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Idle),
            1 => Some(Self::Dispatch),
            2 => Some(Self::Batch),
            3 => Some(Self::Shutdown),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// CompletionStatus — strongly typed completion signal
// ---------------------------------------------------------------------------

/// Completion status signaled from a worker back to the engine.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    /// Work is still in progress (or not started).
    Pending = 0,
    /// Work is complete.
    Done = 1,
}

impl CompletionStatus {
    #[cfg(test)]
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Pending),
            1 => Some(Self::Done),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkDesc — per-worker work description
// ---------------------------------------------------------------------------

/// Type-erased per-round work function.
///
/// Called by persistent worker threads each round. The context pointer
/// carries round-specific state (EngineRing, SimulatorState, etc.)
/// cast to `*const ()`. Workers cast it back to the concrete type.
///
/// # Safety
///
/// The caller must ensure `ctx` points to valid data for the duration
/// of the function call. The pointed-to data must match the concrete
/// type expected by this function.
pub type RoundFn = unsafe fn(WorkerId, *const ());

/// Describes the work a worker should perform in one round.
///
/// Written by the engine before issuing a command, read by the worker
/// after receiving the command. The command futex provides the
/// happens-before synchronization.
pub struct WorkDesc {
    /// Which CPU this worker simulates.
    pub cpu: WorkerId,
    /// Opaque payload for the work function. Interpretation depends on the
    /// command (Dispatch vs Batch). The engine sets this before waking
    /// the worker; the worker reads it after being woken.
    pub payload: u64,
    /// Type-erased work function for this round. Set by the engine before
    /// waking the worker. `None` means the worker should use the default
    /// `work_fn` from pool creation.
    pub round_fn: Option<RoundFn>,
    /// Context pointer passed to `round_fn`. Valid only while the worker
    /// is executing (the engine ensures the pointed-to data outlives the
    /// round).
    pub round_ctx: *const (),
}

impl Default for WorkDesc {
    fn default() -> Self {
        Self {
            cpu: WorkerId(0),
            payload: 0,
            round_fn: None,
            round_ctx: std::ptr::null(),
        }
    }
}

// SAFETY: WorkDesc fields are synchronized by the command futex: engine
// writes before storing the command, worker reads after loading the command.
// The raw pointer (`round_ctx`) is only dereferenced on the worker thread
// while the engine-provided data is alive (guaranteed by the protocol).
unsafe impl Send for WorkDesc {}
unsafe impl Sync for WorkDesc {}

// ---------------------------------------------------------------------------
// WorkerPool
// ---------------------------------------------------------------------------

/// A pool of persistent worker threads that park between dispatch rounds.
///
/// Workers run a loop: park -> read command -> execute -> signal done -> park.
/// TLS is installed once at pool creation and persists across rounds.
///
/// # Thread safety
///
/// The pool uses per-worker futex words for commands and completions.
/// Each worker only reads/writes its own slots. The engine writes commands
/// and reads completions; workers read commands and write completions.
/// The futex operations provide the necessary memory ordering.
pub struct WorkerPool {
    handles: Vec<JoinHandle<()>>,
    /// Per-worker command futex: engine writes, worker reads/resets.
    commands: Box<[AtomicU32]>,
    /// Per-worker completion signal: worker writes, engine reads.
    completions: Box<[AtomicU32]>,
    /// Per-worker work description (written by engine, read by worker).
    work_descs: Box<[UnsafeCell<WorkDesc>]>,
    /// Number of workers.
    total: usize,
    /// Whether shutdown has been called.
    shut_down: bool,
}

// SAFETY: WorkerPool's UnsafeCell<WorkDesc> fields are accessed by exactly
// one thread at a time: the engine writes before issuing a command (while
// the worker is parked), and the worker reads after loading the command
// (while the engine waits on the completion futex).
unsafe impl Send for WorkerPool {}
unsafe impl Sync for WorkerPool {}

impl WorkerPool {
    /// Spawn `total` persistent worker threads.
    ///
    /// Each thread calls `setup(worker_id)` for one-time TLS installation,
    /// then enters the park loop. The `work_fn` callback is invoked each
    /// time a worker receives a `Dispatch` or `Batch` command; it receives
    /// `(worker_id, command, &WorkDesc)`.
    ///
    /// # Panics
    ///
    /// Panics if `total` is 0.
    pub fn new<S, W>(total: usize, setup: S, work_fn: W) -> Self
    where
        S: Fn(WorkerId) + Send + Sync + 'static,
        W: Fn(WorkerId, WorkerCommand, &WorkDesc) + Send + Sync + 'static,
    {
        assert!(total > 0, "WorkerPool requires at least 1 worker");

        let commands: Box<[AtomicU32]> = (0..total)
            .map(|_| AtomicU32::new(WorkerCommand::Idle as u32))
            .collect();
        let completions: Box<[AtomicU32]> = (0..total)
            .map(|_| AtomicU32::new(CompletionStatus::Pending as u32))
            .collect();
        let work_descs: Box<[UnsafeCell<WorkDesc>]> = (0..total)
            .map(|_| UnsafeCell::new(WorkDesc::default()))
            .collect();

        // Take pointers to the heap-allocated arrays so workers can access
        // them. The arrays live in `self`, which outlives all workers because
        // Drop joins every thread before freeing the arrays.
        let cmd_ptr = commands.as_ptr();
        let comp_ptr = completions.as_ptr();
        let desc_ptr = work_descs.as_ptr();

        let setup = std::sync::Arc::new(setup);
        let work_fn = std::sync::Arc::new(work_fn);

        let handles = (0..total)
            .map(|i| {
                let setup = std::sync::Arc::clone(&setup);
                let work_fn = std::sync::Arc::clone(&work_fn);
                let cmd_ptr = SendRawPtr(cmd_ptr);
                let comp_ptr = SendRawPtr(comp_ptr);
                let desc_ptr = SendRawPtr(desc_ptr);

                thread::spawn(move || {
                    let worker_id = WorkerId(i);
                    setup(worker_id);
                    worker_loop(worker_id, cmd_ptr, comp_ptr, desc_ptr, &*work_fn);
                })
            })
            .collect();

        WorkerPool {
            handles,
            commands,
            completions,
            work_descs,
            total,
            shut_down: false,
        }
    }

    /// Number of workers in the pool.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Write a work description for the given worker.
    ///
    /// Must only be called while the worker is parked (command == Idle).
    pub fn set_work_desc(&self, worker_id: WorkerId, desc: WorkDesc) {
        // SAFETY: Worker is parked (Idle), so it's not reading the desc.
        // The caller must ensure `wait_all_idle()` was called first.
        unsafe {
            *self.work_descs[worker_id.0].get() = desc;
        }
    }

    /// Issue a command to a specific worker and wake it.
    ///
    /// Resets the completion to `Pending` before issuing the command.
    fn issue_command(&self, worker_id: WorkerId, cmd: WorkerCommand) {
        debug_assert_ne!(cmd, WorkerCommand::Idle, "cannot issue Idle as a command");
        self.completions[worker_id.0].store(CompletionStatus::Pending as u32, SeqCst);
        self.commands[worker_id.0].store(cmd as u32, SeqCst);
        futex_wake(&self.commands[worker_id.0], 1);
    }

    /// Wait for a specific worker to signal completion.
    fn wait_for_completion(&self, worker_id: WorkerId) {
        loop {
            if self.completions[worker_id.0].load(SeqCst) == CompletionStatus::Done as u32 {
                break;
            }
            futex_wait(
                &self.completions[worker_id.0],
                CompletionStatus::Pending as u32,
            );
        }
    }

    /// Spin-wait until a specific worker has reset its command to Idle.
    ///
    /// The worker stores Idle after signaling Done. This ensures the
    /// worker is fully parked before the engine writes a new work desc.
    fn wait_idle(&self, worker_id: WorkerId) {
        loop {
            if self.commands[worker_id.0].load(SeqCst) == WorkerCommand::Idle as u32 {
                break;
            }
            futex_wait(
                &self.commands[worker_id.0],
                // We know the worker just had a Dispatch or Batch command.
                // We wait on the current (non-Idle) value and the worker
                // will wake us when it stores Idle.
                self.commands[worker_id.0].load(SeqCst),
            );
        }
    }

    /// Run a dispatch round: wake all workers, wait for all completions.
    ///
    /// The caller must call `set_work_desc` for each worker before calling
    /// this method.
    pub fn dispatch_round(&self) {
        self.run_round(WorkerCommand::Dispatch);
    }

    /// Run a batch round: wake all workers, wait for all completions.
    ///
    /// The caller must call `set_work_desc` for each worker before calling
    /// this method.
    pub fn batch_round(&self) {
        self.run_round(WorkerCommand::Batch);
    }

    /// Issue a command to all workers and wait for all completions.
    fn run_round(&self, cmd: WorkerCommand) {
        for i in 0..self.total {
            self.issue_command(WorkerId(i), cmd);
        }
        // Wait for all workers to complete and re-park.
        for i in 0..self.total {
            self.wait_for_completion(WorkerId(i));
            self.wait_idle(WorkerId(i));
        }
    }

    /// Wake the first `count` workers with the given command.
    ///
    /// Unlike [`run_round`](Self::run_round), this does NOT wait for
    /// completion. The caller must call [`wait_workers_complete`] after
    /// performing any engine-side work (e.g. running an engine loop).
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds the pool size.
    pub fn wake_workers(&self, count: usize, cmd: WorkerCommand) {
        assert!(
            count <= self.total,
            "wake_workers: count ({count}) exceeds pool size ({})",
            self.total
        );
        for i in 0..count {
            self.issue_command(WorkerId(i), cmd);
        }
    }

    /// Wait for the first `count` workers to complete and re-park.
    ///
    /// Must be called after [`wake_workers`](Self::wake_workers) to
    /// collect completions.
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds the pool size.
    pub fn wait_workers_complete(&self, count: usize) {
        assert!(
            count <= self.total,
            "wait_workers_complete: count ({count}) exceeds pool size ({})",
            self.total
        );
        for i in 0..count {
            self.wait_for_completion(WorkerId(i));
            self.wait_idle(WorkerId(i));
        }
    }

    /// Shut down all workers and join their threads.
    ///
    /// Workers receive the `Shutdown` command, exit their loops, and the
    /// handles are joined. Safe to call multiple times (subsequent calls
    /// are no-ops).
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        for i in 0..self.total {
            self.commands[i].store(WorkerCommand::Shutdown as u32, SeqCst);
            futex_wake(&self.commands[i], 1);
        }

        for handle in self.handles.drain(..) {
            handle
                .join()
                .expect("WorkerPool: worker thread panicked during shutdown");
        }
    }

    /// Get the OS thread IDs of all workers.
    ///
    /// Useful for verifying thread identity stability across rounds.
    pub fn thread_ids(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.handles.iter().map(|h| h.thread().id())
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Worker loop (runs on each spawned thread)
// ---------------------------------------------------------------------------

/// The persistent worker loop. Parks on the command futex, reads the
/// command, executes work, signals completion, resets to Idle, and loops.
fn worker_loop<W>(
    worker_id: WorkerId,
    commands: SendRawPtr<AtomicU32>,
    completions: SendRawPtr<AtomicU32>,
    work_descs: SendRawPtr<UnsafeCell<WorkDesc>>,
    work_fn: &W,
) where
    W: Fn(WorkerId, WorkerCommand, &WorkDesc),
{
    let commands = commands.0;
    let completions = completions.0;
    let work_descs = work_descs.0;

    loop {
        // Park until a non-Idle command arrives.
        // SAFETY: `commands` points into the pool's heap-allocated `commands`
        // array. The pool outlives all workers (Drop joins before freeing).
        let cmd_atom = unsafe { &*commands.add(worker_id.0) };
        loop {
            if cmd_atom.load(SeqCst) != WorkerCommand::Idle as u32 {
                break;
            }
            futex_wait(cmd_atom, WorkerCommand::Idle as u32);
        }

        let raw_cmd = cmd_atom.load(SeqCst);
        let cmd = WorkerCommand::from_u32(raw_cmd).unwrap_or_else(|| {
            panic!(
                "WorkerPool: invalid command {raw_cmd} for worker {}",
                worker_id.0
            )
        });

        if cmd == WorkerCommand::Shutdown {
            return;
        }

        // SAFETY: The engine wrote the desc before storing the command.
        // The SeqCst command store provides happens-before.
        let desc = unsafe { &*(*work_descs.add(worker_id.0)).get() };

        // If the work desc carries a round-specific function, call it.
        // Otherwise fall back to the pool-level work_fn.
        if let Some(round_fn) = desc.round_fn {
            // SAFETY: round_ctx was set by the engine and points to valid
            // data for the duration of this round (engine waits for
            // completion before dropping the context).
            unsafe { round_fn(worker_id, desc.round_ctx) };
        } else {
            work_fn(worker_id, cmd, desc);
        }

        // Signal completion, then reset command to Idle so the engine
        // knows we're parked and can safely write the next work desc.
        // SAFETY: `completions` points into the pool's heap-allocated array.
        let comp_atom = unsafe { &*completions.add(worker_id.0) };
        comp_atom.store(CompletionStatus::Done as u32, SeqCst);
        futex_wake(comp_atom, 1);

        cmd_atom.store(WorkerCommand::Idle as u32, SeqCst);
        futex_wake(cmd_atom, 1);
    }
}

// ---------------------------------------------------------------------------
// SendRawPtr — wrapper to send raw pointers across thread boundaries
// ---------------------------------------------------------------------------

/// Wrapper to send a raw pointer across thread boundaries.
///
/// # Safety
///
/// The caller must ensure the pointed-to data outlives the receiving thread
/// and access is properly synchronized (e.g. by the futex protocol).
struct SendRawPtr<T>(*const T);

// SAFETY: The pointed-to data outlives all workers because Drop joins every
// thread before the data is freed. Access is synchronized by the
// command/completion futex protocol.
unsafe impl<T> Send for SendRawPtr<T> {}
unsafe impl<T> Sync for SendRawPtr<T> {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst as TestSeqCst};
    use std::sync::Arc;

    #[test]
    fn test_single_worker_dispatch() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let mut pool = WorkerPool::new(
            1,
            |_worker_id| {},
            move |_worker_id, cmd, _desc| {
                assert_eq!(cmd, WorkerCommand::Dispatch);
                counter_clone.fetch_add(1, TestSeqCst);
            },
        );

        pool.set_work_desc(WorkerId(0), WorkDesc::default());
        pool.dispatch_round();

        assert_eq!(counter.load(TestSeqCst), 1);
        pool.shutdown();
    }

    #[test]
    fn test_multiple_workers_dispatch() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let num_workers = 4;
        let mut pool = WorkerPool::new(
            num_workers,
            |_worker_id| {},
            move |_worker_id, cmd, _desc| {
                assert_eq!(cmd, WorkerCommand::Dispatch);
                counter_clone.fetch_add(1, TestSeqCst);
            },
        );

        for i in 0..num_workers {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.dispatch_round();

        assert_eq!(counter.load(TestSeqCst), num_workers);
        pool.shutdown();
    }

    #[test]
    fn test_batch_command() {
        let saw_batch = Arc::new(AtomicUsize::new(0));
        let saw_batch_clone = Arc::clone(&saw_batch);

        let mut pool = WorkerPool::new(
            2,
            |_| {},
            move |_worker_id, cmd, _desc| {
                if cmd == WorkerCommand::Batch {
                    saw_batch_clone.fetch_add(1, TestSeqCst);
                }
            },
        );

        for i in 0..2 {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.batch_round();

        assert_eq!(saw_batch.load(TestSeqCst), 2);
        pool.shutdown();
    }

    #[test]
    fn test_multiple_rounds() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let num_workers = 3;
        let num_rounds = 5;
        let mut pool = WorkerPool::new(
            num_workers,
            |_| {},
            move |_worker_id, _cmd, _desc| {
                counter_clone.fetch_add(1, TestSeqCst);
            },
        );

        for _round in 0..num_rounds {
            for i in 0..num_workers {
                pool.set_work_desc(WorkerId(i), WorkDesc::default());
            }
            pool.dispatch_round();
        }

        assert_eq!(counter.load(TestSeqCst), num_workers * num_rounds);
        pool.shutdown();
    }

    #[test]
    fn test_thread_ids_stable_across_rounds() {
        let num_workers = 3;
        let mut pool = WorkerPool::new(num_workers, |_| {}, |_, _, _| {});

        let ids_before: Vec<ThreadId> = pool.thread_ids().collect();
        assert_eq!(ids_before.len(), num_workers);

        // Run a dispatch round.
        for i in 0..num_workers {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.dispatch_round();

        let ids_after: Vec<ThreadId> = pool.thread_ids().collect();
        assert_eq!(
            ids_before, ids_after,
            "thread IDs must be stable across rounds"
        );

        // Run a batch round.
        for i in 0..num_workers {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.batch_round();

        let ids_after2: Vec<ThreadId> = pool.thread_ids().collect();
        assert_eq!(ids_before, ids_after2, "thread IDs must remain stable");

        pool.shutdown();
    }

    #[test]
    fn test_setup_called_once_per_worker() {
        let setup_count = Arc::new(AtomicUsize::new(0));
        let setup_clone = Arc::clone(&setup_count);

        let num_workers = 4;
        let mut pool = WorkerPool::new(
            num_workers,
            move |_| {
                setup_clone.fetch_add(1, TestSeqCst);
            },
            |_, _, _| {},
        );

        // Run multiple rounds -- setup is called exactly once per worker.
        for _round in 0..3 {
            for i in 0..num_workers {
                pool.set_work_desc(WorkerId(i), WorkDesc::default());
            }
            pool.dispatch_round();
        }

        assert_eq!(
            setup_count.load(TestSeqCst),
            num_workers,
            "setup must be called exactly once per worker"
        );
        pool.shutdown();
    }

    #[test]
    fn test_work_desc_payload_delivered() {
        let sum = Arc::new(AtomicUsize::new(0));
        let sum_clone = Arc::clone(&sum);

        let num_workers = 3;
        let mut pool = WorkerPool::new(
            num_workers,
            |_| {},
            move |_worker_id, _cmd, desc| {
                sum_clone.fetch_add(desc.payload as usize, TestSeqCst);
            },
        );

        for i in 0..num_workers {
            pool.set_work_desc(
                WorkerId(i),
                WorkDesc {
                    cpu: WorkerId(i),
                    payload: (i as u64 + 1) * 10,
                    round_fn: None,
                    round_ctx: std::ptr::null(),
                },
            );
        }
        pool.dispatch_round();

        // Payloads: 10 + 20 + 30 = 60.
        assert_eq!(sum.load(TestSeqCst), 60);
        pool.shutdown();
    }

    #[test]
    fn test_shutdown_is_clean() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let mut pool = WorkerPool::new(
            2,
            |_| {},
            move |_worker_id, _cmd, _desc| {
                counter_clone.fetch_add(1, TestSeqCst);
            },
        );

        for i in 0..2 {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.dispatch_round();
        assert_eq!(counter.load(TestSeqCst), 2);

        // First shutdown joins cleanly.
        pool.shutdown();
        // Second shutdown is a no-op.
        pool.shutdown();
    }

    #[test]
    fn test_drop_shuts_down() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        {
            let pool = WorkerPool::new(
                2,
                |_| {},
                move |_worker_id, _cmd, _desc| {
                    counter_clone.fetch_add(1, TestSeqCst);
                },
            );
            for i in 0..2 {
                pool.set_work_desc(WorkerId(i), WorkDesc::default());
            }
            // pool dropped here; Drop calls shutdown().
        }
        // Reaching here means Drop didn't deadlock.
    }

    #[test]
    fn test_worker_command_roundtrip() {
        for cmd in [
            WorkerCommand::Idle,
            WorkerCommand::Dispatch,
            WorkerCommand::Batch,
            WorkerCommand::Shutdown,
        ] {
            let raw = cmd as u32;
            assert_eq!(WorkerCommand::from_u32(raw), Some(cmd));
        }
        assert_eq!(WorkerCommand::from_u32(99), None);
    }

    #[test]
    fn test_completion_status_roundtrip() {
        for status in [CompletionStatus::Pending, CompletionStatus::Done] {
            let raw = status as u32;
            assert_eq!(CompletionStatus::from_u32(raw), Some(status));
        }
        assert_eq!(CompletionStatus::from_u32(99), None);
    }

    #[test]
    fn test_worker_id_passed_correctly() {
        let num_workers = 4;
        let seen = Arc::new(
            (0..num_workers)
                .map(|_| AtomicUsize::new(usize::MAX))
                .collect::<Vec<_>>(),
        );
        let seen_clone = Arc::clone(&seen);

        let mut pool = WorkerPool::new(
            num_workers,
            |_| {},
            move |worker_id, _cmd, _desc| {
                seen_clone[worker_id.0].store(worker_id.0, TestSeqCst);
            },
        );

        for i in 0..num_workers {
            pool.set_work_desc(WorkerId(i), WorkDesc::default());
        }
        pool.dispatch_round();

        for i in 0..num_workers {
            assert_eq!(
                seen[i].load(TestSeqCst),
                i,
                "worker {i} should have received WorkerId({i})"
            );
        }
        pool.shutdown();
    }

    #[test]
    #[should_panic(expected = "WorkerPool requires at least 1 worker")]
    fn test_zero_workers_panics() {
        let _pool = WorkerPool::new(0, |_| {}, |_, _, _| {});
    }

    #[test]
    fn test_alternating_dispatch_and_batch() {
        let dispatch_count = Arc::new(AtomicUsize::new(0));
        let batch_count = Arc::new(AtomicUsize::new(0));
        let dc = Arc::clone(&dispatch_count);
        let bc = Arc::clone(&batch_count);

        let num_workers = 2;
        let mut pool = WorkerPool::new(
            num_workers,
            |_| {},
            move |_worker_id, cmd, _desc| match cmd {
                WorkerCommand::Dispatch => {
                    dc.fetch_add(1, TestSeqCst);
                }
                WorkerCommand::Batch => {
                    bc.fetch_add(1, TestSeqCst);
                }
                _ => panic!("unexpected command: {cmd:?}"),
            },
        );

        for _round in 0..3 {
            for i in 0..num_workers {
                pool.set_work_desc(WorkerId(i), WorkDesc::default());
            }
            pool.dispatch_round();

            for i in 0..num_workers {
                pool.set_work_desc(WorkerId(i), WorkDesc::default());
            }
            pool.batch_round();
        }

        assert_eq!(dispatch_count.load(TestSeqCst), num_workers * 3);
        assert_eq!(batch_count.load(TestSeqCst), num_workers * 3);
        pool.shutdown();
    }
}
