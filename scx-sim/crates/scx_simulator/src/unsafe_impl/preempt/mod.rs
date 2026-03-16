//! Preemptive interleaving via PMU RBC timer signals.
//!
//! Extends the cooperative kfunc-boundary interleaving (see [`interleave`]) with
//! mid-C-code preemption points. A PMU counter fires `SIGSTKFLT` after a random
//! number of retired conditional branches; the signal handler parks the worker
//! via futex and the [`PreemptRing`] passes execution to another worker.
//!
//! # Safety
//!
//! This module contains extensive `unsafe` code: signal handler registration
//! (`sigaction`), raw `futex()` syscalls, inline assembly for reading RIP,
//! atomic operations with `SeqCst` ordering for cross-thread communication,
//! and raw file-descriptor manipulation for PMU `perf_event_open`. The signal
//! handler path is constrained to async-signal-safe primitives only.
//!
//! ## Determinism
//!
//! RBC (Retired Branch Conditionals) counts only *retired* (committed) branches,
//! not speculative ones. This makes RBC fully deterministic — the same code path
//! produces the same branch count. Tools like [rr](https://rr-project.org/) and
//! [Hermit](https://github.com/facebookexperimental/hermit) rely on this property
//! for record/replay.
//!
//! Combined with deterministic PRNG-driven timeslice selection, preemptive
//! interleaving is deterministic: same seed → same interleaving → same trace.
//!
//! See `ai_docs/DETERMINISM.md` for the full explanation.
//!
//! ## Signal safety
//!
//! The entire preemption path — signal handler, token passing, park/unpark —
//! uses only async-signal-safe primitives: atomics and raw `futex()` syscalls.
//! No `Mutex`, `Condvar`, or heap allocation in the hot path.
//!
//! ## Relationship to [`interleave`]
//!
//! - [`interleave::TokenRing`] uses `Mutex`/`Condvar` for cooperative yields at
//!   kfunc boundaries.
//! - [`PreemptRing`] uses atomics/futex for both cooperative and preemptive
//!   yields, making it safe to call from signal handlers.
//!
//! When preemptive interleaving is enabled, `PreemptRing` replaces `TokenRing`.
//! The existing [`interleave::maybe_yield`] cooperative yield points continue
//! to work — they call into `PreemptRing` instead of `TokenRing`.
//!
//! [`interleave`]: crate::interleave

use core::fmt::Write as FmtWrite;
use std::cell::Cell;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

use crate::interleave::WorkerId;
use crate::kfuncs::OpsContext;
use crate::types::CpuId;

pub mod trace;

// ---------------------------------------------------------------------------
// PreemptionRecord — instrumentation for verifying determinism
// ---------------------------------------------------------------------------

/// Maximum number of preemption records to store.
/// This is a fixed-size ring buffer to avoid allocation in signal handlers.
const MAX_PREEMPTION_RECORDS: usize = 4096;

/// Number of instruction bytes captured at each preemption point.
/// Used to detect .so version mismatches during replay.
pub const INSN_BYTES_LEN: usize = 5;

/// A record of a single preemption point (PMU or cooperative kfunc yield).
///
/// Captures all relevant state at the moment of preemption for verifying
/// that RBC-based preemption is truly deterministic: same branch count,
/// same instruction, every time. Includes structop context for correlating
/// preemptions with scheduler ops callback invocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreemptionRecord {
    /// The timeslice (retired branch conditionals) for this preemption.
    /// For cooperative (kfunc) yields this is 0.
    pub rbc_count: u64,
    /// The instruction pointer (RIP) at the preemption point.
    /// For cooperative (kfunc) yields this is 0.
    pub instruction_pointer: u64,
    /// The CPU ID of the worker that was preempted.
    pub cpu_id: CpuId,
    /// The worker that was preempted (for replay grouping).
    pub worker_id: WorkerId,
    /// Sequence number (monotonically increasing per PreemptRing).
    pub sequence: u64,
    /// Per-worker structop count (1-based) at the time of preemption.
    pub structop_local: u64,
    /// Global structop count (1-based) at the time of preemption.
    pub structop_global: u64,
    /// Cumulative per-worker RBC across the entire dispatch round.
    ///
    /// This is the total of all `rbc_count` values accumulated via
    /// `record_rbc_preemption()` on this worker up to this point.
    /// NOT per-structop — the counter is never reset at structop boundaries.
    ///
    /// In replay mode, the PMU counter runs continuously without resets at
    /// kfunc boundaries, and replay handlers compare against this cumulative
    /// value to find the correct preemption point.
    pub structop_rbc: u64,
    /// Which ops callback was active at the time of preemption.
    pub ops_context: OpsContext,
    /// Name of the kfunc being called (empty if unknown or PMU preemption).
    pub kfunc_name: &'static str,
    /// Number of kfuncs executed within the current structop at preemption.
    /// Resets when a new structop begins. Used as a sub-coordinate for replay.
    pub kfunc_count: u32,
    /// First [`INSN_BYTES_LEN`] bytes of the instruction at the RIP.
    /// Used to detect .so version mismatches during replay.
    pub insn_bytes: [u8; INSN_BYTES_LEN],
}

/// Pack instruction bytes into a `u64` for signal-safe atomic storage.
///
/// The first [`INSN_BYTES_LEN`] bytes are stored in the low bytes of the u64.
pub fn pack_insn_bytes(bytes: [u8; INSN_BYTES_LEN]) -> u64 {
    let mut val: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        val |= (b as u64) << (i * 8);
    }
    val
}

/// Unpack instruction bytes from a `u64` stored by [`pack_insn_bytes`].
pub fn unpack_insn_bytes(val: u64) -> [u8; INSN_BYTES_LEN] {
    let mut bytes = [0u8; INSN_BYTES_LEN];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = ((val >> (i * 8)) & 0xff) as u8;
    }
    bytes
}

/// Read [`INSN_BYTES_LEN`] bytes from the given instruction pointer.
///
/// The RIP is in the process's own address space (scheduler .so code),
/// so we can dereference it directly. Returns all zeros if `rip` is 0.
///
/// # Safety
///
/// The caller must ensure `rip` points to a valid, readable address
/// in the process address space. This is guaranteed for RIPs captured
/// from `ucontext` in the signal handler — they point into the loaded
/// scheduler .so's executable mapping.
pub fn read_insn_bytes_at(rip: u64) -> [u8; INSN_BYTES_LEN] {
    if rip == 0 {
        return [0u8; INSN_BYTES_LEN];
    }
    let mut bytes = [0u8; INSN_BYTES_LEN];
    // SAFETY: rip comes from a ucontext captured by the kernel signal
    // delivery mechanism, pointing into the loaded scheduler .so's
    // executable text segment. The .so remains loaded for the duration
    // of the simulation.
    unsafe {
        std::ptr::copy_nonoverlapping(rip as *const u8, bytes.as_mut_ptr(), INSN_BYTES_LEN);
    }
    bytes
}

/// Format instruction bytes as a hex string (e.g. "48890424ff").
pub fn format_insn_bytes(bytes: &[u8; INSN_BYTES_LEN]) -> String {
    let mut s = String::with_capacity(INSN_BYTES_LEN * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl std::fmt::Display for PreemptionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "seq={} ops={} kfunc={} structop={}:{} kfunc_count={} rbc={} rip=0x{:x} insn={} cpu={} worker={}",
            self.sequence,
            self.ops_context.short_name(),
            if self.kfunc_name.is_empty() {
                "none"
            } else {
                self.kfunc_name
            },
            self.structop_local,
            self.structop_global,
            self.kfunc_count,
            self.structop_rbc,
            self.instruction_pointer,
            format_insn_bytes(&self.insn_bytes),
            self.cpu_id.0,
            self.worker_id.0
        )
    }
}

/// Number of AtomicU64 slots per preemption record.
const RECORD_FIELDS: usize = 13;

/// Global monotonic sequence counter shared across all `PreemptionRecordStore`
/// instances. Ensures unique, globally-ordered sequence numbers even when
/// multiple `PreemptRing`s are created across dispatch rounds.
static PREEMPTION_GLOBAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Reset the global preemption sequence counter.
///
/// Call before starting a new simulation to get clean sequence numbers.
pub fn reset_preemption_sequence() {
    PREEMPTION_GLOBAL_SEQ.store(0, SeqCst);
}

/// Fixed-size storage for preemption records (signal-safe).
///
/// Uses a fixed array with atomic index to avoid heap allocation in signal
/// handlers. Records beyond MAX_PREEMPTION_RECORDS are dropped.
pub(crate) struct PreemptionRecordStore {
    /// Fixed-size array of records (pre-allocated).
    records: Box<[AtomicU64; MAX_PREEMPTION_RECORDS * RECORD_FIELDS]>,
    /// Number of records stored (atomic for signal safety).
    count: AtomicUsize,
}

impl PreemptionRecordStore {
    pub(crate) fn new() -> Self {
        // Initialize all slots to zero using a const array.
        let records: Box<[AtomicU64; MAX_PREEMPTION_RECORDS * RECORD_FIELDS]> = (0
            ..MAX_PREEMPTION_RECORDS * RECORD_FIELDS)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        PreemptionRecordStore {
            records,
            count: AtomicUsize::new(0),
        }
    }

    /// Add a record (signal-safe: uses only atomics).
    ///
    /// Returns the sequence number assigned, or None if the buffer is full.
    pub(crate) fn push(
        &self,
        rbc_count: u64,
        instruction_pointer: u64,
        cpu_id: CpuId,
        worker_id: WorkerId,
        sinfo: StructopInfo,
        insn_bytes: [u8; INSN_BYTES_LEN],
    ) -> Option<u64> {
        let idx = self.count.fetch_add(1, SeqCst);
        if idx >= MAX_PREEMPTION_RECORDS {
            // Buffer full, revert and drop.
            self.count.fetch_sub(1, SeqCst);
            return None;
        }
        let seq = PREEMPTION_GLOBAL_SEQ.fetch_add(1, SeqCst);
        let base = idx * RECORD_FIELDS;
        self.records[base].store(rbc_count, SeqCst);
        self.records[base + 1].store(instruction_pointer, SeqCst);
        self.records[base + 2].store(cpu_id.0 as u64, SeqCst);
        self.records[base + 3].store(worker_id.0 as u64, SeqCst);
        self.records[base + 4].store(seq, SeqCst);
        self.records[base + 5].store(sinfo.cpu_count, SeqCst);
        self.records[base + 6].store(sinfo.global_count, SeqCst);
        self.records[base + 7].store(sinfo.rbc_total, SeqCst);
        self.records[base + 8].store(sinfo.ops_context as u8 as u64, SeqCst);
        // Store kfunc_name as ptr+len — safe because all names are &'static str.
        self.records[base + 9].store(sinfo.kfunc_name.as_ptr() as u64, SeqCst);
        self.records[base + 10].store(sinfo.kfunc_name.len() as u64, SeqCst);
        self.records[base + 11].store(pack_insn_bytes(insn_bytes), SeqCst);
        self.records[base + 12].store(sinfo.kfunc_count_local, SeqCst);
        Some(seq)
    }

    /// Retrieve all records (not signal-safe, call after simulation).
    pub(crate) fn drain(&self) -> Vec<PreemptionRecord> {
        let count = self.count.load(SeqCst).min(MAX_PREEMPTION_RECORDS);
        let mut records = Vec::with_capacity(count);
        for i in 0..count {
            let base = i * RECORD_FIELDS;
            let ops_disc = self.records[base + 8].load(SeqCst) as u8;
            let kfunc_ptr = self.records[base + 9].load(SeqCst) as usize;
            let kfunc_len = self.records[base + 10].load(SeqCst) as usize;
            // SAFETY: kfunc_name was a &'static str, so ptr+len is valid.
            let kfunc_name = if kfunc_ptr != 0 && kfunc_len > 0 {
                unsafe {
                    std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                        kfunc_ptr as *const u8,
                        kfunc_len,
                    ))
                }
            } else {
                ""
            };
            records.push(PreemptionRecord {
                rbc_count: self.records[base].load(SeqCst),
                instruction_pointer: self.records[base + 1].load(SeqCst),
                cpu_id: CpuId(self.records[base + 2].load(SeqCst) as u32),
                worker_id: WorkerId(self.records[base + 3].load(SeqCst) as usize),
                sequence: self.records[base + 4].load(SeqCst),
                structop_local: self.records[base + 5].load(SeqCst),
                structop_global: self.records[base + 6].load(SeqCst),
                structop_rbc: self.records[base + 7].load(SeqCst),
                ops_context: OpsContext::from_discriminant(ops_disc),
                kfunc_name,
                kfunc_count: self.records[base + 12].load(SeqCst) as u32,
                insn_bytes: unpack_insn_bytes(self.records[base + 11].load(SeqCst)),
            });
        }
        // Sort by sequence number to ensure deterministic ordering.
        records.sort_by_key(|r| r.sequence);
        records
    }
}

// ---------------------------------------------------------------------------
// DeterminismCheckpoint — aggressive determinism verification
// ---------------------------------------------------------------------------

