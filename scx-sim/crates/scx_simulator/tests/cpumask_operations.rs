//! BPF cpumask helper operations + task-to-CPU allowed-mask observability.
//!
//! Two layers of "cpumask" exist in scxsim and this file covers the one that
//! nothing else exercises directly:
//!
//! * **Part A — raw BPF cpumask helpers** (`bpf_cpumask_create`,
//!   `bpf_cpumask_and/or/xor`, `bpf_cpumask_set/clear/test_cpu`,
//!   `bpf_cpumask_first/weight/empty`, `bpf_cpumask_subset/intersects/copy`,
//!   `bpf_cpumask_test_and_set_cpu`). These are the **real** kernel-substrate
//!   bit operations in `csrc/sim_bpf_stubs.c` that every SCX scheduler calls
//!   into — they are the kfunc surface the BPF programs see, so testing them
//!   here exercises the actual C code (No-Stub Rule: this is the kernel's own
//!   cpumask implementation, not a Rust re-approximation). They are invoked
//!   directly over FFI. The mask is `struct { unsigned long bits[128] }`
//!   (`NR_CPUS = 128`), allocated from the deterministic 32 MiB bump arena.
//!
//! * **Part B — task allowed-mask → `SetCpumask` trace**. The engine builds
//!   each task's `cpus_ptr` from `TaskDef.allowed_cpus`, notifies the scheduler
//!   via `ops.set_cpumask`, and records a `SetCpumask{pid, cpumask_hex}` trace
//!   event (hex is LSB = cpu 0). This asserts that the rendered mask reflects
//!   the requested affinity — the real-cpumask half of `cpumask_to_hex` that the
//!   existing `cpumask_to_hex_renders_lsb_cpu_zero` unit test explicitly defers
//!   to "an integration test".
//!
//! Task-affinity *enforcement* (a pinned task is never scheduled on a disallowed
//! CPU) and *runtime* cpuset changes are covered separately in `cpu_affinity.rs`;
//! this file deliberately does not duplicate them.

use std::ffi::c_void;

use scx_simulator::*;

mod common;

// ===========================================================================
// Part A — raw BPF cpumask helper operations (direct FFI).
// ===========================================================================

// The real kernel-substrate cpumask kfuncs live in `csrc/sim_bpf_stubs.c`, which
// is compiled into each scheduler `.so` (not statically into the test binary).
// So we resolve them from a loaded scheduler `.so` via `DynamicScheduler::
// get_symbol` — exercising the exact code every SCX scheduler calls into
// (No-Stub Rule: this is the kernel's own cpumask implementation, not a Rust
// re-approximation). `struct bpf_cpumask` and `struct cpumask` share the same
// `unsigned long bits[128]` layout (`NR_CPUS = 128`), so a `*bpf_cpumask` is
// passed wherever a `*const cpumask` is expected; masks are allocated from the
// `.so`'s deterministic bump arena.

/// `bpf_cpumask_first` returns this sentinel (>= nr_cpu_ids) when no bit is set:
/// `128 words * 64 bits/word`.
const FIRST_NONE: u32 = 128 * 64;

// Function-pointer types for the cpumask kfuncs, matching the C signatures.
type FnCreate = unsafe extern "C" fn() -> *mut c_void;
type FnRelease = unsafe extern "C" fn(*mut c_void);
type FnUnary = unsafe extern "C" fn(*mut c_void);
type FnBitCpu = unsafe extern "C" fn(u32, *mut c_void);
type FnTestCpu = unsafe extern "C" fn(u32, *const c_void) -> bool;
type FnTestAndSet = unsafe extern "C" fn(u32, *mut c_void) -> bool;
type FnPred = unsafe extern "C" fn(*const c_void) -> bool;
type FnScan = unsafe extern "C" fn(*const c_void) -> u32;
type FnAnd = unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void) -> bool;
type FnBinVoid = unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void);
type FnCopy = unsafe extern "C" fn(*mut c_void, *const c_void);
type FnRelOp = unsafe extern "C" fn(*const c_void, *const c_void) -> bool;

