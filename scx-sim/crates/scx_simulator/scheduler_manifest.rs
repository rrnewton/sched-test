// Declarative per-scheduler descriptors.
//
// This is the single source of truth for the per-scheduler variation that the
// build (and, in later increments, the generic register/setup path) needs. It
// follows the declarative model of scx's own scx_cargo::BpfBuilder, where each
// scx scheduler's build.rs declares its sources/intf/skel as data passed to a
// shared builder crate (no codegen). Here each scheduler likewise declares WHAT
// it needs -- build flags now; maps and config in later increments -- as data
// the generic build/register/setup code reads. It is NOT codegen.
//
// Included via `include!` from build.rs (build-time fields today). Runtime
// fields (maps, rodata, init actions) are added in later increments and the
// runtime will include! the same file so there is one source of truth.

/// Source-text transform applied to a scheduler's upstream BPF source before
/// compilation (a pre-existing build step, not manifest-generated code).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codegen {
    /// cosmos: guard the one division in update_freq() against a zero divisor.
    /// BPF integer divide-by-zero yields 0; native C raises SIGFPE.
    CosmosDivZeroGuard,
}

/// Declarative per-scheduler descriptor. Build-side fields only for now;
/// register/setup fields (maps, rodata, init actions) land in later increments.
pub struct SchedulerManifest {
    /// Scheduler name; matches the schedulers/<name>/ directory.
    pub name: &'static str,
    /// Strip `const` so the BPF const-volatile globals are writable. True for
    /// every scheduler except `simple`, whose source is local and needs no
    /// rodata writes.
    pub strip_const: bool,
    /// Add `-I <scx_root>/scheds/rust/scx_<name>/src/bpf` (where the scheduler's
    /// BPF source and headers live). True for every scheduler except `simple`.
    pub scx_bpf_dir: bool,
    /// Add `-I <schedulers>/<name>` -- lavd and cosmos keep a generated/patched
    /// source (e.g. cosmos_main_patched.c) in their own directory.
    pub extra_local_include: bool,
    /// Source codegen transform applied before compilation, if any.
    pub codegen: Option<Codegen>,
}

/// The declared scheduler set. The build cross-checks this against the
/// discovered schedulers/<name>/ directories so neither can drift silently.
pub const SCHEDULERS: &[SchedulerManifest] = &[
    SchedulerManifest {
        name: "cosmos",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: true,
        codegen: Some(Codegen::CosmosDivZeroGuard),
    },
    SchedulerManifest {
        name: "lavd",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: true,
        codegen: None,
    },
    SchedulerManifest {
        name: "mitosis",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        codegen: None,
    },
    SchedulerManifest {
        name: "simple",
        strip_const: false,
        scx_bpf_dir: false,
        extra_local_include: false,
        codegen: None,
    },
    SchedulerManifest {
        name: "tickless",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        codegen: None,
    },
];
