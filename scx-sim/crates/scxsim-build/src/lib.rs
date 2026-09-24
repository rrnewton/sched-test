//! Build-time scheduler `.so` compilation plus the declarative per-scheduler
//! manifest, factored into a standalone crate so BOTH `scx_simulator`'s build
//! script and an embedder (e.g. cargo-ktstr) can drive the same build path and
//! share the same [`SchedulerDefinition`] input type. Follows the declarative
//! model of scx's own `scx_cargo::BpfBuilder` (each scheduler declares WHAT it
//! needs as data -- build flags, rodata, and any source-text patches -- rather
//! than as per-scheduler code branches).
//!
//! `scx_simulator` depends on this crate both as a normal dependency (the
//! runtime applies a [`SchedulerDefinition`]'s rodata via
//! `DynamicScheduler::load_with_definition`) and as a build-dependency (its build
//! script calls [`build_schedulers`] and emits the link args for
//! [`HOST_EXPORTS`]).

use std::path::{Path, PathBuf};
use std::process::Command;

mod embed;
pub use embed::{
    bundled_scx_root, emit_bundled_upstream_module, emit_host_link_args, emit_upstream_module,
    host_link_args, resolve_scx_root, SimBuildInputs,
};

/// A scheduler config global's value, written before run via write_*_global.
/// `NumCpus` resolves to the simulator's CPU count at apply time (e.g. the
/// nr_cpu_ids / nr_possible_cpus / nr_cpus_onln globals).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ConfigValue {
    Bool(bool),
    U8(u8),
    U32(u32),
    U64(u64),
    NumCpus,
}

/// The runtime (register/setup) half of a scheduler's manifest. Grows
/// incrementally -- rodata now; maps and init actions in later increments.
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
    /// Source-text find/replace patches applied to the scheduler's upstream
    /// `main.bpf.c` before compilation, as `(find, replace)`. Empty for none.
    /// A pre-existing build step (not manifest-generated code): the patched copy
    /// is written next to the wrapper as `<name>_main_patched.c`, which the
    /// wrapper `#include`s (resolved via `extra_local_include`).
    pub source_patches: &'static [(&'static str, &'static str)],
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
        // Guard the one division in update_freq() against a zero divisor: BPF
        // integer divide-by-zero yields 0; native C raises SIGFPE.
        source_patches: &[(
            "new_freq = (100 * NSEC_PER_MSEC) / interval;",
            "new_freq = interval ? (100 * NSEC_PER_MSEC) / interval : 0;",
        )],
        // Migrated from cosmos_setup's config-global writes.
        // smt_enabled=true (SMT avoidance is unconditional upstream; the avoid_smt
        // toggle was deprecated, so there is no avoid_smt global to set).
        // slice_ns / slice_lag / busy_threshold are u64; nr_node_ids is u32. Four
        // globals' setup value differs from the BPF rodata default, so those
        // writes are load-bearing: nr_node_ids, mm_affinity, slice_ns,
        // busy_threshold.
        //
        // perf_config is deliberately NOT set. Upstream defaults it to 0x0 ("no
        // event", scx_cosmos/src/main.rs), and forcing it to 1 here switched on
        // cosmos's PMU path while the wrapper fed it fabricated counter values —
        // a No-Stub Rule violation that also meant cosmos's real no-PMU path
        // never ran. Nothing sets perf_threshold either, so is_event_heavy()
        // degenerated to "perf_events > 0" and cosmos treated essentially every
        // task as event-heavy, changing its migration decisions. Leaving it at
        // the rodata default keeps the simulator on the path production takes
        // without -e/--perf-config. numa_enabled and nr_node_ids are re-overwritten
        // by cosmos_configure_numa, which runs after apply_rodata (ffi.rs), so these
        // manifest values are the correct pre-NUMA defaults.
        runtime: SchedulerRuntime {
            rodata: &[
                ("smt_enabled", ConfigValue::Bool(true)),
                ("primary_all", ConfigValue::Bool(true)),
                ("flat_idle_scan", ConfigValue::Bool(false)),
                ("preferred_idle_scan", ConfigValue::Bool(false)),
                ("cpufreq_enabled", ConfigValue::Bool(true)),
                ("numa_enabled", ConfigValue::Bool(false)),
                ("nr_node_ids", ConfigValue::U32(1)),
                ("mm_affinity", ConfigValue::Bool(true)),
                ("slice_ns", ConfigValue::U64(20_000_000)),
                ("slice_lag", ConfigValue::U64(20_000_000)),
                ("busy_threshold", ConfigValue::U64(1)),
            ],
        },
    },
    SchedulerManifest {
        name: "lavd",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: true,
        source_patches: &[],
        // Migrated from lavd_setup's const-volatile config-global writes.
        // nr_cpu_ids resolves to num_cpus; nr_llcs and no_use_em are load-bearing
        // (the BPF rodata default 0 differs from the setup value). no_use_em and
        // verbose are u8 (const volatile u8 -- U32 would 3-byte-overrun adjacent
        // rodata). Only const-volatile globals are here; the plain-volatile mutable
        // globals the scheduler overwrites at runtime (nr_cpus_onln, power_mode,
        // is_powersave_mode, no_core_compaction, no_freq_scaling, no_preemption),
        // the computed per-CPU arrays, and the cpdom init stay in lavd_setup.
        //
        // bw_kick_builtin_idle belongs to lib/cgroup_bw and is load-bearing:
        // scx_lavd's main.rs sets it unconditionally, and with it left at the
        // rodata default the replenish timer calls the weak
        // scx_cgroup_bw_kick_idle_cb(), which scx_bpf_error()s. Its sibling
        // bw_set_sleepable is deliberately absent (false): scx_utils'
        // setup_cgroup_bw() leaves it off on a kernel without the
        // cgroup_set_bandwidth may-sleep marker -- the RESERVED tier, whose
        // contexts are pre-reserved in ops.cgroup_init().
        runtime: SchedulerRuntime {
            rodata: &[
                ("nr_cpu_ids", ConfigValue::NumCpus),
                ("nr_llcs", ConfigValue::U64(1)),
                ("is_smt_active", ConfigValue::Bool(false)),
                ("enable_cpu_bw", ConfigValue::Bool(false)),
                ("is_autopilot_on", ConfigValue::Bool(false)),
                ("no_wake_sync", ConfigValue::Bool(false)),
                ("no_slice_boost", ConfigValue::Bool(false)),
                ("no_use_em", ConfigValue::U8(1)),
                ("verbose", ConfigValue::U8(0)),
                ("bw_kick_builtin_idle", ConfigValue::Bool(true)),
            ],
        },
    },
    SchedulerManifest {
        name: "mitosis",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        source_patches: &[],
        // Migrated from mitosis_setup's config-global writes. nr_possible_cpus
        // resolves to num_cpus at apply time; root_cgid is a u64 (cgid width). The 2
        // flags whose C rodata default differs from the value here (smt_enabled,
        // exiting_task_workaround_enabled) make these writes load-bearing, not
        // redundant. There is no cpu_controller_disabled: upstream 929f6c370 made
        // tracepoint cgroup tracking, which the sim had selected with it, the only
        // path and deleted the flag. The all_cpus bitmask (computed) and the
        // timer-state clears stay in mitosis_setup -- not const-volatile scalar rodata.
        runtime: SchedulerRuntime {
            rodata: &[
                ("nr_possible_cpus", ConfigValue::NumCpus),
                ("smt_enabled", ConfigValue::Bool(false)),
                ("slice_ns", ConfigValue::U64(20_000_000)),
                ("root_cgid", ConfigValue::U64(1)),
                ("exiting_task_workaround_enabled", ConfigValue::Bool(false)),
                ("reject_multicpu_pinning", ConfigValue::Bool(false)),
            ],
        },
    },
    SchedulerManifest {
        name: "layered",
        strip_const: true,
        scx_bpf_dir: true,
        // No generated or patched source: layered's BPF compiles unmodified,
        // and its wrapper #includes straight out of the scx tree.
        extra_local_include: false,
        source_patches: &[],
        // Deliberately empty. layered's config is not a handful of rodata
        // scalars — it is a topology, a layer table with match rules, and a CPU
        // allocation, all published by layered_setup()/layered_set_topology()
        // /layered_add_layer() the way scx_layered's Rust userspace publishes
        // them. Listing a few scalars here would split that across two
        // mechanisms.
        runtime: SchedulerRuntime::EMPTY,
    },
    SchedulerManifest {
        name: "simple",
        strip_const: false,
        scx_bpf_dir: false,
        extra_local_include: false,
        source_patches: &[],
        runtime: SchedulerRuntime::EMPTY,
    },
    SchedulerManifest {
        name: "tickless",
        strip_const: true,
        scx_bpf_dir: true,
        extra_local_include: false,
        source_patches: &[],
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

/// Owned, serializable per-scheduler descriptor -- the public INPUT type both
/// the build (`build_schedulers`) and the runtime
/// (`DynamicScheduler::load_with_definition`) consume.
/// `standalone_definitions()` supplies these from the bundled `SCHEDULERS` const
/// for the standalone build; an embedder (cargo-ktstr) constructs them via
/// [`SchedulerDefinition::new`] from a scheduler name it already holds.
///
/// There is no source-location field: `build_schedulers` derives a scheduler's
/// BPF source dir from `scx_root` + [`name`](Self::name) and its `wrapper.c` dir
/// from `schedulers_src` + name, so the name is the only source key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchedulerDefinition {
    /// Scheduler name; matches the schedulers/<name>/ dir and the .so/ops prefix.
    pub name: String,
    /// Strip `const` so BPF const-volatile globals are writable (all but `simple`).
    pub strip_const: bool,
    /// Add -I <scx_root>/scheds/rust/scx_<name>/src/bpf (all but `simple`).
    pub scx_bpf_dir: bool,
    /// Add -I <schedulers>/<name> (a generated/patched source lives there).
    pub extra_local_include: bool,
    /// Source-text find/replace patches applied to the upstream `main.bpf.c`
    /// before compilation, as `(find, replace)`. Empty for none.
    pub source_patches: Vec<(String, String)>,
    /// const-volatile config globals written before run, as (symbol, value).
    pub rodata: Vec<(String, ConfigValue)>,
}

