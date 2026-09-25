# Embedding scx_simulator in-process (cargo-ktstr and other consumers)

This is the contract for a consumer, cargo-ktstr first, that builds scheduler
`.so` files in its own build script and runs them in-process through the
deterministic simulator. It covers crates.io and path dependencies alike; there
is no standalone `scxsim` binary and no `make` involved. Every claim below was
checked against the tree this document ships in (scx pin
`413031d445506fb327f997d2b693089d53971ec8`). APIs are cited by symbol. The
measurements are summarised in `SCXSIM_CRATESIO_RELEASE_CANDIDATE_20260924.md`
next to this file. Scripts and raw summaries live in the harness repo, under
[`experiments/scxsim_cratesio_rc_20260924/`](https://github.com/rrnewton/dev-sched-test/tree/main/experiments/scxsim_cratesio_rc_20260924).

The crates a consumer uses:

- `scx_simulator` (runtime): load a `.so`, run a `Scenario`, read a `Trace`.
  A normal `[dependencies]` entry with `default-features = false`.
- `scxsim-build` (build-dependency): the host-export link arguments and the
  `.so` build driver. It is needed as a normal dependency only to call
  `standalone_definitions()` at runtime: the runtime entry points take a
  `SchedulerDefinition`, and `scx_simulator::prelude` re-exports
  `SchedulerDefinition`, `ConfigValue` and `KernelConfig`.
- `scxsim-workload-ir` (optional): the restricted workload IR and the
  lowering from ktstr's op vocabulary. `to_scenario`, which produces a
  `Scenario`, needs the `ingest` feature. Without `ingest` the crate's
  dependency graph holds no scx-sim crate at all, build dependencies included.

Worked examples in this tree, all built through the interfaces documented here
rather than workspace paths:

- `crates/embed_harness` builds and runs `libscx_simple.so`.
- `crates/ktstr-scenario-replay` does the same, then lowers ktstr ops to a
  `Scenario`.
- `crates/embed_unexported` is the negative control: it has no build script,
  so its load is refused.

## Read this first: the host-export trap

A scheduler `.so` takes the simulator's map, kfunc, SDT-task, arena and ATQ
symbols from the dynamic symbol table of the binary that loads it. It is opened
with `dlopen(RTLD_NOW | RTLD_LOCAL)`, and the global scope, which starts with
the executable, is searched before the `.so`'s own definitions. By default a
Rust binary exports none of these symbols. `cargo:rustc-link-arg` does not reach
dependents, so scx_simulator's own build script cannot export them for you.
`scxsim_build::HOST_EXPORTS` lists the 55 such symbols defined by
scx_simulator's C static libraries. Each entry is classed (`IfUnexported`) by
what happens when the loading binary does not export it:

| `IfUnexported` | entries | what a bundled `.so` does when the binary does not export it |
|---|---|---|
| `DlopenFails` | 11 | refers to it strongly: `dlopen` fails with `undefined symbol` |
| `OwnDefinitionBinds` | 24 | carries its own copy: `dlopen` succeeds and the scheduler runs that copy |
| `ResolvesToNull` | 20 | refers to it weakly: `dlopen` succeeds with the reference bound to NULL |

**44 of the 55 fail silently.** The worst of them are a No-Stub violation at
runtime. `crates/scx_simulator/scxtest/overrides.c` is linked into every
scheduler `.so`, and it defines `scx_task_alloc`, `scx_task_data`,
`scx_task_free` and `scx_atq_create_internal` as `__weak` fallbacks. The first
two return NULL, `scx_atq_create_internal` returns 0 and `scx_task_free` does
nothing. All six bundled `.so` files carry them. A binary that loads a `.so`
without exporting the simulator's definitions gets these fallbacks in place of
the simulator's SDT task storage and ATQ. `dlopen` succeeds and nothing is
reported, so the run is green, quiet, and is not running the real scheduler.
The other 20 `OwnDefinitionBinds` entries and all 20 `ResolvesToNull` entries
fail silently in the same way.

An in-process consumer is exactly the configuration exposed to this. The
arguments that prevent it are not transitive, so every consumer starts without
them.

**Publishing without the load-time probe would have exposed external users to
silent stub-binding. The probe turns it into one actionable error that names
the fix.** Before `dlopen`, `try_load_with_definition` looks up every
`HOST_EXPORTS` name with `dlsym(RTLD_DEFAULT)`. If any is missing it refuses
with `LoadError::HostSymbolsNotExported`. This is what a consumer got when its
binary did not export `scx_task_data` and it loaded lavd:

> not loading `<OUT_DIR>/schedulers/libscx_lavd.so`: this binary does not
> export scx_task_data for the scheduler to resolve. Where a scheduler library
> carries its own copy of scx_task_data, dlopen would have succeeded and the
> scheduler would have run that copy (a stub, for most) in place of the
> simulator's definition, silently. Fix: call
> scxsim_build::emit_host_link_args() from the build script of the package
> that builds this binary (cargo does not pass link arguments on to
> dependents)

### How a consumer walks into it (measured, probe disabled)

- **No link arguments at all: the first failure is loud.** Every bundled `.so`
  needs `sim_arena_buf`, `sim_arena_offset` and `scx_test_map_lookup_elem`,
  so `dlopen` fails with `undefined symbol`.
- **The obvious next step leads into the trap.** Suppose a consumer answers
  each failure by exporting the one name it asks for, through a selective
  export list.
  - First round: the 79 names the six `.so` files demand at `dlopen`, plus
    every `HOST_EXPORTS` entry whose omission alone crashed, panicked or
    errored a run in the leave-one-out below (another 11). 5 of the 6
    schedulers died of SIGSEGV. The mechanism in the source:
    `scx_bpf_dsq_insert` and `scx_bpf_dsq_insert_vtime` in the scx
    `compat.bpf.h` look for their newer kfuncs and, finding them absent (the
    unexported ones resolve to NULL), call the weak, unguarded
    `scx_bpf_dispatch___compat` and `scx_bpf_dispatch_vtime___compat`.
  - Adding those two dispatch names (92 in all) got all six to
    `ExitKind::Normal`. Only simple's trace matched a full-contract build.
    lavd never scheduled a task (367 trace events against 16277). The other
    four ran different traces with no error at all.
  - The probe would have named 33 missing exports at that point.
- **Leave-one-out on a selective list.** The base list covered all 125 names
  the six `.so` files resolve from the binary: the 55 `HOST_EXPORTS` entries
  plus the 70 other symbols they reference strongly. Each `HOST_EXPORTS` entry
  was withheld in turn, 55 × 6 = 330 runs:

  | outcome | runs |
  |---|---|
  | identical to baseline | 259 |
  | `dlopen` failure | 30 |
  | silently wrong (`Normal` exit, different trace) | 18 |
  | killed by a signal | 14 |
  | Rust panic | 7 |
  | scheduler error exit | 2 |

  With the probe on, all 330 were refused, each naming exactly the withheld
  symbol. Withholding `scx_task_data` from lavd produced 1 s and 5 s runs that
  exit `Normal` having scheduled nothing. Only at 40 s does the stall watchdog
  fire (`ErrorStall`, at 30 s).
- **Bare `-rdynamic` did not reproduce the silent binding in this tree.** That
  is `-rdynamic` without the per-symbol `--undefined` arguments. It was run at
  three build profiles (release thin LTO, release fat LTO, dev `opt-level=1`)
  against all six schedulers, and matched the full contract 18 of 18 times:
  other references currently pull in every archive member that defines a
  host export. That is a property of today's link and not a guarantee, so the
  contract keeps `--undefined`, and the probe checks the result instead of
  trusting the arguments.

The contract on `integration` before this document's release was different.
It asked consumers to re-emit `EXPORTED_SYMS` (15 names) by hand. Those names
covered 11 `DlopenFails` and 4 `OwnDefinitionBinds` entries. The other 40
entries were missing, including all 20 `ResolvesToNull`. Before sched-test#191
the list had 13 names.

### What the probe does not catch

- **It checks the bundled table, not the `.so` being loaded (sim-4b77c).** A
  `.so` built from your own `wrapper.c` (§3b) can refer to a host symbol that
  `HOST_EXPORTS` does not list, and nothing checks that symbol.
- **It checks presence, not identity (sim-c7ukt).** `dlsym(RTLD_DEFAULT)` finds a
  symbol of that name anywhere in the global scope, so another library's copy
  passes.
- **glibc can override the `.so`'s own definitions (sim-o3kct).**
  `csrc/sim_deterministic_mem.c` gives each `.so` its own memory functions, and
  glibc supplies some of them first (`memcpy`, `memset`, `calloc`, `free`). The
  known-gap test that records this is in `tests/symbol_export.rs`.

The probe checks the whole table, deliberately, so it also refuses a `.so`
that never references the missing symbol: simple is refused for a missing
`scx_task_data`. What a binary exports is a property of the binary, not of
one scheduler.

Guards in the tree:

- `crates/scx_simulator/tests/symbol_export.rs`:
  - `host_exports_bind_to_this_binary`
  - `every_silent_binding_to_this_binary_is_listed`
  - `host_export_classes_match_the_so_files`
- `crates/embed_unexported/tests/refused.rs`:
  `a_binary_without_host_link_args_is_refused_before_dlopen`

## 1. Dependencies

```toml
[dependencies]
scx_simulator = { version = "1.0.0", default-features = false }
# Optional: the workload IR and ktstr lowering; `ingest` adds to_scenario.
scxsim-workload-ir = { version = "0.1.0", features = ["ingest"] }

[build-dependencies]
scxsim-build = "0.1.0"
```

- **`scx_simulator` must be listed under `[dependencies]` of the package whose
  build script builds the `.so`.** Its build inputs reach that build script as
  `DEP_SCXSIM_*` (it declares `links = "scxsim"`), and cargo passes a links
  crate's metadata only to packages that depend on it directly as a normal
  dependency. A dev- or build-dependency does not count.
  `SimBuildInputs::from_dep_env()` panics with that explanation if the inputs
  are absent.
- **`default-features = false`** drops `standalone`. That feature bakes the
  bundled `.so` directory into the crate at compile time and adds the
  convenience constructors (`DynamicScheduler::simple()` and friends). Cargo
  unifies features across the packages it builds together, so a `--workspace`
  build can turn `standalone` back on if another member asks for it
  (`crates/embed_harness/Cargo.toml` explains its own case).
- **scx_simulator's own build compiles the six bundled `.so` files on every
  build**, whether you load them or not (sim-ybrh9). clang is required either
  way.
- **libbpf-sys comes with scx_simulator**, which uses only its headers.
  libbpf-sys declares `links = "bpf"`, so a dependency graph holds exactly one
  version. scx_simulator requires `1.6.1` as a caret requirement, with default
  features on. Without the default `vendored-libbpf` feature,
  `DEP_BPF_INCLUDE` names a directory libbpf-sys never fills. Your graph
  therefore gets the vendored libbpf build. The scratch consumer resolved
  libbpf-sys 1.7.0+v1.7.0 alongside libbpf-rs 0.26.2.
- **MSRV:** `rust-version = "1.88"` on the whole publish set. This was
  measured: 1.87.0 fails on let chains, and 1.88.0 reproduces the 1.97.1
  fingerprints. CI does not re-check it yet (sim-ojycs).
- **Tools:** clang, or whatever `$BPF_CLANG` names, plus what libbpf-sys's
  build needs.

## 2. Link arguments and the load-time probe

Every package whose binaries load a scheduler `.so` calls this from its own
build script. Binaries here means bins, tests, examples and benches.

```rust
scxsim_build::emit_host_link_args();
```

It emits `-rdynamic` plus one `-Wl,--undefined=<name>` per `HOST_EXPORTS` entry.
If you drive the linker yourself, `host_link_args()` yields the same list.

It is **not transitive**. `cargo:rustc-link-arg` applies only to the calling
package's own targets; the variant that propagates is nightly-only. So a
library crate that wraps scx_simulator cannot do this for its users, and each
package that produces a loading binary must. In this tree these packages do:

- `scx_simulator`
- `scxsim-workload-ir`, for its `ingest` test
- `scxsim-calibration`
- `ktstr-scenario-replay`
- `embed_harness`

`emit_host_link_args()` reaches every target the package links, including any
binaries it ships: `-rdynamic` puts every global symbol of each into its
dynamic symbol table, and the `--undefined` arguments link the simulator's
definitions of the 55 names into each. A package in
which only some targets load a `.so` can scope the same arguments with cargo's
per-target instructions:

```rust
for arg in scxsim_build::host_link_args() {
    println!("cargo:rustc-link-arg-tests={arg}");
}
```

`-tests` reaches `[[test]]` targets only, not the library's unit tests. Cargo
rejects `cargo:rustc-link-arg-tests` in a package with no test target, so emit
it only from a package that has one. The other scopes are `-bins`, `-bin=<NAME>`, `-examples` and `-benches`. The probe
checks the result whichever you choose. ktstr's candidate `scxsim` feature
scopes them this way, because only one of its test targets loads a `.so`.

You can use a selective export list instead of `-rdynamic`: a version script,
`--dynamic-list` or `--export-dynamic-symbol`. If you do, it must cover every
symbol any `.so` resolves from the binary. That is the 55 `HOST_EXPORTS`
entries plus everything else the `.so` files reference strongly, 125 names
for the six bundled `.so` files at this pin. Keep `--undefined` for the 55
entries. Nothing else is guaranteed to pull their archive members into the
link.

The probe runs first in `try_load_with_definition`, before the arena is
entered and before `dlopen`. It has no side effects, so a refused load leaves
nothing to clean up. `load_with_definition` panics with the same message.
`HostSymbolsNotExported { path, missing }` carries the absent `HostExport`s,
each with its `IfUnexported` class. The message groups them by consequence and
ends with the fix.

## 3. Building the `.so`

### 3a. A bundled scheduler: `SimBuildInputs::build_bundled`

This is `crates/embed_harness/build.rs` with its comments removed. It is the
whole build-side contract:

```rust
use std::env;
use std::path::PathBuf;
use scxsim_build::{emit_host_link_args, KernelConfig, SchedulerDefinition, SimBuildInputs};
fn main() {
    emit_host_link_args();
    let inputs = SimBuildInputs::from_dep_env();
    let simple_def = SchedulerDefinition::new("simple")
        .with_strip_const(false)
        .with_scx_bpf_dir(false);
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();
    let so_dir = inputs.build_bundled(
        std::slice::from_ref(&simple_def),
        &out_dir,
        &KernelConfig::default(),
    );
    println!("cargo:rustc-env=HARNESS_SO_DIR={}", so_dir.display());
}
```

`SimBuildInputs::from_dep_env()` returns what scx_simulator was built with,
wherever cargo unpacked it: its C sources (`csrc`, `scxtest`), its scx tree
(`scx_root`), the libbpf headers (`bpf_include`) and the bundled wrapper
directory (`schedulers`).

`build_bundled(defs, out_dir, kernel_config)` works in three steps:

1. It stages one symlink per definition under `out_dir/schedulers_src`.
2. It compiles `libscx_<name>.so` into `out_dir/schedulers` with `$BPF_CLANG`,
   falling back to `clang`, and emits the rerun triggers.
3. It returns the output directory.

Hand that directory to your runtime through `cargo:rustc-env`. Every entry in
`defs` must name a bundled scheduler: simple, tickless, cosmos, mitosis, lavd
or layered.

### 3b. A scheduler scx-sim does not bundle: `build_schedulers`

Write `<src>/<name>/wrapper.c`, modelled on the bundled ones under
`crates/scx_simulator/schedulers/`. Then call `scxsim_build::build_schedulers`
directly, the same way `build_bundled` does:

```rust
let inputs = SimBuildInputs::from_dep_env();
let compiler = std::env::var("BPF_CLANG").unwrap_or_else(|_| "clang".into());
scxsim_build::build_schedulers(
    &my_src,              // holds <name>/wrapper.c for each def
    &defs,
    &so_dir,              // receives libscx_<name>.so
    &inputs.csrc,
    &inputs.scxtest,
    &inputs.include_paths(),
    &inputs.scx_root,
    &compiler,
    false,                // coverage: standalone-only instrumentation
    scxsim_build::cgroup_bw_new_api(&inputs.scx_root),
    &KernelConfig::default(),
);
```

This path is not exercised by any downstream test in this tree, and the probe
does not cover a host symbol that only your wrapper refers to (sim-4b77c).
Every `wrapper.c` subdirectory of `my_src` is compiled.

## 4. `SchedulerDefinition`

- **Construct it with the fluent chain**, which is the canonical form:
  `SchedulerDefinition::new(name)` followed by `.with_rodata(..)`,
  `.with_strip_const(..)` and so on. The fields are `pub`, so assignment also
  works; `crates/embed_harness/tests/embed.rs` builds its definition that way.
- **Start from the bundled definitions.** `standalone_definitions()` returns the
  six bundled definitions exactly as scx-sim builds them.
- **Build and load with the same definition.** Rodata is applied at load time
  from the definition you pass.
- **`with_rodata`** gives each `(global, ConfigValue)` pair, written into the
  `.so`'s const-volatile global before any op runs. A global the `.so` does
  not define gives `LoadError::MissingRodataGlobal`. The write width is not
  checked against the C type (sim-65zbt).
- **`with_source_patches`** takes `(from, to)` text replacements applied to the
  scheduler's `main.bpf.c` before it is compiled. Only cosmos's wrapper
  compiles the patched copy (sim-ld1ro):
  - tickless, lavd and layered build from the unpatched source and say nothing;
  - mitosis and simple panic, because they have no `main.bpf.c` under
    `scx_root` to patch.
- **`with_extra_local_include`** appears to be vestigial (sim-5z1nt).

## 5. vmlinux.h: no per-`.so` override

Every `.so` compiles against scx's vendored x86 `scheds/vmlinux` for this pin,
and there is no way to substitute a kernel-derived header. The host static
libraries allocate `task_struct`, `cgroup` and `css_set` themselves. No CO-RE
relocation happens between a `.so` and the host, so a `.so` built against a
different layout reads the host's structures at the wrong offsets.

Measured with 17 size and offset probes on `task_struct`, `sched_ext_entity`,
`cgroup`, `css_set` and `kernfs_node`:

- The host kernel's (7.1.3) `vmlinux.h` differs from the vendored header in 15
  of the 17. It does not build at all (it has no `struct scx_cmask`).
- scx's arm64 header differs in 7 of the 17. Built with it, simple ran
  identical, while lavd, mitosis and cosmos died of SIGSEGV.

Pass the kernel under test through `KernelConfig` (§7). A sound override is
sim-004y6.

## 6. scx_root: which scx tree

- **By default** scx_simulator builds against the scx subset it bundles under
  `vendor/scx`. In a sched-test checkout those are symlinks into the submodule;
  `cargo package` makes them real files.
- **`SCX_ROOT`** can name another scx checkout. `resolve_scx_root` then
  canonicalises it, requires `scheds/include`, and reruns the build script when
  the variable changes. That existence check is the only one: a tree whose
  sources do not match the bundled wrappers fails at compile time.
- **`SCX_ROOT` also redirects the upstream Rust sources** that
  `scx_layered_alloc` and `scx_layered_growth` compile verbatim
  (`emit_bundled_upstream_module`). The `.so` files and that Rust therefore
  come from one tree.
- **lavd's lib bodies:** production lavd compiles them from
  `scheds/rust/scx_lavd/src/bpf/lib`, which at this pin is a symlink to
  `scx/lib`. Those are the same files the simulator compiles through
  `-I<scx_root>/lib`.
- **The packaged crates are enough.** The scratch consumer built all six `.so`
  files from the packaged crates alone, with the vendored subset and no
  submodule, and matched the in-tree fingerprints.

## 7. `KernelConfig`

Four optional scalars, compiled into each `.so` as `-DSIM_<NAME>`:

- `kernel_version`, encoded `major<<16 | minor<<8 | patch`, default `0x061200`
- `preempt_rcu`, default false
- `hz`, default 250
- `no_hz_idle`, default false

`KernelConfig::default()`, with every field `None`, gives a byte-identical
standalone build. The values live in the `.so` alone; the host static
libraries are built once, with no `KernelConfig`.

What each field actually changes in the simulator today:

- **`no_hz_idle`** changes scheduling: it gates lavd's sys_stat idle-drift
  branch.
- **`kernel_version` and `preempt_rcu`** are a coupled pair; set them
  together. They feed scx's `is_migration_disabled`, which the simulator
  overrides with ground truth, so for now they reach only the dump banner.
- **`hz`** is tickless's fallback when `tick_freq` is 0.

## 8. Runtime: load and run

```rust
use scx_simulator::prelude::*;

fn run(so: &str, def: &SchedulerDefinition, scenario: Scenario)
    -> Result<Trace, LoadError>
{
    // The compiled C scheduler has global state: one simulation per process at a time.
    let _guard = scx_simulator::SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sched = DynamicScheduler::try_load_with_definition(so, def, scenario.nr_cpus)?;
    Ok(Simulator::new(sched).run(scenario))
}
```

- **The load order** is: the host-export probe (§2), then the arena, then
  `dlopen(RTLD_NOW | RTLD_LOCAL)`, then `{prefix}_setup(nr_cpus)` if the `.so`
  defines it, then `sim_arena_mark_persistent`, then the ops lookup
  (`MissingOp`), then rodata (`MissingRodataGlobal`). So `{prefix}_setup` runs
  before the definition's rodata is written. A `.so` that cannot be opened gives
  `LibraryOpen`.
- **`LoadError` is `#[non_exhaustive]`.** Give any `match` on it a wildcard
  arm.
- **Loading your own definition:** `load` and `try_load` take a prefix and
  look the definition up among the bundled ones, giving `UnknownPrefix` for
  anything else. To load a definition you built, use `*_with_definition`.
- **`SIM_LOCK` is your obligation.** Nothing in the load or run path takes it
  (sim-0qpv9). Under `cargo nextest` each test is its own process; under
  `cargo test`, tests share one.
- **`Simulator::run` prints to stdout** unconditionally when a run ends: the
  simulation summary, the structop summary and preemption statistics. A
  consumer that captures stdout will see them.
- **Building a `Scenario`:** either use `Scenario::builder()`
  (`crates/embed_harness/tests/embed.rs` has a minimal one-CPU, one-task
  example), or lower ktstr ops with `scxsim_workload_ir::lower` and then call
  `to_scenario` (feature `ingest`).
- **Checking the result:** `trace.exit_kind()` returns a `&ExitKind`.
  `ExitKind::Normal` means the scenario ran to its end.

## Who owns what

scx-sim owns:

- `HOST_EXPORTS` and the probe
- the `.so` build driver and the bundled wrappers and definitions
- the engine
- the `SchedulerDefinition` and `KernelConfig` types

The consumer owns:

- calling `emit_host_link_args()` in every package that produces a loading
  binary
- building its `.so` files in its build script, and passing their directory to
  its runtime
- holding `SIM_LOCK`
- choosing each definition's rodata and the `KernelConfig`
- setting `SCX_ROOT` when it builds against its own scx tree
