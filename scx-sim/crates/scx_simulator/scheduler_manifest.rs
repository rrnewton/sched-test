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

// build.rs and the runtime lib each include! this file and read a DIFFERENT
// subset of the fields/types, so each compilation context sees the other's as
// unused -- #[allow(dead_code)] on the shared types silences that.

/// Source-text transform applied to a scheduler's upstream BPF source before
/// compilation (a pre-existing build step, not manifest-generated code).
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codegen {
    /// cosmos: guard the one division in update_freq() against a zero divisor.
    /// BPF integer divide-by-zero yields 0; native C raises SIGFPE.
    CosmosDivZeroGuard,
}

/// A scheduler config global's value, written before run via write_*_global.
/// `NumCpus` resolves to the simulator's CPU count at apply time (e.g. the
/// nr_cpu_ids / nr_possible_cpus / nr_cpus_onln globals).
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfigValue {
    Bool(bool),
    U32(u32),
    U64(u64),
    NumCpus,
}

/// The runtime (register/setup) half of a scheduler's manifest. Grows
/// incrementally -- rodata now; maps and init actions in later increments.
#[allow(dead_code)]
pub struct SchedulerRuntime {
    /// const-volatile config globals the generic setup writes before run, as
    /// (symbol, value). Replaces the per-scheduler C setup's rodata writes.
    pub rodata: &'static [(&'static str, ConfigValue)],
}

impl SchedulerRuntime {
    /// A scheduler with no generic runtime data (e.g. simple).
    pub const EMPTY: Self = Self { rodata: &[] };
}

/// Declarative per-scheduler descriptor: build-side fields (read by build.rs)
/// plus the runtime register/setup half (read by the lib).
#[allow(dead_code)]
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
    /// Runtime register/setup data consumed by the generic setup path.
    pub runtime: SchedulerRuntime,
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
        runtime: SchedulerRuntime::EMPTY,
    },
    SchedulerManifest {
        name: "lavd",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: true,
        codegen: None,
        runtime: SchedulerRuntime::EMPTY,
    },
    SchedulerManifest {
        name: "mitosis",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        codegen: None,
        // Migrated from mitosis_setup's config-global writes. nr_possible_cpus
        // resolves to num_cpus at apply time; root_cgid is a u64 (cgid width). The 3
        // flags whose C rodata default differs from the value here (smt_enabled,
        // exiting_task_workaround_enabled, cpu_controller_disabled) make these writes
        // load-bearing, not redundant. The all_cpus bitmask (computed) and the
        // timer-state clears stay in mitosis_setup -- not const-volatile scalar rodata.
        runtime: SchedulerRuntime {
            rodata: &[
                ("nr_possible_cpus", ConfigValue::NumCpus),
                ("smt_enabled", ConfigValue::Bool(false)),
                ("slice_ns", ConfigValue::U64(20_000_000)),
                ("root_cgid", ConfigValue::U64(1)),
                ("debug_events_enabled", ConfigValue::Bool(false)),
                ("exiting_task_workaround_enabled", ConfigValue::Bool(false)),
                ("cpu_controller_disabled", ConfigValue::Bool(true)),
                ("reject_multicpu_pinning", ConfigValue::Bool(false)),
            ],
        },
    },
    SchedulerManifest {
        name: "simple",
        strip_const: false,
        scx_bpf_dir: false,
        extra_local_include: false,
        codegen: None,
        runtime: SchedulerRuntime::EMPTY,
    },
    SchedulerManifest {
        name: "tickless",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        codegen: None,
        // Migrated from tickless_setup's rodata writes. nr_cpu_ids
        // resolves to num_cpus at apply time; the rest are fixed config.
        runtime: SchedulerRuntime {
            rodata: &[
                ("nr_cpu_ids", ConfigValue::NumCpus),
                ("smt_enabled", ConfigValue::Bool(false)),
                ("slice_ns", ConfigValue::U64(20_000_000)),
                ("tick_freq", ConfigValue::U64(250)),
            ],
        },
    },
];