impl SchedulerDefinition {
    /// Embedder-facing constructor. `name` is the scheduler's `.so` / ops prefix
    /// and the key `build_schedulers` derives the BPF source dir
    /// (`scx_root`/scheds/rust/scx_<name>/src/bpf) and `wrapper.c` dir
    /// (`schedulers_src`/<name>) from. Build-side policy flags default to the
    /// common non-`simple` profile: `strip_const` and `scx_bpf_dir` true,
    /// `extra_local_include` false, no `source_patches`. (`strip_const`/`scx_bpf_dir`
    /// are false only for `simple`; lavd and cosmos override
    /// `extra_local_include`/`source_patches`.) rodata is empty until supplied via
    /// [`with_rodata`](Self::with_rodata).
    ///
    /// CANONICAL construction (the expression a code-generating embedder DSL,
    /// e.g. ktstr's scheduler-definition DSL, should emit): `new(name)` then
    /// chain the `with_*` builders for any field that differs from the common
    /// profile — [`with_strip_const`](Self::with_strip_const) /
    /// [`with_scx_bpf_dir`](Self::with_scx_bpf_dir) /
    /// [`with_extra_local_include`](Self::with_extra_local_include) for the build
    /// flags, [`with_source_patches`](Self::with_source_patches) for a patched
    /// scheduler, [`with_rodata`](Self::with_rodata) for config globals. The
    /// fields are also `pub` (for serde round-trip + a non-fluent escape hatch),
    /// but the fluent `new(..).with_*(..)` chain is the documented onboarding
    /// contract — a single expression, which is what codegen wants to emit.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            strip_const: true,
            scx_bpf_dir: true,
            extra_local_include: false,
            source_patches: Vec::new(),
            rodata: Vec::new(),
        }
    }

    /// Set the const-volatile config globals written before run, as
    /// (symbol, value). The common embedder path is
    /// `SchedulerDefinition::new(name).with_rodata(..)`.
    pub fn with_rodata(mut self, rodata: Vec<(String, ConfigValue)>) -> Self {
        self.rodata = rodata;
        self
    }

    /// Set the source-text find/replace patches (see [`source_patches`](Self::source_patches)).
    /// Fluent peer of [`with_rodata`](Self::with_rodata) for a patched scheduler
    /// (e.g. cosmos's divide-by-zero guard).
    pub fn with_source_patches(mut self, patches: Vec<(String, String)>) -> Self {
        self.source_patches = patches;
        self
    }

    /// Override `strip_const` (default `true`; set `false` for a scheduler whose
    /// source is local and needs no const-volatile rodata writes, e.g. `simple`).
    pub fn with_strip_const(mut self, strip_const: bool) -> Self {
        self.strip_const = strip_const;
        self
    }

    /// Override `scx_bpf_dir` (default `true`; set `false` for a scheduler with no
    /// `<scx_root>/scheds/rust/scx_<name>/src/bpf` include, e.g. `simple`).
    pub fn with_scx_bpf_dir(mut self, scx_bpf_dir: bool) -> Self {
        self.scx_bpf_dir = scx_bpf_dir;
        self
    }

    /// Override `extra_local_include` (default `false`; set `true` for a scheduler
    /// that keeps a generated/patched source in its own dir, e.g. lavd/cosmos).
    pub fn with_extra_local_include(mut self, extra_local_include: bool) -> Self {
        self.extra_local_include = extra_local_include;
        self
    }
}