/// The cpumask kfunc surface, resolved out of a loaded scheduler `.so`.
///
/// Holds the function pointers copied from the `.so`'s symbol table. They point
/// into the `.so`'s mapped code and stay valid as long as the owning
/// `DynamicScheduler` (and thus its `libloading::Library`) is alive — callers
/// must keep that scheduler in scope for the lifetime of any [`Mask`].
struct CpumaskApi {
    create: FnCreate,
    release: FnRelease,
    set_cpu: FnBitCpu,
    clear_cpu: FnBitCpu,
    clear: FnUnary,
    setall: FnUnary,
    test_cpu: FnTestCpu,
    test_and_set_cpu: FnTestAndSet,
    empty: FnPred,
    weight: FnScan,
    first: FnScan,
    and: FnAnd,
    or: FnBinVoid,
    xor: FnBinVoid,
    copy: FnCopy,
    subset: FnRelOp,
    intersects: FnRelOp,
}

/// Resolve one symbol from the scheduler `.so`, panicking with a clear message
/// if it is missing, and copy the function pointer out of the borrowed `Symbol`.
///
/// # Safety
/// `T` must match the C symbol's actual type.
unsafe fn sym<T: Copy>(sched: &DynamicScheduler, name: &[u8]) -> T {
    let s = sched.get_symbol::<T>(name).unwrap_or_else(|| {
        panic!(
            "cpumask symbol {:?} not found in .so",
            String::from_utf8_lossy(name)
        )
    });
    *s
}

impl CpumaskApi {
    /// Load the cpumask kfuncs from `sched`'s `.so`.
    fn load(sched: &DynamicScheduler) -> Self {
        // SAFETY: each type parameter matches the corresponding C signature
        // declared in `csrc/sim_bpf_stubs.c`.
        unsafe {
            CpumaskApi {
                create: sym(sched, b"bpf_cpumask_create\0"),
                release: sym(sched, b"bpf_cpumask_release\0"),
                set_cpu: sym(sched, b"bpf_cpumask_set_cpu\0"),
                clear_cpu: sym(sched, b"bpf_cpumask_clear_cpu\0"),
                clear: sym(sched, b"bpf_cpumask_clear\0"),
                setall: sym(sched, b"bpf_cpumask_setall\0"),
                test_cpu: sym(sched, b"bpf_cpumask_test_cpu\0"),
                test_and_set_cpu: sym(sched, b"bpf_cpumask_test_and_set_cpu\0"),
                empty: sym(sched, b"bpf_cpumask_empty\0"),
                weight: sym(sched, b"bpf_cpumask_weight\0"),
                first: sym(sched, b"bpf_cpumask_first\0"),
                and: sym(sched, b"bpf_cpumask_and\0"),
                or: sym(sched, b"bpf_cpumask_or\0"),
                xor: sym(sched, b"bpf_cpumask_xor\0"),
                copy: sym(sched, b"bpf_cpumask_copy\0"),
                subset: sym(sched, b"bpf_cpumask_subset\0"),
                intersects: sym(sched, b"bpf_cpumask_intersects\0"),
            }
        }
    }

    /// Allocate a fresh, all-zero cpumask from the `.so`'s arena.
    fn new_mask(&self) -> Mask<'_> {
        let p = unsafe { (self.create)() };
        assert!(!p.is_null(), "bpf_cpumask_create returned NULL (arena?)");
        Mask { api: self, ptr: p }
    }

    /// Allocate a mask with exactly `cpus` set.
    fn mask_from(&self, cpus: &[u32]) -> Mask<'_> {
        let m = self.new_mask();
        for &c in cpus {
            m.set(c);
        }
        m
    }
}