/// Event types that can trigger a determinism checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CheckpointEvent {
    /// PMU signal-triggered preemption.
    Preemption = 0,
    /// Cooperative yield at kfunc boundary.
    CooperativeYield = 1,
    /// ops.dispatch() callback invoked.
    Dispatch = 2,
    /// ops.enqueue() callback invoked.
    Enqueue = 3,
    /// ops.running() callback invoked.
    Running = 4,
    /// ops.stopping() callback invoked.
    Stopping = 5,
    /// ops.select_cpu() callback invoked.
    SelectCpu = 6,
    /// ops.tick() callback invoked.
    Tick = 7,
}

impl std::fmt::Display for CheckpointEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CheckpointEvent::Preemption => "preemption",
            CheckpointEvent::CooperativeYield => "coop_yield",
            CheckpointEvent::Dispatch => "dispatch",
            CheckpointEvent::Enqueue => "enqueue",
            CheckpointEvent::Running => "running",
            CheckpointEvent::Stopping => "stopping",
            CheckpointEvent::SelectCpu => "select_cpu",
            CheckpointEvent::Tick => "tick",
        };
        write!(f, "{}", s)
    }
}

/// A determinism checkpoint capturing scheduler state at a key event.
///
/// Used for aggressive determinism verification: compare checkpoint sequences
/// from two runs with the same seed to detect divergence and pinpoint exactly
/// where execution differed (RIP, RBC, or memory state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeterminismCheckpoint {
    /// Monotonically increasing sequence number.
    pub sequence: u64,
    /// The type of event that triggered this checkpoint.
    pub event: CheckpointEvent,
    /// Instruction pointer (RIP) at the checkpoint, if available.
    pub instruction_pointer: u64,
    /// RBC (retired branch conditional) count, if available.
    pub rbc_count: u64,
    /// FNV-1a hash of scheduler-visible memory state (DSQ contents, etc.).
    pub memory_hash: u64,
    /// CPU ID where the event occurred.
    pub cpu_id: CpuId,
}

impl std::fmt::Display for DeterminismCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "seq={} event={} rip=0x{:016x} rbc={} hash=0x{:016x} cpu={}",
            self.sequence,
            self.event,
            self.instruction_pointer,
            self.rbc_count,
            self.memory_hash,
            self.cpu_id.0
        )
    }
}

/// Result of comparing two checkpoint sequences.
#[derive(Debug, Clone)]
pub struct CheckpointDivergence {
    /// Index of the first diverging checkpoint.
    pub checkpoint_index: usize,
    /// The expected checkpoint (from run 1).
    pub expected: DeterminismCheckpoint,
    /// The actual checkpoint (from run 2).
    pub actual: DeterminismCheckpoint,
    /// Which field(s) diverged.
    pub divergence_type: DivergenceType,
}

impl std::fmt::Display for CheckpointDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Divergence at checkpoint {}: {}\n  expected: {}\n  actual:   {}",
            self.checkpoint_index, self.divergence_type, self.expected, self.actual
        )
    }
}

/// Which field(s) diverged between two checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceType {
    /// Instruction pointer (RIP) differs.
    Rip,
    /// RBC count differs.
    Rbc,
    /// Memory hash differs.
    MemoryHash,
    /// Event type differs.
    EventType,
    /// CPU ID differs.
    CpuId,
    /// Multiple fields differ.
    Multiple(Vec<DivergenceType>),
}

impl std::fmt::Display for DivergenceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DivergenceType::Rip => write!(f, "RIP differs"),
            DivergenceType::Rbc => write!(f, "RBC count differs"),
            DivergenceType::MemoryHash => write!(f, "memory hash differs"),
            DivergenceType::EventType => write!(f, "event type differs"),
            DivergenceType::CpuId => write!(f, "CPU ID differs"),
            DivergenceType::Multiple(types) => {
                let strs: Vec<_> = types.iter().map(|t| format!("{}", t)).collect();
                write!(f, "{}", strs.join(", "))
            }
        }
    }
}

/// Compare two checkpoint sequences and return the first divergence, if any.
///
/// Returns `None` if the sequences are identical, `Some(divergence)` otherwise.
pub fn compare_checkpoints(
    expected: &[DeterminismCheckpoint],
    actual: &[DeterminismCheckpoint],
) -> Option<CheckpointDivergence> {
    // First check for length mismatch
    if expected.len() != actual.len() {
        // Find the first index where they differ
        let min_len = expected.len().min(actual.len());
        for i in 0..min_len {
            if let Some(div) = compare_single_checkpoint(i, &expected[i], &actual[i]) {
                return Some(div);
            }
        }
        // If all common elements match, the divergence is at the shorter length
        if expected.len() > actual.len() {
            return Some(CheckpointDivergence {
                checkpoint_index: actual.len(),
                expected: expected[actual.len()],
                actual: DeterminismCheckpoint {
                    sequence: 0,
                    event: CheckpointEvent::Preemption,
                    instruction_pointer: 0,
                    rbc_count: 0,
                    memory_hash: 0,
                    cpu_id: CpuId(0),
                },
                divergence_type: DivergenceType::EventType,
            });
        } else {
            return Some(CheckpointDivergence {
                checkpoint_index: expected.len(),
                expected: DeterminismCheckpoint {
                    sequence: 0,
                    event: CheckpointEvent::Preemption,
                    instruction_pointer: 0,
                    rbc_count: 0,
                    memory_hash: 0,
                    cpu_id: CpuId(0),
                },
                actual: actual[expected.len()],
                divergence_type: DivergenceType::EventType,
            });
        }
    }

    // Compare element by element
    for i in 0..expected.len() {
        if let Some(div) = compare_single_checkpoint(i, &expected[i], &actual[i]) {
            return Some(div);
        }
    }
    None
}

fn compare_single_checkpoint(
    index: usize,
    expected: &DeterminismCheckpoint,
    actual: &DeterminismCheckpoint,
) -> Option<CheckpointDivergence> {
    let mut divergences = Vec::new();

    if expected.event != actual.event {
        divergences.push(DivergenceType::EventType);
    }
    if expected.cpu_id != actual.cpu_id {
        divergences.push(DivergenceType::CpuId);
    }
    if expected.instruction_pointer != actual.instruction_pointer {
        divergences.push(DivergenceType::Rip);
    }
    if expected.rbc_count != actual.rbc_count {
        divergences.push(DivergenceType::Rbc);
    }
    if expected.memory_hash != actual.memory_hash {
        divergences.push(DivergenceType::MemoryHash);
    }

    if divergences.is_empty() {
        None
    } else {
        let divergence_type = if divergences.len() == 1 {
            divergences.pop().unwrap()
        } else {
            DivergenceType::Multiple(divergences)
        };
        Some(CheckpointDivergence {
            checkpoint_index: index,
            expected: *expected,
            actual: *actual,
            divergence_type,
        })
    }
}

// ---------------------------------------------------------------------------
// FNV-1a hash for fast memory hashing
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x100000001b3;