/// The bundled schedulers as owned [`SchedulerDefinition`]s -- the standalone
/// provider, value-identical to the `SCHEDULERS` manifest const.
pub fn standalone_definitions() -> Vec<SchedulerDefinition> {
    SCHEDULERS
        .iter()
        .map(|m| SchedulerDefinition {
            name: m.name.to_string(),
            strip_const: m.strip_const,
            scx_bpf_dir: m.scx_bpf_dir,
            extra_local_include: m.extra_local_include,
            source_patches: m
                .source_patches
                .iter()
                .map(|(f, r)| (f.to_string(), r.to_string()))
                .collect(),
            rodata: m
                .runtime
                .rodata
                .iter()
                .map(|(s, v)| (s.to_string(), *v))
                .collect(),
        })
        .collect()
}

/// What a dlopen'd scheduler `.so` does when the host binary does NOT export a
/// [`HostExport`] -- which is why the symbol is on the list.
///
/// Only the first is loud. The other two load cleanly and run something other
/// than the simulator's definition, with nothing to say so: the scheduler runs,
/// and it is not the scheduler -- a No-Stub violation no later check can see,
/// which is why the load path probes for every entry before `dlopen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IfUnexported {
    /// No `.so` defines it and every reference is strong, so `dlopen`
    /// (`RTLD_NOW`) of any `.so` that references it fails with "undefined
    /// symbol". Loud.
    DlopenFails,
    /// The `.so` carries its own definition -- a weak fallback from
    /// scxtest's `overrides.c` or `csrc/sim_bpf_stubs.c`, a scheduler wrapper's
    /// own copy, or a strong duplicate of the host's. `dlopen` SUCCEEDS and the
    /// `.so` binds its own copy in place of the simulator's definition: for most
    /// of these a NULL/0-returning stub standing in for the real kfunc, SDT, ATQ
    /// or cgroup code.
    OwnDefinitionBinds,
    /// The `.so` holds only a weak undefined reference (`__ksym __weak`), so
    /// `dlopen` SUCCEEDS and the reference resolves to NULL. `bpf_ksym_exists()`
    /// and the `__COMPAT_*` helpers then report the kfunc absent, and the
    /// scheduler takes its path for a kernel without it; a call the scheduler
    /// does not guard jumps to address 0.
    ResolvesToNull,
}

/// A symbol DEFINED by the host binary (a linked C static lib, or a Rust
/// `#[no_mangle]` kfunc) that the scheduler `.so` files it dlopens resolve from
/// it, with what happens when the host does not export it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostExport {
    /// The ELF symbol name.
    pub name: &'static str,
    /// The consequence of the host not exporting it.
    pub if_unexported: IfUnexported,
}

impl HostExport {
    const fn dlopen_fails(name: &'static str) -> Self {
        Self {
            name,
            if_unexported: IfUnexported::DlopenFails,
        }
    }

    const fn own_definition(name: &'static str) -> Self {
        Self {
            name,
            if_unexported: IfUnexported::OwnDefinitionBinds,
        }
    }

    const fn resolves_to_null(name: &'static str) -> Self {
        Self {
            name,
            if_unexported: IfUnexported::ResolvesToNull,
        }
    }
}