/// A cpumask handle bound to the [`CpumaskApi`] that created it. Releases its
/// arena allocation on drop (arena free is a no-op, but this keeps intent clear
/// and pointers scoped). Methods are safe wrappers over the resolved kfuncs.
struct Mask<'a> {
    api: &'a CpumaskApi,
    ptr: *mut c_void,
}

impl Mask<'_> {
    fn ptr(&self) -> *const c_void {
        self.ptr
    }
    fn ptr_mut(&self) -> *mut c_void {
        self.ptr
    }

    fn set(&self, cpu: u32) {
        unsafe { (self.api.set_cpu)(cpu, self.ptr) }
    }
    fn clear_cpu(&self, cpu: u32) {
        unsafe { (self.api.clear_cpu)(cpu, self.ptr) }
    }
    fn test(&self, cpu: u32) -> bool {
        unsafe { (self.api.test_cpu)(cpu, self.ptr) }
    }
    fn weight(&self) -> u32 {
        unsafe { (self.api.weight)(self.ptr) }
    }
    fn empty(&self) -> bool {
        unsafe { (self.api.empty)(self.ptr) }
    }
    fn first(&self) -> u32 {
        unsafe { (self.api.first)(self.ptr) }
    }

    /// Enumerate the set CPUs in ascending order by repeatedly taking `first()`
    /// and clearing it — i.e. cpumask iteration. Consumes (clears) the mask.
    fn drain_to_sorted_vec(&self) -> Vec<u32> {
        let mut out = Vec::new();
        loop {
            let f = self.first();
            if f >= FIRST_NONE {
                break;
            }
            out.push(f);
            self.clear_cpu(f);
        }
        out
    }
}

impl Drop for Mask<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.release)(self.ptr) }
    }
}

// ---------------------------------------------------------------------------
// (1) Creation & the empty mask.
// ---------------------------------------------------------------------------

/// A freshly created cpumask is empty: `empty()` true, `weight()` 0, no CPU
/// tests set, and `first()` returns the "none" sentinel.
#[test]
fn test_create_yields_empty_mask() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let m = api.new_mask();
    assert!(m.empty(), "new mask should be empty");
    assert_eq!(m.weight(), 0, "new mask weight should be 0");
    assert_eq!(m.first(), FIRST_NONE, "first() on empty should be sentinel");
    for cpu in [0u32, 1, 7, 63, 64, 127] {
        assert!(!m.test(cpu), "new mask should not have cpu {cpu} set");
    }
}

// ---------------------------------------------------------------------------
// (1b) Building a mask: set / test / clear, including across a word boundary.
// ---------------------------------------------------------------------------

/// `set_cpu` / `test_cpu` / `clear_cpu` round-trip, including CPUs on both sides
/// of the 64-bit word boundary (63/64/65) to catch word-index / shift bugs.
#[test]
fn test_set_test_clear_across_word_boundary() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let m = api.new_mask();
    let cpus = [0u32, 1, 63, 64, 65, 127];
    for &c in &cpus {
        m.set(c);
        assert!(m.test(c), "cpu {c} should read back set");
    }
    assert_eq!(m.weight(), cpus.len() as u32, "weight after sets");
    // A CPU between two set ones (e.g. 2) must remain clear.
    assert!(!m.test(2), "cpu 2 was never set");

    m.clear_cpu(64);
    assert!(!m.test(64), "cpu 64 cleared");
    assert!(
        m.test(63) && m.test(65),
        "neighbors of cleared bit unaffected"
    );
    assert_eq!(m.weight(), cpus.len() as u32 - 1, "weight after one clear");
}

/// `test_and_set_cpu` returns the *previous* state and always leaves the bit
/// set: first call on a clear bit returns false, a repeat returns true.
#[test]
fn test_test_and_set_cpu_reports_prior_state() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let m = api.new_mask();
    let was_set = unsafe { (api.test_and_set_cpu)(5, m.ptr_mut()) };
    assert!(
        !was_set,
        "first test_and_set on cpu 5 should report not-previously-set"
    );
    assert!(m.test(5), "cpu 5 should now be set");
    let was_set2 = unsafe { (api.test_and_set_cpu)(5, m.ptr_mut()) };
    assert!(
        was_set2,
        "second test_and_set on cpu 5 should report previously-set"
    );
}