/// FNV-1a hash implementation (fast, non-cryptographic).
///
/// This is a simple, fast hash suitable for determinism checking.
/// It has good distribution properties for detecting state changes.
#[inline]
pub fn fnv1a_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Combine multiple hashes into one (order-dependent).
#[inline]
pub fn fnv1a_combine(h1: u64, h2: u64) -> u64 {
    let mut hash = h1;
    // Hash h2's bytes into the combined hash
    for i in 0..8 {
        let byte = ((h2 >> (i * 8)) & 0xff) as u8;
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hash a u64 value.
#[inline]
pub fn fnv1a_hash_u64(value: u64) -> u64 {
    fnv1a_hash_bytes(&value.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Global checkpoint collector (for aggressive determinism mode)
// ---------------------------------------------------------------------------

/// Maximum number of checkpoints to store.
const MAX_CHECKPOINTS: usize = 8192;

/// Global state for aggressive determinism mode.
struct CheckpointCollector {
    /// Whether aggressive determinism mode is enabled.
    enabled: bool,
    /// Collected checkpoints.
    checkpoints: Vec<DeterminismCheckpoint>,
    /// Sequence counter.
    sequence: u64,
}

impl CheckpointCollector {
    const fn new() -> Self {
        CheckpointCollector {
            enabled: false,
            checkpoints: Vec::new(),
            sequence: 0,
        }
    }
}

/// Global checkpoint collector.
static CHECKPOINT_COLLECTOR: Mutex<CheckpointCollector> = Mutex::new(CheckpointCollector::new());

/// Global flag for fast path checking (avoid lock on every callback).
static DETERMINISM_MODE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable aggressive determinism mode.
///
/// Call this before running a simulation to start collecting checkpoints
/// at scheduling events. Checkpoints include memory hashes for detecting
/// state divergence.
pub fn enable_determinism_mode() {
    let mut guard = CHECKPOINT_COLLECTOR.lock().unwrap();
    guard.enabled = true;
    guard.checkpoints.clear();
    guard.checkpoints.reserve(MAX_CHECKPOINTS);
    guard.sequence = 0;
    DETERMINISM_MODE_ENABLED.store(true, SeqCst);
}

/// Disable aggressive determinism mode and drain collected checkpoints.
///
/// Returns the collected checkpoints sorted by sequence number.
pub fn drain_determinism_checkpoints() -> Vec<DeterminismCheckpoint> {
    let mut guard = CHECKPOINT_COLLECTOR.lock().unwrap();
    guard.enabled = false;
    DETERMINISM_MODE_ENABLED.store(false, SeqCst);
    let mut checkpoints = std::mem::take(&mut guard.checkpoints);
    checkpoints.sort_by_key(|c| c.sequence);
    checkpoints
}

/// Check if aggressive determinism mode is enabled (fast path).
#[inline]
pub fn is_determinism_mode_enabled() -> bool {
    DETERMINISM_MODE_ENABLED.load(SeqCst)
}

/// Record a determinism checkpoint.
///
/// Only records if aggressive determinism mode is enabled.
/// Returns the assigned sequence number, or None if disabled/full.
pub fn record_checkpoint(
    event: CheckpointEvent,
    instruction_pointer: u64,
    rbc_count: u64,
    memory_hash: u64,
    cpu_id: CpuId,
) -> Option<u64> {
    // Fast path: check atomic flag before taking lock
    if !DETERMINISM_MODE_ENABLED.load(SeqCst) {
        return None;
    }

    let mut guard = match CHECKPOINT_COLLECTOR.try_lock() {
        Ok(g) => g,
        Err(_) => return None, // Don't block if lock is held
    };

    if !guard.enabled || guard.checkpoints.len() >= MAX_CHECKPOINTS {
        return None;
    }

    let seq = guard.sequence;
    guard.sequence += 1;

    guard.checkpoints.push(DeterminismCheckpoint {
        sequence: seq,
        event,
        instruction_pointer,
        rbc_count,
        memory_hash,
        cpu_id,
    });

    Some(seq)
}

// ---------------------------------------------------------------------------
// Global preemption record collector (for test instrumentation)
// ---------------------------------------------------------------------------

/// Global collector for preemption records, accessible from tests.
///
/// This provides a way to capture preemption records across all `PreemptRing`
/// instances in a simulation run, without threading records through the engine.
static GLOBAL_PREEMPTION_COLLECTOR: Mutex<Option<Vec<PreemptionRecord>>> = Mutex::new(None);

/// Enable global preemption record collection.
///
/// Call this before running a simulation to start collecting preemption records.
/// Records are accumulated until `drain_preemption_records()` is called.
/// Also resets the global sequence counter so records start at seq=0.
pub fn enable_preemption_collection() {
    reset_preemption_sequence();
    let mut guard = GLOBAL_PREEMPTION_COLLECTOR.lock().unwrap();
    *guard = Some(Vec::new());
}

/// Disable and drain all collected preemption records.
///
/// Returns the collected records sorted by sequence number, or an empty Vec
/// if collection was not enabled. Also disables further collection.
pub fn drain_preemption_records() -> Vec<PreemptionRecord> {
    let mut guard = GLOBAL_PREEMPTION_COLLECTOR.lock().unwrap();
    let mut records = guard.take().unwrap_or_default();
    records.sort_by_key(|r| r.sequence);
    records
}

/// Add a record to the global collector (signal-safe: uses try_lock).
///
/// Called from the signal handler after recording to the ring's local store.
/// This duplicates records to the global collector for test access.
fn maybe_collect_global(record: PreemptionRecord) {
    // Use try_lock to avoid blocking in signal handler.
    // If the lock is contended, we simply drop this record from global collection.
    if let Ok(mut guard) = GLOBAL_PREEMPTION_COLLECTOR.try_lock() {
        if let Some(ref mut records) = *guard {
            records.push(record);
        }
    }
}

// ---------------------------------------------------------------------------
// Futex wrappers (async-signal-safe — raw syscalls only)
// ---------------------------------------------------------------------------

/// Atomically check `*futex == expected` and sleep until woken.
///
/// Returns immediately (spurious wakeup) if the value has changed.
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
    // Return value intentionally ignored — spurious wakeups handled by caller.
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
// Structop tracking — per-callback context for trace messages
// ---------------------------------------------------------------------------

/// Snapshot of structop tracking for trace output.
///
/// A "structop" is one invocation of a scheduler ops callback (dispatch,
/// enqueue, select_cpu, etc.). All counters are monotonically increasing
/// across the entire simulation for a given CPU:
/// - `cpu_count`: how many structops this CPU has entered
/// - `global_count`: how many structops all CPUs have entered
/// - `rbc_total`: cumulative retired conditional branches on this CPU
/// - `kfunc_count`: cumulative cooperative yields on this CPU
/// - `interleave_count`: cumulative interleaving events (preemptions + yields)
/// - `ops_context`: which ops callback is currently active
/// - `kfunc_name`: name of the kfunc about to be called
#[derive(Debug, Clone, Copy, Default)]
pub struct StructopInfo {
    /// Per-CPU structop call count (monotonically increasing).
    pub cpu_count: u64,
    /// Global structop call count across all CPUs (monotonically increasing).
    pub global_count: u64,
    /// Cumulative RBC count on this worker (monotonically increasing).
    ///
    /// Sum of per-preemption `rbc_count` values. In recording mode, each
    /// `rbc_count` is a per-reset delta (branches since the last
    /// `rearm_timer`). In replay mode, each `rbc_count` is the recorded
    /// delta from the trace. This value is stored as `structop_rbc` in
    /// [`PreemptionRecord`] (despite the name, it is NOT per-structop).
    pub rbc_total: u64,
    /// Cumulative cooperative yield count on this CPU (monotonically increasing).
    pub kfunc_count: u64,
    /// Cumulative interleaving events on this CPU (preemptions + cooperative yields).
    pub interleave_count: u64,
    /// Which ops callback is currently active.
    pub ops_context: OpsContext,
    /// Name of the kfunc about to be called (empty string if unknown).
    pub kfunc_name: &'static str,
    /// Per-structop kfunc count (resets at each structop boundary).
    /// Tracks how many kfuncs have executed within the current structop.
    pub kfunc_count_local: u64,
}

thread_local! {
    static STRUCTOP_CPU_COUNT: Cell<u64> = const { Cell::new(0) };
    static STRUCTOP_RBC_TOTAL: Cell<u64> = const { Cell::new(0) };
    static STRUCTOP_KFUNC_COUNT: Cell<u64> = const { Cell::new(0) };
    static STRUCTOP_INTERLEAVE_COUNT: Cell<u64> = const { Cell::new(0) };
    /// Per-structop kfunc count (resets at each structop boundary).
    static STRUCTOP_KFUNC_COUNT_LOCAL: Cell<u64> = const { Cell::new(0) };
    static IN_STRUCTOP: Cell<bool> = const { Cell::new(false) };
    /// Current kfunc name, set before each `maybe_yield()` call.
    static CURRENT_KFUNC_NAME: Cell<&'static str> = const { Cell::new("") };
    /// Current ops context, cached from SimulatorState at structop boundary.
    static CURRENT_OPS_CONTEXT: Cell<OpsContext> = const { Cell::new(OpsContext::None) };
}
static STRUCTOP_GLOBAL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Seed the thread-local structop counters with base offsets from previous
/// dispatch rounds. Call on each worker thread after `install()`.
pub fn seed_structop(base: &StructopInfo) {
    STRUCTOP_CPU_COUNT.with(|c| c.set(base.cpu_count));
    STRUCTOP_RBC_TOTAL.with(|c| c.set(base.rbc_total));
    STRUCTOP_KFUNC_COUNT.with(|c| c.set(base.kfunc_count));
    STRUCTOP_INTERLEAVE_COUNT.with(|c| c.set(base.interleave_count));
    IN_STRUCTOP.with(|c| c.set(false));
    CURRENT_KFUNC_NAME.with(|c| c.set(""));
    STRUCTOP_KFUNC_COUNT_LOCAL.with(|c| c.set(0));
    CURRENT_OPS_CONTEXT.with(|c| c.set(OpsContext::None));
}

/// Begin a new structop: increment per-CPU and global counts.
///
/// Note: does NOT reset `STRUCTOP_RBC_TOTAL`. Despite the "structop" prefix,
/// the RBC counter (`rbc_total` / `structop_rbc`) is cumulative across the
/// entire dispatch round, not per-structop. In recording mode, the PMU
/// counter is reset per-kfunc via `rearm_timer`, and `rbc_count` deltas
/// accumulate into the cumulative total. In replay mode, the counter runs
/// continuously without any resets.
fn begin_structop() {
    STRUCTOP_CPU_COUNT.with(|c| c.set(c.get() + 1));
    STRUCTOP_GLOBAL_COUNT.fetch_add(1, SeqCst);
    STRUCTOP_KFUNC_COUNT_LOCAL.with(|c| c.set(0));
}

/// Read current structop tracking state.
pub fn structop_info() -> StructopInfo {
    StructopInfo {
        cpu_count: STRUCTOP_CPU_COUNT.with(|c| c.get()),
        global_count: STRUCTOP_GLOBAL_COUNT.load(SeqCst),
        rbc_total: STRUCTOP_RBC_TOTAL.with(|c| c.get()),
        kfunc_count: STRUCTOP_KFUNC_COUNT.with(|c| c.get()),
        interleave_count: STRUCTOP_INTERLEAVE_COUNT.with(|c| c.get()),
        ops_context: CURRENT_OPS_CONTEXT.with(|c| c.get()),
        kfunc_name: CURRENT_KFUNC_NAME.with(|c| c.get()),
        kfunc_count_local: STRUCTOP_KFUNC_COUNT_LOCAL.with(|c| c.get()),
    }
}

/// Print a summary table of per-CPU sched_ext structop statistics.
///
/// Prints structops (ops callback invocations), RBC counts, kfunc
/// counts, and interleaving counts per CPU. The RBC and interlv columns
/// are omitted when their totals are zero.
pub fn print_structop_summary(accum: &[StructopInfo]) {
    let total_structops: u64 = accum.iter().map(|a| a.cpu_count).sum();
    let total_rbc: u64 = accum.iter().map(|a| a.rbc_total).sum();
    let total_kfunc: u64 = accum.iter().map(|a| a.kfunc_count).sum();
    let total_interleave: u64 = accum.iter().map(|a| a.interleave_count).sum();

    if total_structops == 0 && total_kfunc == 0 {
        return;
    }

    let has_rbc = total_rbc > 0;
    let has_interlv = total_interleave > 0;

    // Build column definitions: (header, separator, per-row value extractor, total).
    // Always-present columns first, then conditionals.
    let print_row = |cpu_label: &dyn std::fmt::Display,
                     structops: &dyn std::fmt::Display,
                     rbc: &dyn std::fmt::Display,
                     kfuncs: &dyn std::fmt::Display,
                     interlv: &dyn std::fmt::Display| {
        print!("  {:>6}  {:>10}", cpu_label, structops);
        if has_rbc {
            print!("  {:>10}", rbc);
        }
        print!("  {:>10}", kfuncs);
        if has_interlv {
            print!("  {:>10}", interlv);
        }
        println!();
    };

    println!();
    println!("Sched_ext structop summary:");
    let sep = "----------";
    print_row(&"cpu", &"structops", &"rbc", &"kfuncs", &"interlv");
    print_row(&"------", &sep, &sep, &sep, &sep);

    for (i, a) in accum.iter().enumerate() {
        if a.cpu_count > 0 || a.rbc_total > 0 || a.kfunc_count > 0 || a.interleave_count > 0 {
            print_row(
                &i,
                &a.cpu_count,
                &a.rbc_total,
                &a.kfunc_count,
                &a.interleave_count,
            );
        }
    }

    print_row(&"------", &sep, &sep, &sep, &sep);
    print_row(
        &"total",
        &total_structops,
        &total_rbc,
        &total_kfunc,
        &total_interleave,
    );
}

/// Accumulate RBC consumed by a preemption into the per-worker total.
///
/// Called from the signal handler with the per-reset `rbc_count` delta.
/// The running total (`STRUCTOP_RBC_TOTAL`) becomes `structop_rbc` in
/// the preemption record. Despite the name, this is cumulative across
/// the entire dispatch round, not per-structop.
///
/// **Async-signal-safe**: uses only a thread-local cell.
pub fn record_rbc_preemption(timeslice: u64) {
    STRUCTOP_RBC_TOTAL.with(|c| c.set(c.get() + timeslice));
}

/// Increment the per-CPU kfunc yield counter (monotonic).
///
/// Called from `maybe_yield_preemptive()` on each cooperative yield.
pub fn inc_structop_kfunc() {
    STRUCTOP_KFUNC_COUNT.with(|c| c.set(c.get() + 1));
    STRUCTOP_KFUNC_COUNT_LOCAL.with(|c| c.set(c.get() + 1));
}

/// Increment the per-CPU interleave counter (monotonic).
///
/// Called from every yield site: signal preemptions, cooperative yields
/// (preemptive ring), and cooperative yields (token ring).
pub fn inc_interleave() {
    STRUCTOP_INTERLEAVE_COUNT.with(|c| c.set(c.get() + 1));
}

/// Detect structop boundary transitions and call `begin_structop()` when
/// entering a new ops callback.
pub fn maybe_begin_structop(in_ops: bool) {
    IN_STRUCTOP.with(|c| {
        if in_ops && !c.get() {
            c.set(true);
            begin_structop();
        } else if !in_ops {
            c.set(false);
        }
    });
}

/// Reset the per-worker structop counters (call when a worker finishes).
pub fn reset_structop_cpu_count() {
    STRUCTOP_CPU_COUNT.with(|c| c.set(0));
    STRUCTOP_RBC_TOTAL.with(|c| c.set(0));
    STRUCTOP_KFUNC_COUNT.with(|c| c.set(0));
    STRUCTOP_INTERLEAVE_COUNT.with(|c| c.set(0));
    IN_STRUCTOP.with(|c| c.set(false));
    CURRENT_KFUNC_NAME.with(|c| c.set(""));
    STRUCTOP_KFUNC_COUNT_LOCAL.with(|c| c.set(0));
    CURRENT_OPS_CONTEXT.with(|c| c.set(OpsContext::None));
}

/// Reset global structop count (call between dispatch rounds).
pub fn reset_structop_globals() {
    STRUCTOP_GLOBAL_COUNT.store(0, SeqCst);
}

/// Set the current kfunc name for preemption trace context.
///
/// Called at the start of each kfunc in `kfuncs.rs` before `maybe_yield()`.
/// The name is a short identifier (e.g. `"dsq_insert"`, `"kick_cpu"`).
pub fn set_current_kfunc(name: &'static str) {
    CURRENT_KFUNC_NAME.with(|c| c.set(name));
}

/// Read the current kfunc name.
pub fn current_kfunc_name() -> &'static str {
    CURRENT_KFUNC_NAME.with(|c| c.get())
}

/// Set the cached ops context from SimulatorState.
///
/// Called from structop boundary detection and yield paths to keep
/// the thread-local in sync with `sim.ops_context`.
pub fn set_current_ops_context(ctx: OpsContext) {
    CURRENT_OPS_CONTEXT.with(|c| c.set(ctx));
}

/// Read the per-thread cached ops context.
///
/// Used by the PMU signal handler to get ops_context without reading
/// shared `SimulatorState` (which may have been modified by another
/// worker during a yield).
pub fn current_ops_context() -> OpsContext {
    CURRENT_OPS_CONTEXT.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// ASLR base detection — .so-relative RIP offsets
// ---------------------------------------------------------------------------

/// Find the base address of the scheduler .so in the current process.
///
/// Parses `/proc/self/maps` looking for the first executable mapping from
/// a `libscx_*.so` file. Returns 0 if not found. The result can be
/// subtracted from an absolute RIP to get a .so-relative offset that
/// survives ASLR.
pub fn scheduler_so_base() -> u64 {
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in maps.lines() {
        // Executable mapping: look for 'r-xp' or 'r--xp' permission field
        // and a path containing "libscx_"
        if !line.contains("libscx_") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        // Check permissions field for executable
        if !parts[1].contains('x') {
            continue;
        }
        // Parse start address from "start-end"
        if let Some(start_str) = parts[0].split('-').next() {
            if let Ok(addr) = u64::from_str_radix(start_str, 16) {
                return addr;
            }
        }
    }
    0
}

/// Find the file path of the scheduler .so in the current process.
///
/// Parses `/proc/self/maps` looking for the first executable mapping from
/// a `libscx_*.so` file. Returns `None` if not found.
pub fn scheduler_so_path() -> Option<String> {
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(s) => s,
        Err(_) => return None,
    };
    for line in maps.lines() {
        if !line.contains("libscx_") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        if !parts[1].contains('x') {
            continue;
        }
        // The path is the last field.
        if let Some(path) = parts.last() {
            return Some(path.to_string());
        }
    }
    None
}

/// Compute an FNV-1a hash of the scheduler .so file contents.
///
/// Returns 0 if the .so file cannot be found or read.
pub fn compute_so_hash() -> u64 {
    let path = match scheduler_so_path() {
        Some(p) => p,
        None => return 0,
    };
    compute_so_hash_from_path(&path)
}

/// Compute an FNV-1a hash of a .so file at the given path.
///
/// Returns 0 if the file cannot be read.
pub fn compute_so_hash_from_path(path: &str) -> u64 {
    match std::fs::read(path) {
        Ok(bytes) => fnv1a_hash_bytes(&bytes),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// PreemptRing — futex-based token ring (signal-safe)
// ---------------------------------------------------------------------------

/// Per-worker state values.
const PARKED: u32 = 0;
const RUNNING: u32 = 1;

/// Signal-safe token ring using atomics and futex.
///
/// All methods are safe to call from signal handlers. The PRNG access is
/// serialized using a spinlock to ensure deterministic ordering.
///
/// NOTE: This intentionally uses a hand-rolled xorshift32 rather than
/// `rand::rngs::SmallRng` because the PRNG state must live in an `AtomicU32`
/// for signal-handler safety — standard library RNGs have multi-word state
/// that cannot be stored atomically.
pub struct PreemptRing {
    /// Per-worker state: `PARKED` or `RUNNING`.
    workers: Box<[AtomicU32]>,
    /// PRNG state (xorshift32). Access is serialized via CAS in `next_prng()`.
    prng: AtomicU32,
    /// Total number of workers.
    total: usize,
    /// Bitmask of finished workers (up to 64).
    finished_mask: AtomicU64,
    /// Orchestrator wake word: 0 = not all done, 1 = all done.
    all_done: AtomicU32,
    /// Count of signal-driven (PMU) preemptions (signal-safe increment).
    signal_preempt_count: AtomicU64,
    /// Count of cooperative yields at kfunc boundaries (safe Rust).
    cooperative_yield_count: AtomicU64,
    /// Storage for preemption records (instrumentation for determinism verification).
    preemption_records: PreemptionRecordStore,
}

impl PreemptRing {
    /// Create a new preemptive ring for `total` workers.
    ///
    /// # Panics
    /// Panics if `total` is 0 or exceeds 64.
    pub fn new(total: usize, seed: u32) -> Self {
        assert!(
            total > 0 && total <= 64,
            "PreemptRing supports 1–64 workers, got {total}"
        );
        let seed = if seed == 0 { 1 } else { seed };
        let workers: Box<[AtomicU32]> = (0..total).map(|_| AtomicU32::new(PARKED)).collect();
        PreemptRing {
            workers,
            prng: AtomicU32::new(seed),
            total,
            finished_mask: AtomicU64::new(0),
            all_done: AtomicU32::new(0),
            signal_preempt_count: AtomicU64::new(0),
            cooperative_yield_count: AtomicU64::new(0),
            preemption_records: PreemptionRecordStore::new(),
        }
    }

    /// Atomically advance the xorshift32 PRNG and return the new value.
    ///
    /// Uses CAS loop to ensure atomic read-modify-write, preventing races
    /// where two threads could load the same state and skip PRNG values.
    fn next_prng(&self) -> u32 {
        loop {
            let old = self.prng.load(SeqCst);
            let mut x = old;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if self.prng.compare_exchange(old, x, SeqCst, SeqCst).is_ok() {
                return x;
            }
            // CAS failed - another thread updated the PRNG; retry.
        }
    }

    fn pick_next(&self) -> Option<WorkerId> {
        let mask = self.finished_mask.load(SeqCst);
        let n_finished = mask.count_ones() as usize;
        let n_remaining = self.total - n_finished;
        if n_remaining == 0 {
            return None;
        }
        let idx = (self.next_prng() as usize) % n_remaining;
        let mut count = 0;
        for i in 0..self.total {
            if mask & (1u64 << i) == 0 {
                if count == idx {
                    return Some(WorkerId(i));
                }
                count += 1;
            }
        }
        unreachable!()
    }

    /// Roll a random timeslice in `[min, max]` using the ring's PRNG.
    pub fn roll_timeslice(&self, min: u64, max: u64) -> u64 {
        debug_assert!(max >= min);
        let range = max - min;
        if range == 0 {
            return min;
        }
        min + (self.next_prng() as u64) % (range + 1)
    }

    /// Increment the signal-driven preemption counter.
    ///
    /// **Async-signal-safe**: uses only an atomic fetch_add.
    pub fn inc_signal_preempt(&self) {
        self.signal_preempt_count.fetch_add(1, SeqCst);
    }

    /// Increment the cooperative yield counter.
    pub fn inc_cooperative_yield(&self) {
        self.cooperative_yield_count.fetch_add(1, SeqCst);
    }

    /// Total signal-driven (PMU) preemptions since creation.
    pub fn signal_preemptions(&self) -> u64 {
        self.signal_preempt_count.load(SeqCst)
    }

    /// Total cooperative yields (kfunc boundary) since creation.
    pub fn cooperative_yields(&self) -> u64 {
        self.cooperative_yield_count.load(SeqCst)
    }

    /// Record a preemption point (signal-safe).
    ///
    /// Called from the signal handler or cooperative yield path to capture
    /// the preemption state including structop context. Reads instruction
    /// bytes at the RIP for .so version mismatch detection during replay.
    ///
    /// Returns the sequence number assigned, or None if the buffer is full.
    pub fn record_preemption(
        &self,
        rbc_count: u64,
        instruction_pointer: u64,
        cpu_id: CpuId,
        worker_id: WorkerId,
        sinfo: StructopInfo,
    ) -> Option<u64> {
        let insn_bytes = read_insn_bytes_at(instruction_pointer);
        let seq = self.preemption_records.push(
            rbc_count,
            instruction_pointer,
            cpu_id,
            worker_id,
            sinfo,
            insn_bytes,
        );
        // Also collect to global store for test instrumentation.
        if let Some(s) = seq {
            maybe_collect_global(PreemptionRecord {
                rbc_count,
                instruction_pointer,
                cpu_id,
                worker_id,
                sequence: s,
                structop_local: sinfo.cpu_count,
                structop_global: sinfo.global_count,
                structop_rbc: sinfo.rbc_total,
                ops_context: sinfo.ops_context,
                kfunc_name: sinfo.kfunc_name,
                kfunc_count: sinfo.kfunc_count_local as u32,
                insn_bytes,
            });
        }
        seq
    }

    /// Retrieve all preemption records collected during the simulation.
    ///
    /// Returns records sorted by sequence number. Call this after the
    /// simulation completes to verify determinism.
    pub fn preemption_records(&self) -> Vec<PreemptionRecord> {
        self.preemption_records.drain()
    }

    /// Orchestrator: select the first worker via PRNG and wake it.
    pub fn start(&self) {
        if let Some(first) = self.pick_next() {
            self.workers[first.0].store(RUNNING, SeqCst);
            futex_wake(&self.workers[first.0], 1);
        }
    }

    /// Worker: block until this worker is selected.
    pub fn wait_for_token(&self, my_id: WorkerId) {
        loop {
            if self.workers[my_id.0].load(SeqCst) == RUNNING {
                break;
            }
            futex_wait(&self.workers[my_id.0], PARKED);
        }
    }

    /// Worker: release token, select next worker via PRNG, block until
    /// re-selected.
    ///
    /// **Async-signal-safe**: safe to call from signal handlers.
    ///
    /// Returns `true` if a different worker was selected (actual context
    /// switch), `false` if the PRNG re-selected the same worker (no-op yield).
    ///
    /// Ordering: parks self BEFORE waking next, preventing the race where
    /// the next worker yields back before we enter futex_wait.
    pub fn yield_token(&self, my_id: WorkerId) -> bool {
        // Park ourselves first to prevent wake-before-wait races.
        self.workers[my_id.0].store(PARKED, SeqCst);

        let switched = if let Some(next) = self.pick_next() {
            self.workers[next.0].store(RUNNING, SeqCst);
            futex_wake(&self.workers[next.0], 1);
            next != my_id
        } else {
            false
        };

        // Wait until re-selected.
        loop {
            if self.workers[my_id.0].load(SeqCst) != PARKED {
                break;
            }
            futex_wait(&self.workers[my_id.0], PARKED);
        }

        switched
    }

    /// Worker: mark as finished and wake the next worker (or signal
    /// all-done to the orchestrator).
    pub fn finish(&self, my_id: WorkerId) {
        self.finished_mask.fetch_or(1u64 << my_id.0, SeqCst);
        if self.finished_mask.load(SeqCst).count_ones() as usize == self.total {
            self.all_done.store(1, SeqCst);
            futex_wake(&self.all_done, 1);
        } else if let Some(next) = self.pick_next() {
            self.workers[next.0].store(RUNNING, SeqCst);
            futex_wake(&self.workers[next.0], 1);
        }
    }

    /// Orchestrator: block until all workers have finished.
    pub fn wait_all_done(&self) {
        loop {
            if self.all_done.load(SeqCst) != 0 {
                break;
            }
            futex_wait(&self.all_done, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local preemptive interleave context
// ---------------------------------------------------------------------------

/// Thread-local context for a worker participating in preemptive interleaving.
#[derive(Clone, Copy)]
struct PreemptCtx {
    ring: *const PreemptRing,
    worker_id: WorkerId,
    /// Raw fd of the RBC timer (for disable/enable in signal handler).
    timer_fd: RawFd,
    /// Raw fd of the RBC measurement counter (for cumulative C-code RBC).
    /// -1 if unavailable (no PMU support).
    measure_fd: RawFd,
    /// Timeslice range for PRNG consumption in `rearm_timer`.
    ///
    /// In recording mode, these bounds produce the random timeslice period.
    /// In replay mode, the timeslice is discarded (replay manages its own
    /// timer periods) but must be consumed to keep the PRNG sequence in
    /// sync with the recording run.
    timeslice_min: u64,
    timeslice_max: u64,
    /// Controls `rearm_timer` behavior at kfunc boundaries.
    ///
    /// When false (recording): reset counter, set new random period, enable.
    /// When true (replay): re-enable only — no reset, no period change.
    ///
    /// See `rearm_timer` docs for the full explanation of this asymmetry.
    replay_mode: bool,
}

// SAFETY: PreemptCtx holds a raw pointer to a PreemptRing that lives in
// a `thread::scope` block on the main thread. Access is serialized by
// the token-passing protocol.
unsafe impl Send for PreemptCtx {}

thread_local! {
    static PREEMPT_CTX: Cell<Option<PreemptCtx>> = const { Cell::new(None) };
}

/// Install preemptive interleave context on the current worker thread.
pub fn install(
    ring: &PreemptRing,
    worker_id: WorkerId,
    timer_fd: RawFd,
    measure_fd: RawFd,
    timeslice_min: u64,
    timeslice_max: u64,
) {
    PREEMPT_CTX.with(|c| {
        c.set(Some(PreemptCtx {
            ring: ring as *const PreemptRing,
            worker_id,
            timer_fd,
            measure_fd,
            timeslice_min,
            timeslice_max,
            replay_mode: false,
        }));
    });
}

/// Remove preemptive interleave context from the current thread.
pub fn uninstall() {
    PREEMPT_CTX.with(|c| c.set(None));
}

/// Install preemptive interleave context for replay mode.
///
/// Like [`install`], but sets `replay_mode = true` so that `rearm_timer`
/// re-enables the counter without resetting or changing the period (see
/// `rearm_timer` docs for the full recording/replay asymmetry explanation).
/// The replay backend manages the timer period directly via
/// [`arm_replay_timer_pub`] and [`arm_replay_next_target`].
///
/// `timeslice_min` and `timeslice_max` must match the recording scenario's
/// values so that `rearm_timer`'s PRNG consumption produces the same
/// sequence as the recording run. The rolled timeslice is discarded in
/// replay mode, but consuming it keeps `pick_next()` deterministic.
/// Also sets `measure_fd = -1` (replay doesn't use a measurement counter).
pub fn install_replay_preempt(
    ring: &PreemptRing,
    worker_id: WorkerId,
    timer_fd: RawFd,
    timeslice_min: u64,
    timeslice_max: u64,
) {
    PREEMPT_CTX.with(|c| {
        c.set(Some(PreemptCtx {
            ring: ring as *const PreemptRing,
            worker_id,
            timer_fd,
            measure_fd: -1,
            timeslice_min,
            timeslice_max,
            replay_mode: true,
        }));
    });
}

// ---------------------------------------------------------------------------
// Cooperative yield via PreemptRing (replaces interleave::maybe_yield)
// ---------------------------------------------------------------------------

/// Whether a cooperative kfunc yield occurs before or after the kfunc body.
///
/// Pre-kfunc yields happen before `with_sim()` — the timer is left disabled
/// and `with_sim()` re-arms it via `resume_timer()`.
///
/// Post-kfunc yields happen inside `with_sim()` after `resume_timer()` — the
/// timer was just re-armed, so we must disable it before yielding and re-arm
/// it again on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KfuncYieldPhase {
    /// Yield BEFORE the kfunc body executes.
    Pre,
    /// Yield AFTER the kfunc body completes.
    Post,
}

impl std::fmt::Display for KfuncYieldPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KfuncYieldPhase::Pre => write!(f, "kfunc"),
            KfuncYieldPhase::Post => write!(f, "kfunc-post"),
        }
    }
}

/// Cooperative yield point for kfunc entry (using the PreemptRing).
///
/// Functionally identical to [`interleave::maybe_yield`] but uses the
/// futex-based `PreemptRing` instead of `Mutex`/`Condvar` `TokenRing`.
///
/// The PMU timer is disabled on entry and stays disabled on return.
/// The caller (via `with_sim()` -> `resume_timer()`) is responsible for
/// re-arming the timer before returning to scheduler C code.
///
/// # Safety contract
///
/// Must be called BEFORE `with_sim()`, so no `&mut SimulatorState`
/// reference exists when the worker yields.
pub fn maybe_yield_preemptive() {
    cooperative_yield_impl(KfuncYieldPhase::Pre);
}

/// Post-kfunc cooperative yield point (using the PreemptRing).
///
/// Called from `with_sim()` after `resume_timer()`. The timer was just
/// re-armed, so this function disables it before yielding and re-arms
/// it on resume. This gives us a guaranteed interleaving point after
/// every kfunc completes, catching concurrency bugs that only manifest
/// when another worker runs between consecutive kfuncs.
///
/// # Safety contract
///
/// Must be called when no `&mut SimulatorState` reference exists.
/// Inside `with_sim()`, the `&mut` borrow ends when `f(sim)` returns,
/// so calling this after `resume_timer()` is safe.
pub fn maybe_yield_preemptive_post() {
    cooperative_yield_impl(KfuncYieldPhase::Post);
}

/// Shared implementation for pre- and post-kfunc cooperative yields.
///
/// Saves/restores `SimulatorState` per-callback context across the yield,
/// manages the PMU timer, and records instrumentation.
fn cooperative_yield_impl(phase: KfuncYieldPhase) {
    let ctx = PREEMPT_CTX.with(|c| c.get());
    let ctx = match ctx {
        Some(ctx) => ctx,
        None => return,
    };

    // SAFETY: `ctx.ring` was set from a valid `&PreemptRing` reference in
    // `install()`. The PreemptRing lives in a `thread::scope` block and
    // outlives all worker threads.
    let ring = unsafe { &*ctx.ring };

    // Disable the PMU timer during the cooperative yield to prevent
    // a preemptive signal from firing while we're in Rust/yield code.
    disable_timer(ctx.timer_fd);

    // Save per-callback context from CALLBACK_CTX thread-local.
    let saved = crate::kfuncs::get_callback_ctx()
        .expect("cooperative_yield_impl called outside simulator context");

    let saved_ops_ctx = saved.ops_context;

    // Detect structop boundary transitions.
    let in_ops = saved_ops_ctx != crate::kfuncs::OpsContext::None;
    maybe_begin_structop(in_ops);

    // Cache ops context for structop_info() to read.
    set_current_ops_context(saved_ops_ctx);

    // Increment per-structop kfunc yield counter.
    inc_structop_kfunc();

    let sinfo = structop_info();
    let ops = sinfo.ops_context.short_name();
    let kfn = if sinfo.kfunc_name.is_empty() {
        "none"
    } else {
        sinfo.kfunc_name
    };

    // Release token and block until re-selected (futex-based).
    ring.inc_cooperative_yield();
    tracing::debug!(
        "preempt:{phase} cooperative, ops={ops} kfunc={kfn} structop#{0}:{1} kfunc#{2} (rbc={3})",
        sinfo.cpu_count,
        sinfo.global_count,
        sinfo.kfunc_count,
        sinfo.rbc_total,
    );
    // Pause measurement counter during yield (don't count parked time).
    disable_measurement(ctx.measure_fd);
    if ring.yield_token(ctx.worker_id) {
        inc_interleave();
    }

    // Resumed — restore our context.
    tracing::debug!(
        "preempt: resumed ({phase}), ops={ops} kfunc={kfn} structop#{0}:{1}",
        sinfo.cpu_count,
        sinfo.global_count,
    );
    // Resume measurement counter now that we're running again.
    enable_measurement(ctx.measure_fd);
    crate::kfuncs::install_callback_ctx(saved);

    // Timer management depends on the phase:
    // - Pre: stays disabled — with_sim() will re-arm via resume_timer().
    // - Post: re-arm — we're about to return to scheduler C code.
    if phase == KfuncYieldPhase::Post {
        rearm_timer(ring, &ctx);
    }
}

// ---------------------------------------------------------------------------
// Timer pause/resume for with_sim() bracketing
// ---------------------------------------------------------------------------

/// Pause the preemption timer to prevent signals during kfunc execution.
///
/// Called by `with_sim()` before accessing `SimulatorState`. No-op if
/// preemptive interleaving is not active on this thread.
///
/// **Async-signal-safe**: uses only a thread-local read and an ioctl.
pub fn pause_timer() {
    if let Some(ctx) = PREEMPT_CTX.with(|c| c.get()) {
        disable_timer(ctx.timer_fd);
    }
}

/// Resume the preemption timer after kfunc execution completes.
///
/// Called by `with_sim()` just before returning to scheduler C code.
/// Delegates to `rearm_timer`, which handles the recording/replay
/// difference: recording resets the counter with a new random period,
/// while replay just re-enables without resetting. In both modes,
/// one PRNG value is consumed for deterministic sequencing. No-op if
/// preemptive interleaving is not active on this thread.
pub fn resume_timer() {
    if let Some(ctx) = PREEMPT_CTX.with(|c| c.get()) {
        // SAFETY: `ctx.ring` is a valid pointer set during `install()`.
        let ring = unsafe { &*ctx.ring };
        rearm_timer(ring, &ctx);
    }
}

/// Pause the RBC measurement counter during kfunc execution.
///
/// Called by `with_sim()` alongside `pause_timer()`. The measurement counter
/// tracks cumulative scheduler C-code RBC, excluding kfuncs. No-op if
/// preemptive interleaving is not active or no measurement counter exists.
pub fn pause_measurement() {
    if let Some(ctx) = PREEMPT_CTX.with(|c| c.get()) {
        disable_measurement(ctx.measure_fd);
    }
}

/// Resume the RBC measurement counter after kfunc execution.
///
/// Called by `with_sim()` alongside `resume_timer()`. No-op if preemptive
/// interleaving is not active or no measurement counter exists.
pub fn resume_measurement() {
    if let Some(ctx) = PREEMPT_CTX.with(|c| c.get()) {
        enable_measurement(ctx.measure_fd);
    }
}

// ---------------------------------------------------------------------------
// Signal-safe stderr writer (no allocation, no locks)
// ---------------------------------------------------------------------------

/// Fixed-size stack buffer that implements `fmt::Write` for use in signal
/// handlers. Writes into a `[u8; N]` without allocation — excess bytes
/// are silently dropped.
struct StackWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> StackWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}

impl FmtWrite for StackWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let n = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

/// Write a pre-formatted message to stderr (async-signal-safe).
///
/// Uses raw `libc::write(STDERR_FILENO, ...)` which is guaranteed
/// async-signal-safe by POSIX.
fn write_stderr(buf: &[u8]) {
    // SAFETY: `libc::write` to STDERR_FILENO is async-signal-safe per POSIX.
    // `buf.as_ptr()` is valid for `buf.len()` bytes.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            buf.as_ptr() as *const libc::c_void,
            buf.len(),
        );
    }
}

// ---------------------------------------------------------------------------
// Signal handler (preemptive yield)
// ---------------------------------------------------------------------------

/// The preemptive signal number. SIGSTKFLT is unused by the kernel.
pub const PREEMPT_SIGNAL: libc::c_int = libc::SIGSTKFLT;

/// Install the process-wide SIGSTKFLT signal handler.
///
/// Must be called before spawning worker threads. The handler is
/// async-signal-safe: it uses only atomics, futex, and raw ioctls.
pub fn install_signal_handler() {
    let sa = libc::sigaction {
        sa_sigaction: preempt_handler as *const () as libc::sighandler_t,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t` (all-zeros = empty set).
        sa_mask: unsafe { std::mem::zeroed() },
        // SA_SIGINFO so we get siginfo_t; SA_RESTART to not fail slow
        // syscalls (though we don't expect any during C scheduler code).
        sa_flags: libc::SA_SIGINFO | libc::SA_RESTART,
        sa_restorer: None,
    };
    // SAFETY: `sa` is a valid sigaction struct. `sigaction` is async-signal-safe.
    let ret = unsafe { libc::sigaction(PREEMPT_SIGNAL, &sa, std::ptr::null_mut()) };
    assert_eq!(ret, 0, "failed to install SIGSTKFLT handler");
}

/// Disable the SIGSTKFLT signal handler after a preemptive interleave.
///
/// Sets the handler to `SIG_IGN` rather than `SIG_DFL` to avoid a race
/// condition: a PMU timer may have already queued a SIGSTKFLT that has
/// not yet been delivered. Under `SIG_DFL`, SIGSTKFLT terminates the
/// process. Under `SIG_IGN`, the stray signal is harmlessly discarded.
/// (See sim-c3fd09.)
pub fn uninstall_signal_handler() {
    let sa = libc::sigaction {
        sa_sigaction: libc::SIG_IGN,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    // SAFETY: `sa` is a valid sigaction struct. Sets handler to SIG_IGN.
    unsafe {
        libc::sigaction(PREEMPT_SIGNAL, &sa, std::ptr::null_mut());
    }
}

/// Extract the instruction pointer (RIP) from a ucontext on x86-64.
///
/// Returns 0 if the context is null or on non-x86-64 platforms.
#[cfg(target_arch = "x86_64")]
fn extract_rip_from_ucontext(ctx: *mut libc::c_void) -> u64 {
    if ctx.is_null() {
        return 0;
    }
    // SAFETY: `ctx` is a non-null ucontext_t pointer provided by the
    // kernel's signal delivery mechanism. REG_RIP (index 16) is valid
    // on x86-64 Linux.
    unsafe {
        let uc = ctx as *const libc::ucontext_t;
        // REG_RIP is index 16 on x86-64 Linux (from sys/ucontext.h).
        (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn extract_rip_from_ucontext(_ctx: *mut libc::c_void) -> u64 {
    0 // Not supported on non-x86-64 platforms.
}

/// Read the current RBC count from the timer fd (signal-safe).
///
/// Returns 0 if the fd is invalid or read fails.
fn read_rbc_count(timer_fd: RawFd) -> u64 {
    if timer_fd < 0 {
        return 0;
    }
    let mut count: u64 = 0;
    // SAFETY: `timer_fd` is a valid perf_event fd. `libc::read` is
    // async-signal-safe. Reading 8 bytes from a perf_event fd returns
    // the current counter value.
    let ret = unsafe {
        libc::read(
            timer_fd,
            &mut count as *mut u64 as *mut libc::c_void,
            std::mem::size_of::<u64>(),
        )
    };
    if ret < 0 {
        0
    } else {
        count
    }
}

/// Signal handler for preemptive interleaving.
///
/// Called on SIGSTKFLT delivery (PMU counter overflow). All operations
/// here are async-signal-safe.
extern "C" fn preempt_handler(
    _signo: libc::c_int,
    _info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    let pctx = PREEMPT_CTX.with(|c| c.get());
    let pctx = match pctx {
        Some(ctx) => ctx,
        None => return, // Not in a preemptive interleave context.
    };

    // SAFETY: `pctx.ring` is a valid pointer set during `install()`.
    // The PreemptRing lives in a `thread::scope` block and outlives workers.
    let ring = unsafe { &*pctx.ring };

    // 1. Disable PMU timer to prevent recursive signals.
    disable_timer(pctx.timer_fd);

    // 1a. Pause measurement counter immediately — the signal handler's own
    //     branches should not contribute to the scheduler overhead RBC count.
    //     Without this, the nondeterministic timing of signal delivery adds
    //     a variable number of handler branches to the RBC total.
    disable_measurement(pctx.measure_fd);

    // 2. Capture preemption instrumentation BEFORE any other work.
    //    Extract RIP from ucontext and read current RBC count.
    let instruction_pointer = extract_rip_from_ucontext(ctx);
    let rbc_count = read_rbc_count(pctx.timer_fd);

    // 3. Read per-callback context from CALLBACK_CTX (async-signal-safe).
    let saved = match crate::kfuncs::get_callback_ctx() {
        Some(c) => c,
        None => return,
    };

    // Read ops_context from per-thread TLS rather than the shared
    // SimulatorState. The shared state may have been modified by
    // another worker that ran while this worker was yielded (e.g.
    // Worker B's exit_sim cleared ops_context to None). The TLS
    // copy was set by set_ops_context() when the engine entered
    // this callback, so it reflects this worker's true context.
    let saved_ops_ctx = current_ops_context();

    // 4. Track structop RBC and emit trace (signal-safe stderr write).
    record_rbc_preemption(rbc_count);
    // Cache ops context so structop_info() picks it up.
    set_current_ops_context(saved_ops_ctx);
    let sinfo = structop_info();

    // Emit preempt:pmu trace to stderr if TRACE level is enabled.
    // We cannot use tracing::trace!() here — it uses internal mutexes and
    // is NOT async-signal-safe. Instead we check the tracing level filter
    // (atomic read, signal-safe) and write directly to stderr.
    if tracing::level_filters::LevelFilter::current() >= tracing::Level::TRACE {
        let ops = sinfo.ops_context.short_name();
        let kfn = if sinfo.kfunc_name.is_empty() {
            "none"
        } else {
            sinfo.kfunc_name
        };
        let mut buf = [0u8; 256];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "preempt:pmu ops={} kfunc={} structop#{}:{} rbc={} rip=0x{:x}",
            ops, kfn, sinfo.cpu_count, sinfo.global_count, sinfo.rbc_total, instruction_pointer,
        );
        write_stderr(w.as_bytes());
    }

    // 4a. Record the preemption point (with structop context).
    ring.record_preemption(
        rbc_count,
        instruction_pointer,
        saved.current_cpu,
        pctx.worker_id,
        sinfo,
    );

    // 5. Measurement counter was already paused at the top of the handler
    //    (step 1a), so we don't need to disable it again before yielding.

    // 6. Yield token (futex-based, signal-safe). Blocks until re-selected.
    ring.inc_signal_preempt(); // atomic, signal-safe
    if ring.yield_token(pctx.worker_id) {
        inc_interleave(); // TLS, safe (signal masked during handler)
    }

    // 7. Resumed — restore context.
    crate::kfuncs::install_callback_ctx(saved);

    // 8. Resume measurement counter now that we're running again.
    enable_measurement(pctx.measure_fd);

    // 9. Do NOT re-arm the timer here. With small timeslices (e.g. 1 RBC),
    //    re-arming inside the handler causes a livelock: the timer overflows
    //    during the handler's own return code, the pending signal fires
    //    immediately (SIGSTKFLT is blocked during the handler, no SA_NODEFER),
    //    and the main code never advances.
    //
    //    Instead, the timer is re-armed naturally by `with_sim()`'s
    //    `resume_timer()` when the next kfunc returns to C code. This
    //    ensures the timer only fires during C scheduler code.
}

// ---------------------------------------------------------------------------
// Timer helpers (raw ioctls — async-signal-safe)
// ---------------------------------------------------------------------------

/// Disable the PMU timer. No-op if fd is -1 (no timer).
fn disable_timer(fd: RawFd) {
    if fd < 0 {
        return;
    }
    // SAFETY: `fd` is a valid perf_event fd. PERF_IOC_DISABLE is a valid
    // ioctl for perf_event fds. Async-signal-safe.
    unsafe {
        libc::ioctl(fd, scx_perf::PERF_IOC_DISABLE, 0 as libc::c_ulong);
    }
}

/// Disable the measurement counter. No-op if fd is -1.
fn disable_measurement(fd: RawFd) {
    if fd < 0 {
        return;
    }
    // SAFETY: `fd` is a valid perf_event fd. Async-signal-safe.
    unsafe {
        libc::ioctl(fd, scx_perf::PERF_IOC_DISABLE, 0 as libc::c_ulong);
    }
}

/// Enable the measurement counter. No-op if fd is -1.
fn enable_measurement(fd: RawFd) {
    if fd < 0 {
        return;
    }
    // SAFETY: `fd` is a valid perf_event fd. Async-signal-safe.
    unsafe {
        libc::ioctl(fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }
}

/// Re-arm the PMU timer after returning from a kfunc to scheduler C code.
///
/// Called from `resume_timer()` and post-kfunc cooperative yield. Always
/// consumes one PRNG value (`roll_timeslice`) to keep the deterministic
/// token-passing sequence in sync between recording and replay.
///
/// # Recording vs replay counter semantics
///
/// **Recording mode**: resets the PMU counter to zero and programs a new
/// random timeslice period. Each kfunc boundary starts a fresh counting
/// window. The signal handler reads the counter as `rbc_count` (a
/// per-reset delta), and accumulates it into the cumulative
/// `STRUCTOP_RBC_TOTAL` (stored as `structop_rbc` in the trace).
///
/// **Replay mode**: re-enables the counter WITHOUT resetting. The PMU
/// counter runs continuously from `arm()` across all kfunc boundaries
/// within a dispatch round. Replay signal handlers compare the live
/// counter against cumulative `structop_rbc` values from the trace.
/// Resetting at kfunc boundaries would destroy the cumulative count
/// that the replay handlers need for targeting. The PRNG is still
/// consumed (result discarded) to keep the deterministic sequence in
/// sync with recording.
///
/// This asymmetry is intentional and correct: recording accumulates
/// per-reset deltas into a cumulative total, while replay uses the
/// cumulative total directly as a continuous counter target.
fn rearm_timer(ring: &PreemptRing, ctx: &PreemptCtx) {
    let fd = ctx.timer_fd;
    if fd < 0 {
        return;
    }
    // Consume PRNG unconditionally for deterministic sequencing.
    let timeslice = ring.roll_timeslice(ctx.timeslice_min, ctx.timeslice_max);
    if ctx.replay_mode {
        // Re-enable only -- counter must run continuously for replay
        // handlers that compare against cumulative structop_rbc targets.
        // SAFETY: `fd` is a valid perf_event fd.
        unsafe {
            libc::ioctl(fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
        }
        return;
    }
    // Recording: reset + new period + enable.
    let mut period = timeslice;
    // SAFETY: `fd` is a valid perf_event fd. These ioctls reset the
    // counter, set a new overflow period, and enable counting.
    unsafe {
        libc::ioctl(fd, scx_perf::PERF_IOC_RESET, 0 as libc::c_ulong);
        libc::ioctl(fd, scx_perf::PERF_IOC_PERIOD, &mut period as *mut u64);
        libc::ioctl(fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }
}

// ---------------------------------------------------------------------------
// Replay engine — hybrid PMU + hardware breakpoint
// ---------------------------------------------------------------------------

/// Margin (in retired conditional branches) before the target at which the
/// PMU timer fires, giving us time to arm the hardware breakpoint.
///
/// Must be well above typical PMU skid (~30-100 branches). 200 provides
/// comfortable headroom.
pub const REPLAY_MARGIN: u64 = 200;

/// The signal used by hardware breakpoints in replay mode.
pub const REPLAY_BP_SIGNAL: libc::c_int = libc::SIGTRAP;

/// Signal-safe cursor into a worker's replay trace.
///
/// Tracks which preemption target is next and accumulates signal-handler
/// overhead for rdpmc accounting. All mutable state uses atomics for
/// signal-handler safety.
pub struct ReplayCursor {
    /// The preemption targets for this worker.
    targets: Vec<PreemptionRecord>,
    /// Index of the next target (atomic for signal-handler access).
    next_idx: AtomicUsize,
    /// Accumulated signal handler branch overhead (for rdpmc accounting).
    total_overhead: AtomicU64,
}

impl ReplayCursor {
    /// Create a cursor from a worker's trace slice.
    pub fn new(targets: Vec<PreemptionRecord>) -> Self {
        ReplayCursor {
            targets,
            next_idx: AtomicUsize::new(0),
            total_overhead: AtomicU64::new(0),
        }
    }

    /// Get the next target, if any remain.
    pub fn current_target(&self) -> Option<&PreemptionRecord> {
        let idx = self.next_idx.load(SeqCst);
        self.targets.get(idx)
    }

    /// Advance to the next target. Returns true if there are more targets.
    fn advance(&self) -> bool {
        let idx = self.next_idx.fetch_add(1, SeqCst) + 1;
        idx < self.targets.len()
    }

    /// Number of targets in this cursor.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether there are no targets.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Accumulated overhead branches from signal-handler execution.
    #[allow(dead_code)] // Infrastructure for rdpmc accounting.
    pub fn overhead(&self) -> u64 {
        self.total_overhead.load(SeqCst)
    }

    /// Clone the target list (for creating reset copies of cursors).
    pub fn clone_targets(&self) -> Vec<PreemptionRecord> {
        self.targets.clone()
    }

    /// Reset the cursor to the beginning (for retry logic).
    pub fn reset(&self) {
        self.next_idx.store(0, SeqCst);
        self.total_overhead.store(0, SeqCst);
    }
}

/// Thread-local context for a worker in REPLAY mode.
#[derive(Clone, Copy)]
struct ReplayCtx {
    ring: *const PreemptRing,
    worker_id: WorkerId,
    /// Raw fd of the RBC timer.
    timer_fd: RawFd,
    /// Raw fd of the hardware breakpoint.
    bp_fd: RawFd,
    /// Pointer to the per-worker replay cursor.
    /// Raw pointer because it must be accessible from a signal handler.
    cursor: *const ReplayCursor,
    /// When true, use breakpoint-only mode (no PMU timer signal).
    /// In PMU mode the timer is disabled before the breakpoint fires,
    /// so the RBC count is frozen and must NOT be checked.
    no_pmu_signal: bool,
}

// SAFETY: ReplayCtx holds raw pointers to PreemptRing and ReplayCursor
// that live in a `thread::scope` block. Access is serialized by the
// token-passing protocol.
unsafe impl Send for ReplayCtx {}

thread_local! {
    static REPLAY_CTX: Cell<Option<ReplayCtx>> = const { Cell::new(None) };
}

/// Install replay context on the current worker thread.
pub fn install_replay(
    ring: &PreemptRing,
    worker_id: WorkerId,
    timer_fd: RawFd,
    bp_fd: RawFd,
    cursor: &ReplayCursor,
    no_pmu_signal: bool,
) {
    REPLAY_CTX.with(|c| {
        c.set(Some(ReplayCtx {
            ring: ring as *const PreemptRing,
            worker_id,
            timer_fd,
            bp_fd,
            cursor: cursor as *const ReplayCursor,
            no_pmu_signal,
        }));
    });
}

/// Remove replay context from the current thread.
pub fn uninstall_replay() {
    REPLAY_CTX.with(|c| c.set(None));
}

/// Install the process-wide replay signal handlers.
///
/// Installs both the PMU handler (SIGSTKFLT) and the breakpoint handler
/// (SIGTRAP). Must be called before spawning worker threads.
pub fn install_replay_signal_handlers() {
    // PMU handler: fires when we're within MARGIN of the target.
    let sa_pmu = libc::sigaction {
        sa_sigaction: replay_pmu_handler as *const () as libc::sighandler_t,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: libc::SA_SIGINFO | libc::SA_RESTART,
        sa_restorer: None,
    };
    // SAFETY: `sa_pmu` is a valid sigaction struct.
    let ret = unsafe { libc::sigaction(PREEMPT_SIGNAL, &sa_pmu, std::ptr::null_mut()) };
    assert_eq!(ret, 0, "failed to install replay PMU handler");

    // Breakpoint handler: fires when we hit the target instruction.
    let sa_bp = libc::sigaction {
        sa_sigaction: replay_bp_handler as *const () as libc::sighandler_t,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: libc::SA_SIGINFO | libc::SA_RESTART,
        sa_restorer: None,
    };
    // SAFETY: `sa_bp` is a valid sigaction struct.
    let ret = unsafe { libc::sigaction(REPLAY_BP_SIGNAL, &sa_bp, std::ptr::null_mut()) };
    assert_eq!(ret, 0, "failed to install replay breakpoint handler");
}

/// Disable the replay signal handlers after a replay interleave.
///
/// Sets SIGSTKFLT to `SIG_IGN` (not `SIG_DFL`) to avoid a race where a
/// pending PMU signal arrives after teardown and kills the process.
/// SIGTRAP is restored to `SIG_DFL` since hardware breakpoints are
/// explicitly disarmed before teardown and SIGTRAP's default (core dump)
/// is the expected behavior for unexpected traps. (See sim-c3fd09.)
pub fn uninstall_replay_signal_handlers() {
    let sa_ignore = libc::sigaction {
        sa_sigaction: libc::SIG_IGN,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    let sa_default = libc::sigaction {
        sa_sigaction: libc::SIG_DFL,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    // SAFETY: Both sigaction structs are valid. SIG_IGN for PREEMPT_SIGNAL
    // prevents stray PMU signals from killing the process. SIG_DFL for
    // REPLAY_BP_SIGNAL restores default SIGTRAP behavior.
    unsafe {
        libc::sigaction(PREEMPT_SIGNAL, &sa_ignore, std::ptr::null_mut());
        libc::sigaction(REPLAY_BP_SIGNAL, &sa_default, std::ptr::null_mut());
    }
}

/// Arm the PMU timer for the next replay target.
///
/// Sets the timer to fire at `target_rbc - REPLAY_MARGIN` branches from
/// the current counter position (which is reset to zero).
fn arm_replay_timer(timer_fd: RawFd, target_rbc: u64) {
    if timer_fd < 0 {
        return;
    }
    let mut period = target_rbc.saturating_sub(REPLAY_MARGIN).max(1);
    // SAFETY: `timer_fd` is a valid perf_event fd. These ioctls reset the
    // counter, set a new overflow period, and enable counting.
    unsafe {
        libc::ioctl(timer_fd, scx_perf::PERF_IOC_RESET, 0 as libc::c_ulong);
        libc::ioctl(timer_fd, scx_perf::PERF_IOC_PERIOD, &mut period as *mut u64);
        libc::ioctl(timer_fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }
}

/// Public wrapper for [`arm_replay_timer`], used by the backend to arm the
/// first replay target before entering scheduler C code.
pub fn arm_replay_timer_pub(timer_fd: RawFd, target_rbc: u64) {
    arm_replay_timer(timer_fd, target_rbc);
}

/// Arm the hardware breakpoint at the given instruction pointer.
fn arm_breakpoint(bp_fd: RawFd, addr: u64) {
    if bp_fd < 0 || addr == 0 {
        return;
    }
    // Build a fresh perf_event_attr for the new address.
    let mut attr = perf_event_open_sys::bindings::perf_event_attr {
        type_: perf_event_open_sys::bindings::PERF_TYPE_BREAKPOINT,
        size: std::mem::size_of::<perf_event_open_sys::bindings::perf_event_attr>() as u32,
        bp_type: perf_event_open_sys::bindings::HW_BREAKPOINT_X,
        ..Default::default()
    };
    attr.__bindgen_anon_3.bp_addr = addr;
    attr.__bindgen_anon_4.bp_len = perf_event_open_sys::bindings::HW_BREAKPOINT_LEN_8 as u64;
    attr.__bindgen_anon_1.sample_period = 1;
    attr.__bindgen_anon_2.wakeup_events = 1;
    attr.set_disabled(0); // enable immediately after modify
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_pinned(1);

    // SAFETY: `bp_fd` is a valid perf_event fd. PERF_IOC_MODIFY_ATTRIBUTES
    // updates the breakpoint address. PERF_IOC_ENABLE starts monitoring.
    // `attr` is a valid perf_event_attr struct on the stack.
    unsafe {
        libc::ioctl(
            bp_fd,
            scx_perf::PERF_IOC_MODIFY_ATTRIBUTES,
            &mut attr as *mut perf_event_open_sys::bindings::perf_event_attr,
        );
        libc::ioctl(bp_fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }
}

/// Whether the PMU-skid warning has been shown during this replay run.
///
/// Set to `true` after the first PMU signal handler invocation so the
/// warning prints at most once per replay run.
static REPLAY_PMU_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

/// Set by `replay_pmu_handler` when PMU skid overshoots the target RBC.
///
/// The outer retry loop in the replay dispatch code checks this flag
/// after each dispatch round completes. When set, the round's results
/// are discarded and retried (up to the configured attempt limits).
pub static REPLAY_OVERSHOT: AtomicBool = AtomicBool::new(false);

/// Reset replay overshoot / warning state for a new replay attempt.
///
/// Call before each retry in the outer replay loop.
pub fn reset_replay_state() {
    REPLAY_PMU_WARNING_SHOWN.store(false, SeqCst);
    REPLAY_OVERSHOT.store(false, SeqCst);
}

/// PMU signal handler for replay mode (SIGSTKFLT).
///
/// Fires when we're within REPLAY_MARGIN branches of the target. Arms
/// the hardware breakpoint at the target's instruction pointer.
///
/// Emits a one-time warning about non-deterministic PMU skid and detects
/// overshoot conditions where the PMU fires past the target RBC.
///
/// All operations are async-signal-safe.
extern "C" fn replay_pmu_handler(
    _signo: libc::c_int,
    _info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    let rctx = REPLAY_CTX.with(|c| c.get());
    let rctx = match rctx {
        Some(ctx) => ctx,
        None => return,
    };

    // 1. Disable PMU timer to prevent recursive signals.
    disable_timer(rctx.timer_fd);

    // 1a. One-time warning about non-deterministic PMU skid.
    if !REPLAY_PMU_WARNING_SHOWN.swap(true, SeqCst) {
        let mut buf = [0u8; 256];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "WARNING: replay using PMU signal approach — non-deterministic skid may \
             cause overshoot. Use --no-pmu-signal for guaranteed determinism.",
        );
        write_stderr(w.as_bytes());
    }

    // 2. Look up the next target from the cursor.
    // SAFETY: `rctx.cursor` is a valid pointer set during `install_replay()`.
    let cursor = unsafe { &*rctx.cursor };
    let target = match cursor.current_target() {
        Some(t) => t,
        None => return, // No more targets.
    };

    // 3. Read current cumulative RBC and check for overshoot.
    //    Uses structop_rbc (cumulative from arm()) because the counter
    //    runs continuously without resets at kfunc boundaries.
    let current_rbc = read_rbc_count(rctx.timer_fd);
    if current_rbc > target.structop_rbc {
        // PMU skid overshot the target. Mark the overshoot flag and
        // return without arming the breakpoint. The outer retry loop
        // will detect this and retry the dispatch round.
        REPLAY_OVERSHOT.store(true, SeqCst);
        let mut buf = [0u8; 256];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "REPLAY OVERSHOOT: PMU skid overshot target \
             (current_rbc={} > target_rbc={} at seq={}). \
             This replay attempt is corrupted.",
            current_rbc, target.structop_rbc, target.sequence,
        );
        write_stderr(w.as_bytes());
        return;
    }

    // 4. Arm the hardware breakpoint at the target instruction pointer.
    arm_breakpoint(rctx.bp_fd, target.instruction_pointer);

    // 5. Re-enable the PMU counter (without resetting) so that
    //    read_rbc_count() in the breakpoint handler returns the live
    //    cumulative value, not the frozen value from disable_timer().
    // SAFETY: `rctx.timer_fd` is a valid perf_event fd.
    unsafe {
        libc::ioctl(rctx.timer_fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }

    // 6. Return from signal handler — execution resumes with bp armed.
}

/// Validate structop name and count match between trace and replay.
///
/// Async-signal-safe: uses only StackWriter + write_stderr + abort.
/// Panics (aborts) with a clear message on mismatch.
fn replay_validate_structop(target: &PreemptionRecord, sinfo: &StructopInfo) {
    // Skip validation if overshoot detected (possibly concurrent).
    if REPLAY_OVERSHOT.load(SeqCst) {
        return;
    }

    // Validate ops context matches.
    if target.ops_context != sinfo.ops_context {
        let mut buf = [0u8; 512];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "REPLAY MISMATCH: ops context at seq={}: trace={}, replay={}",
            target.sequence,
            target.ops_context.short_name(),
            sinfo.ops_context.short_name(),
        );
        write_stderr(w.as_bytes());
        // SAFETY: `libc::abort()` is async-signal-safe per POSIX.
        unsafe { libc::abort() };
    }

    // Validate structop counts match.
    if target.structop_local != sinfo.cpu_count || target.structop_global != sinfo.global_count {
        let mut buf = [0u8; 512];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "REPLAY MISMATCH: structop at seq={}: trace={}:{}, replay={}:{}",
            target.sequence,
            target.structop_local,
            target.structop_global,
            sinfo.cpu_count,
            sinfo.global_count,
        );
        write_stderr(w.as_bytes());
        // SAFETY: `libc::abort()` is async-signal-safe per POSIX.
        unsafe { libc::abort() };
    }

    // Soft-check kfunc_count: warn but don't abort on mismatch.
    // This is a sub-coordinate for replay verification — divergence
    // here suggests the scheduler took a different kfunc path, which
    // may or may not be fatal depending on the scheduler's logic.
    let replay_kfunc_count = sinfo.kfunc_count_local as u32;
    if target.kfunc_count != 0 && replay_kfunc_count != target.kfunc_count {
        let mut buf = [0u8; 512];
        let mut w = StackWriter::new(&mut buf);
        let _ = writeln!(
            w,
            "REPLAY WARNING: kfunc_count at seq={}: trace={}, replay={}",
            target.sequence, target.kfunc_count, replay_kfunc_count,
        );
        write_stderr(w.as_bytes());
    }
}

/// Validate instruction bytes at the current RIP match the trace record.
///
/// Async-signal-safe: uses only StackWriter + write_stderr + abort.
/// Detects .so version mismatches by comparing the first [`INSN_BYTES_LEN`]
/// bytes of the instruction at the breakpoint address.
fn replay_validate_insn_bytes(target: &PreemptionRecord) {
    if target.instruction_pointer == 0 {
        return;
    }
    // Skip validation if overshoot detected (run is corrupt).
    if REPLAY_OVERSHOT.load(SeqCst) {
        return;
    }
    // Skip validation if the trace record has all-zero insn_bytes (old trace).
    if target.insn_bytes == [0u8; INSN_BYTES_LEN] {
        return;
    }
    let current = read_insn_bytes_at(target.instruction_pointer);
    if current != target.insn_bytes {
        let mut buf = [0u8; 512];
        let mut w = StackWriter::new(&mut buf);
        let _ = write!(
            w,
            "REPLAY MISMATCH: instruction bytes at seq={} rip=0x{:x}: trace=",
            target.sequence, target.instruction_pointer,
        );
        for b in &target.insn_bytes {
            let _ = write!(w, "{:02x}", b);
        }
        let _ = write!(w, ", replay=");
        for b in &current {
            let _ = write!(w, "{:02x}", b);
        }
        let _ = writeln!(w, " -- .so version mismatch?");
        write_stderr(w.as_bytes());
        // SAFETY: `libc::abort()` is async-signal-safe per POSIX.
        unsafe { libc::abort() };
    }
}

/// Breakpoint signal handler for replay mode (SIGTRAP).
///
/// Fires when execution hits the target instruction pointer. Preempts
/// the worker by yielding the token, then arms the timer for the next
/// target (if any).
///
/// All operations are async-signal-safe.
extern "C" fn replay_bp_handler(
    _signo: libc::c_int,
    _info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    let rctx = REPLAY_CTX.with(|c| c.get());
    let rctx = match rctx {
        Some(ctx) => ctx,
        None => return,
    };

    // SAFETY: `rctx.cursor` and `rctx.ring` are valid pointers set during
    // `install_replay()`. They live in a `thread::scope` block.
    let cursor = unsafe { &*rctx.cursor };
    let ring = unsafe { &*rctx.ring };

    // 0. If a PMU overshoot was already detected (possibly by a
    //    concurrent worker), this run is corrupt — bail out.
    if REPLAY_OVERSHOT.load(SeqCst) {
        return;
    }

    // 1. Disable breakpoint to prevent re-firing immediately.
    disable_timer(rctx.bp_fd);

    // 2. Read the current target (should exist since the PMU handler armed us).
    let target = match cursor.current_target() {
        Some(t) => *t,
        None => return,
    };

    // 2a. RBC count check -- breakpoint-only mode only.
    //
    // In breakpoint-only mode (no PMU timer signal), the breakpoint fires
    // on every execution of the target instruction. We check the RBC count
    // to find the right dynamic instance. If the current count is below
    // the target, re-enable the breakpoint and return.
    //
    // In PMU mode this check MUST be skipped: the PMU handler disabled the
    // timer before arming the breakpoint, so the RBC counter is frozen at
    // approximately `target_rbc - REPLAY_MARGIN + skid`. Checking it here
    // would always fail (frozen count < target) and cause an infinite loop
    // of breakpoint re-arms -- the root cause of the replay hang bug.
    if rctx.no_pmu_signal {
        let current_rbc = read_rbc_count(rctx.timer_fd);
        if current_rbc < target.structop_rbc {
            // Not the right instance yet -- re-enable breakpoint and return.
            arm_breakpoint(rctx.bp_fd, target.instruction_pointer);
            return;
        }
    }

    // 3. Read per-callback context from CALLBACK_CTX (async-signal-safe).
    let saved = match crate::kfuncs::get_callback_ctx() {
        Some(c) => c,
        None => return,
    };
    let saved_cpu = saved.current_cpu;
    let saved_ops_ctx = saved.ops_context;

    // 4. Track structop RBC.
    record_rbc_preemption(target.rbc_count);
    set_current_ops_context(saved_ops_ctx);
    let sinfo = structop_info();

    // 4a. Replay sanity checks (async-signal-safe: StackWriter + abort).
    replay_validate_structop(&target, &sinfo);
    replay_validate_insn_bytes(&target);

    // 4b. Record the preemption point (with structop context).
    ring.record_preemption(
        target.rbc_count,
        target.instruction_pointer,
        saved_cpu,
        rctx.worker_id,
        sinfo,
    );

    // 5. Yield token (futex-based, signal-safe).
    ring.inc_signal_preempt();
    ring.yield_token(rctx.worker_id);

    // 6. Resumed — restore context.
    crate::kfuncs::install_callback_ctx(saved);

    // 7. Advance cursor and arm timer/breakpoint for next target.
    let counter_now = read_rbc_count(rctx.timer_fd);
    if cursor.advance() {
        if let Some(next) = cursor.current_target() {
            arm_replay_next_target(
                rctx.timer_fd,
                rctx.bp_fd,
                next,
                counter_now,
                rctx.no_pmu_signal,
            );
        }
    }
}

/// Arm the next replay target using either PMU timer or breakpoint-only mode.
///
/// In normal (PMU) mode: arms the PMU timer to fire at
/// `target_rbc - REPLAY_MARGIN`. If the delta to the target is zero
/// (target is too close for the PMU to fire in time), falls back to
/// breakpoint-only for this specific target.
///
/// In breakpoint-only mode (`no_pmu_signal`): arms the breakpoint
/// directly at the target instruction pointer.
fn arm_replay_next_target(
    timer_fd: RawFd,
    bp_fd: RawFd,
    target: &PreemptionRecord,
    current_counter: u64,
    no_pmu_signal: bool,
) {
    if no_pmu_signal {
        // Breakpoint-only mode: arm the breakpoint directly.
        arm_breakpoint(bp_fd, target.instruction_pointer);
        return;
    }

    // PMU mode: compute delta to the REPLAY_MARGIN approach point.
    let delta = target
        .structop_rbc
        .saturating_sub(REPLAY_MARGIN)
        .saturating_sub(current_counter);

    if delta == 0 {
        // Target is too close for PMU — arm breakpoint directly
        // for this target (per-target bp-only fast path).
        arm_breakpoint(bp_fd, target.instruction_pointer);
        return;
    }

    let mut period = delta;
    // SAFETY: `timer_fd` is a valid perf_event fd. These ioctls set
    // the overflow period and enable counting.
    unsafe {
        libc::ioctl(timer_fd, scx_perf::PERF_IOC_PERIOD, &mut period as *mut u64);
        libc::ioctl(timer_fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
    }
}

/// Install replay signal handlers for breakpoint-only mode.
///
/// Installs only the breakpoint handler (SIGTRAP). No PMU signal handler
/// is installed since breakpoint-only mode does not use the PMU timer.
pub fn install_replay_bp_only_handlers() {
    // Breakpoint handler: fires on every execution of the target instruction.
    let sa_bp = libc::sigaction {
        sa_sigaction: replay_bp_handler as *const () as libc::sighandler_t,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: libc::SA_SIGINFO | libc::SA_RESTART,
        sa_restorer: None,
    };
    // SAFETY: `sa_bp` is a valid sigaction struct.
    let ret = unsafe { libc::sigaction(REPLAY_BP_SIGNAL, &sa_bp, std::ptr::null_mut()) };
    assert_eq!(ret, 0, "failed to install replay breakpoint handler");
}

/// Remove the replay breakpoint-only signal handler.
pub fn uninstall_replay_bp_only_handlers() {
    let sa_default = libc::sigaction {
        sa_sigaction: libc::SIG_DFL,
        // SAFETY: `std::mem::zeroed()` is valid for `sigset_t`.
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    // SAFETY: Restores SIGTRAP to default handler.
    unsafe {
        libc::sigaction(REPLAY_BP_SIGNAL, &sa_default, std::ptr::null_mut());
    }
}

/// Arm a hardware breakpoint directly for breakpoint-only replay mode.
///
/// Public wrapper for the backend to arm the first target without using
/// the PMU timer.
pub fn arm_replay_breakpoint_pub(bp_fd: RawFd, addr: u64) {
    arm_breakpoint(bp_fd, addr);
}

// ---------------------------------------------------------------------------
// e9patch preemption — shared state and extern "C" entry point
// ---------------------------------------------------------------------------

/// Fixed virtual address for the shared RBC state page.
///
/// Both the Rust backend and the e9-injected trampoline use this hardcoded
/// address — no RIP-relative addressing, no dlsym, no relocations. The Rust
/// backend mmaps a page here before loading the `_e9.so`.
///
/// Chosen at `0x1_E900_0000` (≈8GB), below typical ASLR base (~0x5555..),
/// above typical mmap base (~0x7fff..), in an obscure gap.
pub const E9_SHARED_ADDR: usize = 0x1E9_000_000;

/// Shared RBC state accessed by both the e9patch trampoline (C code injected
/// into the `.so` by e9tool) and the Rust backend (`E9PatchBackend`).
///
/// Mapped at [`E9_SHARED_ADDR`] via `mmap(MAP_FIXED)`. The trampoline reads
/// `counter` and `armed` on the fast path (via hardcoded `movabs`).
/// The `.so`'s `e9_arm()` / `e9_disarm()` write here.
///
/// Only `counter`, `armed`, and `yield_fn` are used by the trampoline.
/// The `ring_ptr` and `worker_id` fields are NOT in this struct — they
/// come from Rust thread-local storage (`PREEMPT_CTX`) inside
/// `e9_preempt_yield`, because the global struct would be overwritten
/// by other workers between yield and resume.
#[repr(C)]
pub struct E9SharedRbc {
    pub counter: i64,
    pub armed: i32,
    pub _pad: i32,
    pub yield_fn: *const std::ffi::c_void,
}

// SAFETY: E9SharedRbc is a plain-old-data struct at a fixed mmap'd address.
// Single-writer access is enforced by the PreemptRing token-passing protocol.
unsafe impl Send for E9SharedRbc {}
unsafe impl Sync for E9SharedRbc {}

/// Get a raw pointer to the shared RBC state at the fixed address.
///
/// # Safety
/// The caller must ensure `mmap_shared_rbc()` has been called first.
pub unsafe fn e9_shared_rbc() -> *mut E9SharedRbc {
    E9_SHARED_ADDR as *mut E9SharedRbc
}

/// Read the current e9patch software branch counter value.
///
/// Returns the counter value from the shared mmap'd page. The counter
/// is decremented on each Jcc in the instrumented `.so`, so
/// `snapshot - current = branches executed`.
///
/// Panics if the shared page has not been mmap'd (e9_shared_rbc is null).
pub fn e9_read_counter() -> i64 {
    // SAFETY: When e9patch mode is active, mmap_shared_rbc() has already
    // been called, making E9_SHARED_ADDR a valid pointer. The single-writer
    // access model (token ring) ensures no concurrent mutation.
    unsafe { (*e9_shared_rbc()).counter }
}

/// Map the shared RBC state page at the fixed address.
///
/// Returns the pointer on success, or panics if the mmap fails.
pub fn mmap_shared_rbc() -> *mut E9SharedRbc {
    let addr = E9_SHARED_ADDR as *mut std::ffi::c_void;
    // SAFETY: `mmap` with MAP_FIXED at our chosen address. The address
    // is in an obscure gap (0x1E9_000_000) below typical ASLR ranges.
    let ptr = unsafe {
        libc::mmap(
            addr,
            std::mem::size_of::<E9SharedRbc>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "e9patch: mmap at {E9_SHARED_ADDR:#x} failed (address in use?)"
    );
    // Initialize to disarmed state.
    let shared = ptr as *mut E9SharedRbc;
    // SAFETY: `shared` points to the freshly mmap'd page (verified non-null
    // above). Writing these fields initializes the struct to disarmed state.
    unsafe {
        (*shared).counter = i64::MAX;
        (*shared).armed = 0;
        (*shared)._pad = 0;
        (*shared).yield_fn = e9_preempt_yield as *const std::ffi::c_void;
    }
    shared
}

/// Unmap the shared RBC state page.
pub fn munmap_shared_rbc() {
    // SAFETY: E9_SHARED_ADDR was mapped by `mmap_shared_rbc()`.
    // Unmapped exactly once here during e9patch teardown.
    unsafe {
        libc::munmap(
            E9_SHARED_ADDR as *mut std::ffi::c_void,
            std::mem::size_of::<E9SharedRbc>(),
        );
    }
}

// Keep E9_SHARED_RBC as a symbol for the .so's extern declarations to resolve.
// The .so's e9_arm/e9_disarm/e9_worker_setup use this to write to the shared
// state. But the actual memory is at the mmap'd fixed address — this static is
// unused at runtime (the .so functions are updated to use the mmap'd address).
#[no_mangle]
pub static mut E9_SHARED_RBC: E9SharedRbc = E9SharedRbc {
    counter: i64::MAX,
    armed: 0,
    _pad: 0,
    yield_fn: std::ptr::null(),
};

// Atomic counter of e9_preempt_yield calls (for debugging).
static E9_YIELD_CALL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read the e9_preempt_yield call count (for diagnostics).
pub fn e9_yield_call_count() -> u64 {
    E9_YIELD_CALL_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Called from the C trampoline (`rbc_trampoline()`) when the software RBC
/// counter expires at an instrumented Jcc instruction.
///
/// Replicates `preempt_handler` logic but callable from regular C code
/// (not a signal handler — can use full Rust, including `tracing`).
///
/// Returns a new timeslice for the C trampoline to load into `rbc_counter`.
///
/// # Safety
///
/// Must be called from a thread with `PREEMPT_CTX` installed (i.e., a
/// worker thread inside a preemptive dispatch).
#[no_mangle]
pub unsafe extern "C" fn e9_preempt_yield() -> u64 {
    E9_YIELD_CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Get ring and worker_id from Rust TLS — NOT from the shared struct,
    // because other workers may have overwritten the shared struct between
    // our yield and our resume.
    let pctx = match PREEMPT_CTX.with(|c| c.get()) {
        Some(ctx) => ctx,
        None => return u64::MAX,
    };
    // SAFETY: `pctx.ring` is a valid pointer set during `install()`.
    let ring = unsafe { &*pctx.ring };
    let wid = pctx.worker_id;

    // 1. Read per-callback context from CALLBACK_CTX.
    let saved = match crate::kfuncs::get_callback_ctx() {
        None => {
            // Not inside a simulator context — return a large timeslice to
            // avoid spinning. This shouldn't happen in practice.
            return u64::MAX;
        }
        Some(c) => c,
    };

    // 2. Track structop and record preemption point.
    let saved_ops = current_ops_context();
    set_current_ops_context(saved_ops);
    // The timeslice that just expired is not directly available here,
    // so we record 0 for rbc_count (no hardware RBC measurement).
    record_rbc_preemption(0);
    let sinfo = structop_info();
    ring.record_preemption(0, 0, saved.current_cpu, wid, sinfo);

    // 3. Trace log (safe — not in a signal handler).
    tracing::debug!(
        "preempt:e9patch ops={} kfunc={} structop#{}:{}",
        sinfo.ops_context.short_name(),
        if sinfo.kfunc_name.is_empty() {
            "none"
        } else {
            sinfo.kfunc_name
        },
        sinfo.cpu_count,
        sinfo.global_count,
    );

    // 4. Yield token (futex-based).
    ring.inc_signal_preempt();
    if ring.yield_token(wid) {
        inc_interleave();
    }

    // 5. Restore context.
    crate::kfuncs::install_callback_ctx(saved);

    // 6. Roll new timeslice from PRNG and return it.
    ring.roll_timeslice(pctx.timeslice_min, pctx.timeslice_max)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_worker_completes() {
        let ring = PreemptRing::new(1, 42);
        ring.start();
        ring.wait_for_token(WorkerId(0));
        ring.finish(WorkerId(0));
        ring.wait_all_done();
    }

    #[test]
    fn test_two_workers_interleave() {
        let ring = PreemptRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.finish(WorkerId(0));
            });

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.yield_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();
        });
    }

    #[test]
    fn test_prng_determinism() {
        let order1 = run_and_record_order(3, 12345);
        let order2 = run_and_record_order(3, 12345);
        assert_eq!(order1, order2, "same seed must give same order");
    }

    #[test]
    fn test_different_seeds_may_differ() {
        let order1 = run_and_record_order(4, 100);
        let order2 = run_and_record_order(4, 999);
        let _ = (order1, order2); // Just verify no panics.
    }

    #[test]
    fn test_finish_without_yield() {
        let ring = PreemptRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                ring_ref.finish(WorkerId(0));
            });

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();
        });
    }

    #[test]
    fn test_multiple_yields() {
        let ring = PreemptRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.finish(WorkerId(0));
            });

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.yield_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();
        });
    }

    #[test]
    fn test_many_workers_stress() {
        // Stress test with many workers and many yields.
        for seed in [1, 42, 12345, 999999] {
            let n = 8;
            let ring = PreemptRing::new(n, seed);

            std::thread::scope(|s| {
                let ring_ref = &ring;
                for i in 0..n {
                    s.spawn(move || {
                        ring_ref.wait_for_token(WorkerId(i));
                        for _ in 0..10 {
                            ring_ref.yield_token(WorkerId(i));
                        }
                        ring_ref.finish(WorkerId(i));
                    });
                }
                ring.start();
                ring.wait_all_done();
            });
        }
    }

    #[test]
    fn test_roll_timeslice() {
        let ring = PreemptRing::new(1, 42);
        // Must be in range.
        for _ in 0..100 {
            let ts = ring.roll_timeslice(50, 500);
            assert!((50..=500).contains(&ts), "timeslice {ts} out of range");
        }
        // Degenerate range.
        assert_eq!(ring.roll_timeslice(100, 100), 100);
    }

    fn run_and_record_order(n: usize, seed: u32) -> Vec<WorkerId> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ring = PreemptRing::new(n, seed);
        let order: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(usize::MAX)).collect();
        let counter = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for i in 0..n {
                let ring_ref = &ring;
                let order_ref = &order;
                let counter_ref = &counter;
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = counter_ref.fetch_add(1, Ordering::SeqCst);
                    order_ref[i].store(seq, Ordering::SeqCst);
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.finish(WorkerId(i));
                });
            }
            ring.start();
            ring.wait_all_done();
        });

        let mut pairs: Vec<(usize, WorkerId)> = order
            .iter()
            .enumerate()
            .map(|(i, a)| (a.load(Ordering::SeqCst), WorkerId(i)))
            .collect();
        pairs.sort_by_key(|&(seq, _)| seq);
        pairs.into_iter().map(|(_, id)| id).collect()
    }
}