/// The symbols a host binary must force into its dynamic symbol table for the
/// scheduler `.so` files it dlopens: every one whose absence a `.so` would NOT
/// report, plus the loud ones the link would otherwise drop. The single source
/// of truth for the link args ([`host_link_args`]) and for the load-time probe
/// that refuses to `dlopen` without them (`scx_simulator`'s
/// `LoadError::HostSymbolsNotExported`). `-rdynamic` puts what the link kept
/// into the binary's dynamic symbol table; `--undefined` makes the link keep
/// each symbol here even when nothing in the binary calls it (static-lib
/// members are otherwise dropped, and a Rust kfunc is kept only by accident of
/// what else shares its object file).
///
/// The [`IfUnexported::OwnDefinitionBinds`] and [`IfUnexported::ResolvesToNull`]
/// entries are the reason the probe exists: a host that under-exports them
/// loads a scheduler that silently runs something other than the simulator.
/// `scx_simulator`'s `tests/symbol_export.rs` reads the built `.so` files and
/// fails if a symbol the host exports binds silently without being listed
/// here, or if an entry's [`IfUnexported`] is not what the `.so` files show.
pub const HOST_EXPORTS: &[HostExport] = &[
    // --- No fallback in any `.so`: an unexported symbol fails dlopen. ---
    // bpf_map_* macros (scxtest).
    HostExport::dlopen_fails("scx_test_map_lookup_elem"),
    HostExport::dlopen_fails("scx_test_map_delete_elem"),
    HostExport::dlopen_fails("scx_test_map_clear_all"),
    // Per-task SDT storage (upstream's `lib/sdt_task.h` API; lavd calls
    // `__scx_task_data` and `scx_task_free_rcu` directly) and the arena
    // allocator.
    HostExport::dlopen_fails("scx_task_init"),
    HostExport::dlopen_fails("__scx_task_data"),
    HostExport::dlopen_fails("scx_task_free_rcu"),
    HostExport::dlopen_fails("scx_arena_subprog_init"),
    HostExport::dlopen_fails("sim_arena_buf"),
    HostExport::dlopen_fails("sim_arena_offset"),
    // e9patch-instrumented variants.
    HostExport::dlopen_fails("e9_preempt_yield"),
    HostExport::dlopen_fails("E9_SHARED_RBC"),
    // --- Own copy in every `.so`: an unexported symbol binds it silently. ---
    // Kfuncs the simulator implements in Rust (`kfuncs.rs`); `overrides.c`
    // stubs them out (-1 / NULL / nothing).
    HostExport::own_definition("scx_bpf_create_dsq"),
    HostExport::own_definition("scx_bpf_dsq_nr_queued"),
    HostExport::own_definition("scx_bpf_error_bstr"),
    HostExport::own_definition("scx_bpf_kick_cpu"),
    HostExport::own_definition("scx_bpf_put_cpumask"),
    HostExport::own_definition("scx_bpf_task_cpu"),
    HostExport::own_definition("bpf_task_from_pid"),
    HostExport::own_definition("bpf_task_release"),
    HostExport::own_definition("bpf_rcu_read_lock"),
    HostExport::own_definition("bpf_rcu_read_unlock"),
    // SDT task storage (`sim_sdt_stubs.c`); `overrides.c` returns NULL.
    HostExport::own_definition("scx_task_alloc"),
    HostExport::own_definition("scx_task_data"),
    HostExport::own_definition("scx_task_free"),
    // ATQ (`sim_atq.c`); `overrides.c` returns 0 or does nothing.
    HostExport::own_definition("scx_atq_create_internal"),
    HostExport::own_definition("scx_atq_insert"),
    HostExport::own_definition("scx_atq_insert_vtime"),
    HostExport::own_definition("scx_atq_nr_queued"),
    HostExport::own_definition("scx_atq_peek"),
    HostExport::own_definition("scx_atq_pop"),
    // Cgroup kfuncs. The host's `bpf_cgroup_from_id` / `bpf_cgroup_ancestor`
    // (`kfuncs.rs`) look the cgroup up in the simulator's registry; the
    // `csrc/sim_bpf_stubs.c` fallbacks every `.so` carries return NULL. (lavd
    // defines its own strong copies and calls them directly, so lavd is
    // unaffected; mitosis binds through a relocation.) acquire / release are
    // identity / no-op on both sides.
    HostExport::own_definition("bpf_cgroup_acquire"),
    HostExport::own_definition("bpf_cgroup_ancestor"),
    HostExport::own_definition("bpf_cgroup_from_id"),
    HostExport::own_definition("bpf_cgroup_release"),
    // A strong duplicate of the host's (`scxtest/scx_test_cpumask.c`) in
    // `csrc/sim_bpf_stubs.c`; the two compute the same bounded bit test today.
    HostExport::own_definition("bpf_cpumask_test_cpu"),
    // --- Weak undefined in the `.so`: an unexported symbol resolves to NULL. ---
    // Kfuncs the simulator implements in Rust (`kfuncs.rs`), most reached
    // through `__COMPAT_*` / `bpf_ksym_exists()` probes.
    HostExport::resolves_to_null("scx_bpf_cpu_curr"),
    HostExport::resolves_to_null("scx_bpf_cpuperf_cap"),
    HostExport::resolves_to_null("scx_bpf_cpuperf_cur"),
    HostExport::resolves_to_null("scx_bpf_cpuperf_set"),
    HostExport::resolves_to_null("scx_bpf_dispatch___compat"),
    HostExport::resolves_to_null("scx_bpf_dispatch_vtime___compat"),
    HostExport::resolves_to_null("scx_bpf_dsq_insert___v1"),
    HostExport::resolves_to_null("scx_bpf_dsq_insert___v2___compat"),
    HostExport::resolves_to_null("scx_bpf_dsq_insert_vtime___compat"),
    HostExport::resolves_to_null("scx_bpf_dsq_peek"),
    HostExport::resolves_to_null("scx_bpf_dump_bstr"),
    HostExport::resolves_to_null("scx_bpf_now"),
    HostExport::resolves_to_null("scx_bpf_nr_cpu_ids"),
    HostExport::resolves_to_null("scx_bpf_reenqueue_local___v1"),
    HostExport::resolves_to_null("scx_bpf_reenqueue_local___v2___compat"),
    HostExport::resolves_to_null("scx_bpf_select_cpu_and___compat"),
    // Cpumask kfuncs (`scxtest/scx_test_cpumask.c`).
    HostExport::resolves_to_null("scx_bpf_get_idle_cpumask_node"),
    HostExport::resolves_to_null("scx_bpf_get_idle_smtmask_node"),
    HostExport::resolves_to_null("scx_bpf_get_online_cpumask"),
    HostExport::resolves_to_null("scx_bpf_pick_idle_cpu_node"),
];

/// The scx-derived `-I` directories for the scheduler `.so` build, computed from
/// an explicit `scx_root` (no submodule assumption, no cargo-metadata
/// derivation). `bpf_include` is the libbpf-sys header dir (the standalone build
/// passes its `DEP_BPF_INCLUDE`; an embedder passes its own). The returned
/// sequence reproduces the standalone build's historical `-I` order exactly:
/// `-I` resolution is first-match, so the order is part of the build contract
/// (changing it can change which header wins, and the resulting `.so` bytes).
/// Does NOT include the caller's crate-local csrc/scxtest dirs (caller-private),
/// nor `<scx_root>/lib` ([`build_schedulers`] appends that itself), so neither is
/// double-added.
///
/// `vmlinux_override`: when `Some(dir)`, `dir` (which must contain a `vmlinux.h`)
/// REPLACES the two vendored `scheds/vmlinux` entries -- `scheds/vmlinux`
/// symlinks `vmlinux.h` into `scheds/vmlinux/arch/x86` (the symlink target dir,
/// where the version-pinned header lives), so both `-I` entries serve only to
/// resolve `#include "vmlinux.h"`. The vendored vmlinux is pinned to one scx
/// version, so an embedder (e.g. ktstr) passes the vmlinux it derived from the
/// kernel under test, compiling the `.so` against the matching kernel ABI.
/// `None` keeps the vendored, scx-versioned vmlinux (the standalone default).
pub fn scx_include_paths(
    scx_root: &Path,
    bpf_include: &Path,
    vmlinux_override: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = vec![
        scx_root.join("scheds/include"),
        scx_root.join("scheds/include/lib"),
    ];
    match vmlinux_override {
        Some(dir) => paths.push(dir.to_path_buf()),
        None => {
            paths.push(scx_root.join("scheds/vmlinux"));
            paths.push(scx_root.join("scheds/vmlinux/arch/x86"));
        }
    }
    paths.push(scx_root.join("scheds/include/bpf-compat"));
    paths.push(bpf_include.to_path_buf());
    paths
}