/// `setall` sets every representable CPU; `clear` empties the mask again.
#[test]
fn test_setall_then_clear() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let m = api.new_mask();
    unsafe { (api.setall)(m.ptr_mut()) };
    assert!(!m.empty(), "setall mask must not be empty");
    for cpu in [0u32, 1, 63, 64, 127] {
        assert!(m.test(cpu), "setall must set cpu {cpu}");
    }
    unsafe { (api.clear)(m.ptr_mut()) };
    assert!(m.empty(), "clear must empty the mask");
    assert_eq!(m.weight(), 0);
}

// ---------------------------------------------------------------------------
// (2) AND / OR / XOR, and NOT expressed via XOR with a full domain.
// ---------------------------------------------------------------------------

/// `bpf_cpumask_and`: dst = s1 & s2, and the return value reports whether the
/// result is non-empty. Overlapping inputs intersect; disjoint inputs yield an
/// empty result and a `false` return.
#[test]
fn test_and_intersection_and_return_flag() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let a = api.mask_from(&[0, 1, 2, 3]);
    let b = api.mask_from(&[2, 3, 4, 5]);
    let dst = api.new_mask();

    let nonempty = unsafe { (api.and)(dst.ptr_mut(), a.ptr(), b.ptr()) };
    assert!(nonempty, "and of overlapping masks should report non-empty");
    assert_eq!(dst.drain_to_sorted_vec(), vec![2, 3], "a & b = {{2,3}}");

    // Disjoint → empty result, false return.
    let c = api.mask_from(&[0, 1]);
    let d = api.mask_from(&[6, 7]);
    let dst2 = api.new_mask();
    let nonempty2 = unsafe { (api.and)(dst2.ptr_mut(), c.ptr(), d.ptr()) };
    assert!(!nonempty2, "and of disjoint masks should report empty");
    assert!(dst2.empty(), "disjoint and result must be empty");
}

/// `bpf_cpumask_or`: dst = s1 | s2 (union).
#[test]
fn test_or_union() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let a = api.mask_from(&[0, 1, 2, 3]);
    let b = api.mask_from(&[2, 3, 4, 5]);
    let dst = api.new_mask();
    unsafe { (api.or)(dst.ptr_mut(), a.ptr(), b.ptr()) };
    assert_eq!(
        dst.drain_to_sorted_vec(),
        vec![0, 1, 2, 3, 4, 5],
        "a | b = {{0..5}}"
    );
}

/// `bpf_cpumask_xor`: dst = s1 ^ s2 (symmetric difference).
#[test]
fn test_xor_symmetric_difference() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let a = api.mask_from(&[0, 1, 2, 3]);
    let b = api.mask_from(&[2, 3, 4, 5]);
    let dst = api.new_mask();
    unsafe { (api.xor)(dst.ptr_mut(), a.ptr(), b.ptr()) };
    assert_eq!(
        dst.drain_to_sorted_vec(),
        vec![0, 1, 4, 5],
        "a ^ b = {{0,1,4,5}}"
    );
}

/// NOT within a domain. There is no `bpf_cpumask_not` kfunc; the kernel-idiomatic
/// complement of `a` restricted to a domain `D` is `D ^ a` (for a ⊆ D). Here
/// D = {0..7}, a = {0,2,4,6} → complement = {1,3,5,7}.
#[test]
fn test_not_via_xor_with_full_domain() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let domain = api.mask_from(&[0, 1, 2, 3, 4, 5, 6, 7]);
    let a = api.mask_from(&[0, 2, 4, 6]);
    let complement = api.new_mask();
    unsafe { (api.xor)(complement.ptr_mut(), domain.ptr(), a.ptr()) };
    assert_eq!(
        complement.drain_to_sorted_vec(),
        vec![1, 3, 5, 7],
        "~a within {{0..7}} = {{1,3,5,7}}"
    );
}