/// Whether the scx tree at `scx_root` uses the NEW cgroup_bw function signatures,
/// gated on `struct scx_task_cgroup_bw` in scheds/include/lib/cgroup.h. The
/// result is passed to [`build_schedulers`] as its `cgroup_bw_new_api` flag; a
/// missing/unreadable header reads as the old API (`false`). Mirrors
/// schedulers/Makefile's `grep '^struct scx_task_cgroup_bw'` (line-anchored, no
/// leading-whitespace tolerance) so this probe and the still-live Makefile grep
/// used by `make e9` agree.
pub fn cgroup_bw_new_api(scx_root: &Path) -> bool {
    std::fs::read_to_string(scx_root.join("scheds/include/lib/cgroup.h"))
        .map(|s| header_has_new_cgroup_bw_api(&s))
        .unwrap_or(false)
}

/// The line-anchored match used by [`cgroup_bw_new_api`], split out so it can be
/// unit-tested without an scx tree.
fn header_has_new_cgroup_bw_api(header: &str) -> bool {
    header
        .lines()
        .any(|l| l.starts_with("struct scx_task_cgroup_bw"))
}

/// Kernel-config / version scalars an embedder can override so a scheduler `.so`
/// compiles against the kernel under test instead of the standalone defaults in
/// `csrc/sim_kconfig_defaults.h`. Each `None` field keeps the C default
/// (byte-identical build); each `Some` emits `-DSIM_<NAME>=<value>` via
/// [`cflag_defines`](Self::cflag_defines). The values live in the `.so` alone:
/// the host static libs are built once, with no `KernelConfig`, and define none
/// of these symbols.
///
/// IN-SIM REALITY (today): only [`no_hz_idle`](Self::no_hz_idle) changes
/// scheduling behavior -- it gates lavd's sys_stat idle-drift branch.
/// [`kernel_version`](Self::kernel_version) + [`preempt_rcu`](Self::preempt_rcu)
/// are a COUPLED PAIR feeding scx's `is_migration_disabled` (preempt_rcu
/// short-circuits the version check), but the sim overrides that callback with
/// ground-truth `migration_disabled`, so the pair affects only the cosmetic dump
/// banner until that override is unwound -- set them TOGETHER for a coherent
/// migrate-disable model. [`hz`](Self::hz) is the tickless fallback, reachable
/// only when `tick_freq` is 0. The values are still compiled into the `.so`
/// (fidelity-forward), so an embedder supplying the kernel-under-test's values is
/// correct even where the consumer is presently shadowed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KernelConfig {
    /// LINUX_KERNEL_VERSION, encoded major<<16 | minor<<8 | patch (default 0x061200).
    pub kernel_version: Option<u32>,
    /// CONFIG_PREEMPT_RCU (default false).
    pub preempt_rcu: Option<bool>,
    /// CONFIG_HZ (default 250).
    pub hz: Option<u32>,
    /// CONFIG_NO_HZ_IDLE (default false).
    pub no_hz_idle: Option<bool>,
}

impl KernelConfig {
    /// The `-DSIM_<NAME>=<value>` flags for the `Some` fields, integer-encoded
    /// (bools as 1/0) to match the header defaults' integer encoding -- and, for
    /// `no_hz_idle` (which has no header default), the `bool` assignment site in
    /// lavd/wrapper.c. Empty for the all-`None` (standalone) config, so the build
    /// is byte-identical.
    pub fn cflag_defines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(v) = self.kernel_version {
            out.push(format!("-DSIM_LINUX_KERNEL_VERSION={v}"));
        }
        if let Some(v) = self.preempt_rcu {
            out.push(format!("-DSIM_CONFIG_PREEMPT_RCU={}", v as u8));
        }
        if let Some(v) = self.hz {
            out.push(format!("-DSIM_CONFIG_HZ={v}"));
        }
        if let Some(v) = self.no_hz_idle {
            out.push(format!("-DSIM_CONFIG_NO_HZ_IDLE={}", v as u8));
        }
        out
    }
}

/// Compile every scheduler `.so` from its `wrapper.c` plus the shared sim C
/// translation units, replicating `schedulers/Makefile` exactly. Each subdir
/// of `schedulers_src` that contains a `wrapper.c` is discovered and built into
/// `libscx_<name>.so` under `out`.
///
/// This crate compiles only the scheduler `.so` TUs; the static-lib cc::Build
/// (scxtest, sim_task, sim_sdt, sim_cgroup, sim_atq) lives in `scx_simulator`'s
/// build script.
///
/// TU split (must match the Makefile and that static-lib cc::Build):
/// - FULL-CFLAGS TUs (`wrapper.c`, `sim_bpf_stubs.c`, `overrides.c`): base
///   flags + coverage + cgroup_bw API + the cgroup_bw compile-in + every include + per-scheduler
///   extras (`-Dconst=` and the scx BPF include for all but `simple`).
/// - SPECIAL TUs (`sim_sigfpe.c`, `sim_rbc_trampoline.c`,
///   `sim_deterministic_mem.c`): CFLAGS_BASE ONLY — no includes, no coverage,
///   no defines. Coverage instrumentation or vmlinux.h here corrupts the x86
///   SIGFPE decoder, the RBC trampoline layout, and the branchless mem ops.
/// - `sim_sdt_stubs.c` is deliberately NOT linked into the `.so` (the single
///   SDT table lives in the main binary).
///
/// `scx_root` is the scx source tree (default = the bundled submodule, or an
/// SCX_ROOT override). All scheduler scx sources — including lavd's compiled-in
/// scx/lib bodies (ravg.bpf.c, cgroup_bw.bpf.c), resolved via -I<scx_root>/lib —
/// derive from scx_root, so every scheduler follows the override.
///
/// `kernel_config` overrides the kernel-config / version scalars (see
/// [`KernelConfig`]); `KernelConfig::default()` keeps the standalone defaults
/// (byte-identical).
#[allow(clippy::too_many_arguments)]
pub fn build_schedulers(
    schedulers_src: &Path,
    defs: &[SchedulerDefinition],
    out: &Path,
    csrc_dir: &Path,
    scxtest_dir: &Path,
    include_paths: &[PathBuf],
    scx_root: &Path,
    compiler: &str,
    coverage: bool,
    cgroup_bw_new_api: bool,
    kernel_config: &KernelConfig,
) {
    // -DSIM_<NAME> overrides for the kernel-config scalars (empty for standalone
    // => header defaults => byte-identical). Applied to the full TUs only; the
    // special TUs carry no kconfig symbols. Inert -D on a TU that doesn't expand
    // the macro has no effect, so all provided overrides go to every full TU.
    let kconfig_defines = kernel_config.cflag_defines();

    // CFLAGS_BASE — applied to every scheduler TU (mirrors Makefile CFLAGS_BASE).
    //
    // An implicitly declared function is an ERROR, not the warning the build
    // used to silence. Upstream scx declares much of its lib API only under
    // `#ifdef __BPF__`, which this userspace build never defines, so a newly
    // called lib function is undeclared here and C would type it as returning
    // `int`: a pointer or u64 result is silently truncated, and a missing
    // symbol only surfaces when the .so is dlopen'd, one symbol per run. The
    // 81738161 scx bump hit both (`__scx_task_data`'s task-ctx pointer,
    // `ravg_sat_add`'s u64). Declare such functions where the sim provides
    // them (scxtest/overrides.h, or the scheduler's wrapper).
    let cflags_base: &[&str] = &[
        "-fPIC",
        "-DSCX_BPF_UNITTEST",
        "-g",
        "-O2",
        "-Wno-unused-parameter",
        "-Wno-unknown-attributes",
        "-Werror=implicit-function-declaration",
    ];

    // Discover schedulers: subdirs of schedulers_src that contain wrapper.c.
    let mut names: Vec<String> = std::fs::read_dir(schedulers_src)
        .expect("read schedulers dir")
        .flatten()
        .filter(|e| e.path().join("wrapper.c").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "no schedulers (subdirs with wrapper.c) under {}",
        schedulers_src.display()
    );
    // The declared manifest and the discovered directories must agree so neither
    // drifts silently (a stale manifest entry without a dir; the reverse -- a dir
    // without a manifest entry -- is caught by the per-name lookup below).
    for m in defs {
        assert!(
            names.iter().any(|n| n.as_str() == m.name.as_str()),
            "manifest lists scheduler {} but schedulers/{}/wrapper.c does not exist",
            m.name,
            m.name
        );
    }

    // -I list shared by the full-CFLAGS TUs: the crate include set + <scx_root>/lib,
    // where lavd's compiled-in scx library bodies (ravg.bpf.c, cgroup_bw.bpf.c)
    // resolve so they follow SCX_ROOT. (The former -I schedulers anchor existed
    // only to resolve the wrappers' "../../scx/lib/*.bpf.c" relative includes,
    // which are now rehomed to plain names found via <scx_root>/lib.)
    let scx_lib = scx_root.join("lib");
    let base_includes: Vec<&Path> = include_paths
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(scx_lib.as_path()))
        .collect();

    // A scheduler that fails to compile or link must not hide the ones after
    // it: record the failure, move on to the next scheduler, and fail the
    // build once at the end naming every broken scheduler. The nightly scx
    // pin bump reported only cosmos (first in sort order) for nine nights
    // while lavd and cgroup_bw were broken behind it.
    let mut failed: Vec<String> = Vec::new();

    'sched: for name in &names {
        let sched_dir = schedulers_src.join(name);

        // Per-scheduler build variation is declared in the manifest (strip-const,
        // scx-bpf-dir, local-include, source-patches), not hardcoded name branches.
        // `simple` strips no `const` and pulls no scx BPF include (local source);
        // every other scheduler strips `const` (BPF const-volatile globals must be
        // writable) and adds scheds/rust/scx_<name>/src/bpf; lavd/cosmos also
        // include their own dir (a generated/patched source lives there).
        let m = defs
            .iter()
            .find(|m| m.name.as_str() == name.as_str())
            .unwrap_or_else(|| panic!("no manifest entry for scheduler {name}"));

        let strip_const = m.strip_const;
        let mut extra_includes: Vec<PathBuf> = Vec::new();
        if m.scx_bpf_dir {
            extra_includes.push(scx_root.join(format!("scheds/rust/scx_{name}/src/bpf")));
        }
        if m.extra_local_include {
            extra_includes.push(sched_dir.clone());
        }
        // A patched scheduler's generated source is written into OUT_DIR (NOT the
        // source tree -- see below), so resolve it via -I<out>. Prepend so it is
        // searched before sched_dir: an embedder building from a read-only sim
        // copy has no source-tree copy, and a dev's stale gitignored one must
        // never shadow the freshly generated OUT_DIR copy.
        if !m.source_patches.is_empty() {
            extra_includes.insert(0, out.to_path_buf());
        }

        // Source-text patches: regenerate a find/replace-patched copy of the
        // scheduler's upstream main.bpf.c into OUT_DIR as <name>_main_patched.c,
        // which the wrapper #includes via the -I<out> above (angle include, so
        // the includer-dir search never picks up a stale source-tree copy).
        // Writing to OUT_DIR (not the source tree) is what lets an embedder build
        // a patched scheduler from a READ-ONLY copy of sim (cargo registry cache
        // / vendored). Regenerated from upstream every build so a stale copy
        // cannot drift from the active scx SHA. cosmos uses this to guard the one
        // update_freq() divide against a zero divisor (BPF integer divide-by-zero
        // yields 0; native C raises SIGFPE). A patch whose `find` is absent
        // (upstream rename/drift) fails the build loudly rather than silently
        // dropping the transform and shipping a crashing .so.
        if !m.source_patches.is_empty() {
            let src = scx_root.join(format!("scheds/rust/scx_{name}/src/bpf/main.bpf.c"));
            let mut content = std::fs::read_to_string(&src)
                .unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
            for (find, replace) in &m.source_patches {
                assert!(
                    content.contains(find.as_str()),
                    "source_patch find string absent in {} (upstream drift?): {find:?}",
                    src.display()
                );
                content = content.replace(find.as_str(), replace.as_str());
            }
            std::fs::write(out.join(format!("{name}_main_patched.c")), content)
                .unwrap_or_else(|e| panic!("write {name}_main_patched.c: {e}"));
        }

        let mut objs: Vec<PathBuf> = Vec::new();

        // Full-CFLAGS TUs.
        let full_srcs = [
            sched_dir.join("wrapper.c"),
            csrc_dir.join("sim_dsq_iter_glue.c"),
            csrc_dir.join("sim_bpf_stubs.c"),
            scxtest_dir.join("overrides.c"),
        ];
        for src in &full_srcs {
            let obj = out.join(format!("{name}_{}.o", file_stem(src)));
            let mut cmd = Command::new(compiler);
            cmd.args(cflags_base);
            if coverage {
                cmd.args(["-fprofile-instr-generate", "-fcoverage-mapping"]);
            }
            if cgroup_bw_new_api {
                cmd.arg("-DSCX_CGROUP_BW_NEW_API=1");
            }
            cmd.arg("-DSCXSIM_PHASE2_REAL_CGROUP_BW=1");
            cmd.args(&kconfig_defines);
            if strip_const {
                cmd.arg("-Dconst=");
            }
            for inc in base_includes
                .iter()
                .copied()
                .chain(extra_includes.iter().map(PathBuf::as_path))
            {
                cmd.arg("-I").arg(inc);
            }
            cmd.arg("-c").arg("-o").arg(&obj).arg(src);
            if let Err(e) = run(cmd, &format!("compile {} for {name}", src.display())) {
                failed.push(e);
                continue 'sched;
            }
            objs.push(obj);
        }

        // Special TUs: CFLAGS_BASE only.
        for tu in [
            "sim_sigfpe.c",
            "sim_rbc_trampoline.c",
            "sim_deterministic_mem.c",
        ] {
            let src = csrc_dir.join(tu);
            let obj = out.join(format!("{name}_{}.o", file_stem(&src)));
            let mut cmd = Command::new(compiler);
            cmd.args(cflags_base);
            cmd.arg("-c").arg("-o").arg(&obj).arg(&src);
            if let Err(e) = run(cmd, &format!("compile {tu} for {name}")) {
                failed.push(e);
                continue 'sched;
            }
            objs.push(obj);
        }

        // Link the .so. `-Wl,--init=e9_so_init` gives the otherwise
        // freestanding (`-nostdlib`) library a DT_INIT so e9tool's loader
        // runs it; the coverage build swaps `-nostdlib` for the profile
        // runtime instead.
        let so = out.join(format!("libscx_{name}.so"));
        let mut link = Command::new(compiler);
        link.arg("-shared");
        if coverage {
            link.arg("-fprofile-instr-generate");
        } else {
            link.arg("-nostdlib");
        }
        link.arg("-Wl,--init=e9_so_init").arg("-o").arg(&so);
        for obj in &objs {
            link.arg(obj);
        }
        if let Err(e) = run(link, &format!("link libscx_{name}.so")) {
            failed.push(e);
        }
    }
    assert!(
        failed.is_empty(),
        "{} scheduler build step(s) failed (compiler diagnostics above):\n  {}",
        failed.len(),
        failed.join("\n  ")
    );
}