// ---------------------------------------------------------------------------
// (3) Iteration + weight/first.
// ---------------------------------------------------------------------------

/// Iteration via `first()` + `clear_cpu()` visits exactly the set CPUs in
/// ascending order, and `weight()` matches the count at every step. Uses CPUs
/// spanning multiple 64-bit words to exercise the word-scan in `first`.
#[test]
fn test_iteration_visits_set_cpus_in_order() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let cpus = [1u32, 4, 7, 63, 64, 100];
    let m = api.mask_from(&cpus);
    assert_eq!(m.weight(), cpus.len() as u32, "weight = number of set bits");
    assert_eq!(m.first(), 1, "first() = lowest set bit");

    // Destructive iteration: weight must decrease by one each step.
    let mut expected_weight = cpus.len() as u32;
    let mut seen = Vec::new();
    loop {
        assert_eq!(m.weight(), expected_weight, "weight tracks remaining bits");
        let f = m.first();
        if f >= FIRST_NONE {
            break;
        }
        seen.push(f);
        m.clear_cpu(f);
        expected_weight -= 1;
    }
    assert_eq!(
        seen,
        cpus.to_vec(),
        "iteration order is ascending set-bit order"
    );
    assert!(m.empty(), "mask emptied by iteration");
    assert_eq!(m.first(), FIRST_NONE, "first() sentinel after drain");
}

// ---------------------------------------------------------------------------
// (4) Relational ops: subset / intersects, and copy independence.
// ---------------------------------------------------------------------------

/// `bpf_cpumask_subset(a, b)` is true iff every bit of `a` is in `b`;
/// `bpf_cpumask_intersects(a, b)` is true iff they share any bit.
#[test]
fn test_subset_and_intersects() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let small = api.mask_from(&[1, 2]);
    let big = api.mask_from(&[0, 1, 2, 3]);
    let disjoint = api.mask_from(&[4, 5]);
    let overlap = api.mask_from(&[3, 4]);

    assert!(
        unsafe { (api.subset)(small.ptr(), big.ptr()) },
        "{{1,2}} ⊆ {{0,1,2,3}}"
    );
    assert!(
        !unsafe { (api.subset)(big.ptr(), small.ptr()) },
        "{{0,1,2,3}} ⊄ {{1,2}}"
    );
    // Every mask is a subset of itself; the empty mask is a subset of anything.
    let empty = api.new_mask();
    assert!(unsafe { (api.subset)(big.ptr(), big.ptr()) }, "self-subset");
    assert!(
        unsafe { (api.subset)(empty.ptr(), small.ptr()) },
        "empty ⊆ any"
    );

    assert!(
        unsafe { (api.intersects)(big.ptr(), overlap.ptr()) },
        "{{0,1,2,3}} ∩ {{3,4}} ≠ ∅"
    );
    assert!(
        !unsafe { (api.intersects)(big.ptr(), disjoint.ptr()) },
        "{{0,1,2,3}} ∩ {{4,5}} = ∅"
    );
}

/// `bpf_cpumask_copy` duplicates the source, and the copy is independent:
/// mutating the source afterwards does not change the destination.
#[test]
fn test_copy_is_independent_snapshot() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::simple();
    let api = CpumaskApi::load(&sched);
    let src = api.mask_from(&[2, 5, 9]);
    let dst = api.new_mask();
    unsafe { (api.copy)(dst.ptr_mut(), src.ptr()) };

    for cpu in [2u32, 5, 9] {
        assert!(dst.test(cpu), "copy should have cpu {cpu}");
    }
    assert_eq!(dst.weight(), 3, "copy weight matches source");

    // Mutate source; destination must be unaffected (independent storage).
    src.set(20);
    src.clear_cpu(2);
    assert!(!dst.test(20), "dst must not see post-copy source set");
    assert!(dst.test(2), "dst must not see post-copy source clear");
    assert_eq!(
        dst.weight(),
        3,
        "dst weight unchanged after source mutation"
    );
}