/// File stem of a C source as a `&str` (e.g. `sim_bpf_stubs.c` → `sim_bpf_stubs`).
fn file_stem(p: &Path) -> &str {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| panic!("source path has no UTF-8 stem: {}", p.display()))
}

/// Run a compile/link command. A non-zero exit is returned as `Err(desc)` so
/// the caller can collect every failing scheduler before failing the build;
/// failing to spawn the compiler at all still panics immediately.
fn run(mut cmd: Command, desc: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("spawn failed ({desc}): {e}"));
    if status.success() {
        Ok(())
    } else {
        Err(format!("{desc} failed: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `standalone_definitions()` must reproduce the `SCHEDULERS` manifest const
    /// field-for-field, so the owned provider is a faithful stand-in for the const
    /// everywhere the build and runtime later consume it.
    #[test]
    fn standalone_definitions_match_manifest() {
        let defs = standalone_definitions();
        assert_eq!(defs.len(), SCHEDULERS.len());
        for (d, m) in defs.iter().zip(SCHEDULERS.iter()) {
            assert_eq!(d.name, m.name);
            assert_eq!(d.strip_const, m.strip_const);
            assert_eq!(d.scx_bpf_dir, m.scx_bpf_dir);
            assert_eq!(d.extra_local_include, m.extra_local_include);
            let want_patches: Vec<(String, String)> = m
                .source_patches
                .iter()
                .map(|(f, r)| (f.to_string(), r.to_string()))
                .collect();
            assert_eq!(d.source_patches, want_patches);
            let want: Vec<(String, ConfigValue)> = m
                .runtime
                .rodata
                .iter()
                .map(|(s, v)| (s.to_string(), *v))
                .collect();
            assert_eq!(d.rodata, want);
        }
    }

    /// `new` encodes the common non-`simple` build-side profile, so an embedder's
    /// `SchedulerDefinition::new(name)` reproduces a plain scheduler's manifest
    /// policy (the build-side flags) without restating it; `with_rodata` sets the
    /// one field `new` leaves empty. `new`'s defaults deliberately do NOT match
    /// lavd/cosmos (which override extra_local_include/source_patches) or `simple`.
    #[test]
    fn new_defaults_match_plain_scheduler_manifest() {
        let def = SchedulerDefinition::new("mitosis");
        assert_eq!(def.name, "mitosis");
        assert!(def.strip_const);
        assert!(def.scx_bpf_dir);
        assert!(!def.extra_local_include);
        assert!(def.source_patches.is_empty());
        assert!(def.rodata.is_empty());

        // new()'s build-side flags equal the manifest entry for a plain
        // non-`simple` scheduler (mitosis); the only field new() leaves to
        // with_rodata is rodata (which the mitosis manifest entry populates).
        let mitosis = standalone_definitions()
            .into_iter()
            .find(|d| d.name == "mitosis")
            .expect("mitosis in bundled set");
        assert_eq!(def.strip_const, mitosis.strip_const);
        assert_eq!(def.scx_bpf_dir, mitosis.scx_bpf_dir);
        assert_eq!(def.extra_local_include, mitosis.extra_local_include);
        assert_eq!(def.source_patches, mitosis.source_patches);
        assert!(
            !mitosis.rodata.is_empty(),
            "mitosis manifest has rodata that new() intentionally omits"
        );

        let with = SchedulerDefinition::new("x")
            .with_rodata(vec![("nr_cpu_ids".to_string(), ConfigValue::NumCpus)]);
        assert_eq!(
            with.rodata,
            vec![("nr_cpu_ids".to_string(), ConfigValue::NumCpus)]
        );
    }

    /// Every fluent `with_*` setter overrides exactly its own field and leaves the
    /// rest at `new()`'s defaults, and the setters compose in a chain.
    ///
    /// These are the embedder-facing constructors: an embedder that cannot use the
    /// bundled manifest builds its `SchedulerDefinition` through this chain, so a
    /// setter writing the wrong field would silently mis-build that embedder's `.so`
    /// (wrong include set, unstripped const, a dropped source patch) rather than
    /// fail loudly. Pinned here because the standalone build never exercises them —
    /// it reads `SCHEDULERS` directly.
    #[test]
    fn fluent_setters_override_only_their_own_field() {
        let base = SchedulerDefinition::new("x");
        assert!(base.strip_const, "new() defaults strip_const = true");
        assert!(base.scx_bpf_dir, "new() defaults scx_bpf_dir = true");
        assert!(
            !base.extra_local_include,
            "new() defaults extra_local_include = false"
        );
        assert!(base.source_patches.is_empty(), "new() has no patches");

        let patches = vec![("find".to_string(), "replace".to_string())];
        let patched = SchedulerDefinition::new("x").with_source_patches(patches.clone());
        assert_eq!(patched.source_patches, patches);
        assert!(patched.strip_const, "unrelated field untouched");
        assert!(patched.scx_bpf_dir, "unrelated field untouched");

        let stripped = SchedulerDefinition::new("x").with_strip_const(false);
        assert!(!stripped.strip_const);
        assert!(stripped.scx_bpf_dir, "unrelated field untouched");

        let no_bpf_dir = SchedulerDefinition::new("x").with_scx_bpf_dir(false);
        assert!(!no_bpf_dir.scx_bpf_dir);
        assert!(no_bpf_dir.strip_const, "unrelated field untouched");

        let local_inc = SchedulerDefinition::new("x").with_extra_local_include(true);
        assert!(local_inc.extra_local_include);
        assert!(local_inc.strip_const, "unrelated field untouched");

        // Chained: the `simple` profile (local source, no scx bpf dir) plus a patch.
        let chained = SchedulerDefinition::new("simple")
            .with_strip_const(false)
            .with_scx_bpf_dir(false)
            .with_extra_local_include(true)
            .with_source_patches(patches.clone())
            .with_rodata(vec![("nr_cpu_ids".to_string(), ConfigValue::NumCpus)]);
        assert_eq!(chained.name, "simple");
        assert!(!chained.strip_const);
        assert!(!chained.scx_bpf_dir);
        assert!(chained.extra_local_include);
        assert_eq!(chained.source_patches, patches);
        assert_eq!(
            chained.rodata,
            vec![("nr_cpu_ids".to_string(), ConfigValue::NumCpus)]
        );
    }

    /// `scx_include_paths` with `None` returns exactly the scx-derived `-I` dirs
    /// in the order the standalone build.rs used inline, excluding `<scx_root>/lib`
    /// (build_schedulers appends that -- double-add hazard) and the caller's
    /// crate-local csrc/scxtest; with `Some(override)` it replaces the two vendored
    /// vmlinux entries in-slot with the override dir.
    #[test]
    fn scx_include_paths_order_and_contents() {
        let scx = Path::new("/scx");
        let bpf = Path::new("/bpf/include");

        // Default (None): the vendored, scx-versioned vmlinux entries, in the
        // historical -I order. Must exclude <scx_root>/lib (build_schedulers
        // appends it) and the caller's crate-local csrc/scxtest.
        let got = scx_include_paths(scx, bpf, None);
        assert_eq!(
            got,
            vec![
                PathBuf::from("/scx/scheds/include"),
                PathBuf::from("/scx/scheds/include/lib"),
                PathBuf::from("/scx/scheds/vmlinux"),
                PathBuf::from("/scx/scheds/vmlinux/arch/x86"),
                PathBuf::from("/scx/scheds/include/bpf-compat"),
                PathBuf::from("/bpf/include"),
            ]
        );
        assert!(!got.iter().any(|p| p == Path::new("/scx/lib")));

        // Override (Some): the embedder's kernel-derived vmlinux dir REPLACES the
        // two vendored scheds/vmlinux entries in the same slot; the vendored ones
        // no longer appear, and the surrounding order is preserved.
        let km = Path::new("/kernel/vmlinux");
        let got = scx_include_paths(scx, bpf, Some(km));
        assert_eq!(
            got,
            vec![
                PathBuf::from("/scx/scheds/include"),
                PathBuf::from("/scx/scheds/include/lib"),
                PathBuf::from("/kernel/vmlinux"),
                PathBuf::from("/scx/scheds/include/bpf-compat"),
                PathBuf::from("/bpf/include"),
            ]
        );
        assert!(!got.iter().any(|p| p == Path::new("/scx/scheds/vmlinux")));
        assert!(!got
            .iter()
            .any(|p| p == Path::new("/scx/scheds/vmlinux/arch/x86")));
    }

    /// `header_has_new_cgroup_bw_api` matches only a line-anchored
    /// `struct scx_task_cgroup_bw` declaration -- the same shape as
    /// schedulers/Makefile's `grep '^struct scx_task_cgroup_bw'`.
    #[test]
    fn cgroup_bw_api_probe_is_line_anchored() {
        assert!(header_has_new_cgroup_bw_api(
            "struct foo;\nstruct scx_task_cgroup_bw {\n\tu64 a;\n};\n"
        ));
        // Leading whitespace must NOT match (the Makefile grep is `^struct ...`).
        assert!(!header_has_new_cgroup_bw_api(
            "    struct scx_task_cgroup_bw {\n};\n"
        ));
        // A mention inside a comment must NOT match.
        assert!(!header_has_new_cgroup_bw_api(
            "// struct scx_task_cgroup_bw is new\nstruct other;\n"
        ));
        // A different struct declaration must NOT match.
        assert!(!header_has_new_cgroup_bw_api("struct scx_task;\n"));
    }

    /// `KernelConfig::default()` (all None) emits no -D, so the standalone build
    /// stays byte-identical; each `Some` emits the integer-encoded
    /// `-DSIM_<NAME>` (bools as 1/0, version/hz as decimal), in field order.
    #[test]
    fn kernel_config_cflag_defines() {
        assert!(KernelConfig::default().cflag_defines().is_empty());

        let kc = KernelConfig {
            kernel_version: Some(0x07_0100),
            preempt_rcu: Some(true),
            hz: Some(1000),
            no_hz_idle: Some(false),
        };
        let defs = kc.cflag_defines();
        assert_eq!(defs.len(), 4);
        // Version is decimal-encoded (not hex), matching the C integer literal.
        assert_eq!(
            defs[0],
            format!("-DSIM_LINUX_KERNEL_VERSION={}", 0x07_0100u32)
        );
        assert_eq!(defs[1], "-DSIM_CONFIG_PREEMPT_RCU=1");
        assert_eq!(defs[2], "-DSIM_CONFIG_HZ=1000");
        assert_eq!(defs[3], "-DSIM_CONFIG_NO_HZ_IDLE=0");

        // Only the Some fields appear.
        let only = KernelConfig {
            no_hz_idle: Some(true),
            ..Default::default()
        };
        assert_eq!(only.cflag_defines(), vec!["-DSIM_CONFIG_NO_HZ_IDLE=1"]);
    }
}