// ===========================================================================
// Part B — task allowed-mask → SetCpumask trace (real-cpumask hex rendering).
// ===========================================================================

use std::collections::HashSet;

/// A named scheduler factory, so one test can sweep several schedulers.
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

/// A forever-running CPU-bound task.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(50_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Build a `TaskDef` with an explicit affinity mask (or `None` for all CPUs).
fn task(name: &str, pid: i32, allowed: Option<Vec<CpuId>>) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: hog(),
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: allowed,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

/// The `cpumask_hex` from the (first) `SetCpumask` event for `pid`.
fn set_cpumask_hex(trace: &Trace, pid: Pid) -> Option<String> {
    trace.events().iter().find_map(|e| match &e.kind {
        TraceKind::SetCpumask {
            pid: p,
            cpumask_hex,
        } if *p == pid => Some(cpumask_hex.clone()),
        _ => None,
    })
}

/// The engine notifies the scheduler of each task's initial cpumask
/// (`ops.set_cpumask`) and records a `SetCpumask{pid, cpumask_hex}` trace event
/// with the mask rendered LSB = cpu 0. This verifies the rendered hex reflects
/// `allowed_cpus`: a task pinned to {0,2} on a 4-CPU box renders `0x5`
/// (bits 0 and 2), while an unpinned task renders `0xf` (all four CPUs). This
/// is the real-`bpf_cpumask` path that the `cpumask_to_hex` unit test defers to
/// an integration test. Swept across simple/lavd/cosmos.
#[test]
fn test_set_cpumask_trace_reflects_allowed_cpus() {
    let _lock = common::setup_test();
    let scheds: [NamedSched; 3] = [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ];

    for (name, make) in scheds {
        let scenario = Scenario::builder()
            .cpus(4)
            .task(task("pinned", 1, Some(vec![CpuId(0), CpuId(2)])))
            .task(task("free", 2, None))
            .duration_ms(50)
            .build();
        let t = Simulator::new(make(4)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // Pinned task {0,2}: bits 0 and 2 set → 0b0101 = 0x5.
        assert_eq!(
            set_cpumask_hex(&t, Pid(1)).as_deref(),
            Some("0x5"),
            "{name}: pinned task cpumask hex should render {{0,2}} as 0x5"
        );
        // Unpinned task: all 4 CPUs allowed → 0b1111 = 0xf.
        assert_eq!(
            set_cpumask_hex(&t, Pid(2)).as_deref(),
            Some("0xf"),
            "{name}: unpinned task cpumask hex should render all 4 CPUs as 0xf"
        );
    }
}

/// Every task that joins the simulation gets exactly one initial `SetCpumask`
/// notification (mirroring the kernel enumerating a task's `cpus_ptr` once at
/// enable time), and the recorded pids are exactly the tasks in the scenario.
#[test]
fn test_set_cpumask_fires_once_per_task() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .task(task("a", 1, Some(vec![CpuId(0)])))
        .task(task("b", 2, Some(vec![CpuId(1)])))
        .task(task("c", 3, None))
        .duration_ms(40)
        .build();
    let t = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal, "not normal exit");

    let pids: Vec<i32> = t
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::SetCpumask { pid, .. } => Some(pid.0),
            _ => None,
        })
        .collect();
    // Exactly one SetCpumask per task, one per distinct pid.
    assert_eq!(
        pids.len(),
        3,
        "expected one SetCpumask per task, got {pids:?}"
    );
    let distinct: HashSet<i32> = pids.iter().copied().collect();
    assert_eq!(
        distinct,
        HashSet::from([1, 2, 3]),
        "SetCpumask pids should be exactly the scenario tasks"
    );
}
