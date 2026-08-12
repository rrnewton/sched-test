# Embedding scx_simulator in cargo-ktstr

This is the contract a `cargo-ktstr` maintainer implements to drive scx-sim as a
library: build each scheduler's `.so` and run it in-process through the
deterministic simulator, with no standalone `scxsim` binary and no `make`. It is
a specification only — it adds no code to scx-sim and requires no edits to ktstr
to be valid; it records the API surface and the build-script steps an embedder
performs. Every API below is cited to its definition so the spec can be
re-verified against the source.

scx-sim is consumed as two workspace crates:

- `scxsim-build` (build-dependency): the manifest types and the `.so` build
  driver. `crates/scxsim-build/src/lib.rs`.
- `scx_simulator` (dependency): the runtime — load a `.so`, run a `Scenario`,
  read a `Trace`. `crates/scx_simulator/src/lib.rs`.

## 1. Dependency edges

The embedder binary (the cargo-ktstr subcommand that runs the sim) declares:

```toml
[dependencies]
scx_simulator = { ... }   # DynamicScheduler, Simulator, Scenario, Trace, ExitKind
scxsim-build  = { ... }   # SchedulerDefinition, KernelConfig (runtime-constructed inputs)
libbpf-sys    = "=1.6.1"  # NORMAL dep: exposes DEP_BPF_INCLUDE to the build script

[build-dependencies]
scxsim-build  = { ... }   # build.rs uses EXPORTED_SYMS, build_schedulers, scx_include_paths,
                          # resolve_scx_root, cgroup_bw_new_api + the SchedulerDefinition /
                          # ConfigValue / Codegen / KernelConfig types (it constructs the inputs)
```

`libbpf-sys` MUST be a normal dependency, not a build-dependency: cargo surfaces
a `links` crate's metadata (here `DEP_BPF_INCLUDE`, the libbpf header dir) to a
package's build script only for its NORMAL dependencies. This mirrors
`scx_simulator`'s own Cargo.toml (`crates/scx_simulator/Cargo.toml`,
`libbpf-sys = "=1.6.1"`). The in-repo `embed_harness` crate
(`crates/embed_harness/Cargo.toml`) is the worked example of this whole edge set.

## 2. build.rs: the kfunc-export link contract (load-bearing)

A dlopen'd scheduler `.so` resolves a fixed set of kfunc / SDT / arena symbols
from the loading binary's dynamic symbol table. Those symbols are forced into the
binary by `-rdynamic` + per-symbol `-Wl,--undefined`, emitted as
`cargo:rustc-link-arg`. Those directives are **non-transitive**: `scx_simulator`'s
own build script emits them for ITS binaries only, so every downstream embedder
MUST re-emit them or the `.so` fails to load (`load_with_definition` dlopens with
`RTLD_NOW`, so a missing symbol fails the open with `undefined symbol: <name>`).

The embedder build.rs re-emits the set from the single source of truth,
`scxsim_build::EXPORTED_SYMS` (`crates/scxsim-build/src/lib.rs`):

```rust
use scxsim_build::EXPORTED_SYMS;
println!("cargo:rustc-link-arg=-rdynamic");
for sym in EXPORTED_SYMS {
    println!("cargo:rustc-link-arg=-Wl,--undefined={sym}");
}
```

This is verbatim `scx_simulator`'s own emission (`crates/scx_simulator/build.rs`)
and is proven end-to-end by `crates/embed_harness/build.rs` +
`crates/embed_harness/tests/embed.rs` (the test loads + runs a downstream-built
`libscx_simple.so`; removing the re-emission makes the load fail on
`undefined symbol: sim_arena_offset`).

## 3. build.rs: compiling each scheduler `.so`

The embedder build.rs calls `scxsim_build::build_schedulers`
(`crates/scxsim-build/src/lib.rs`):

```rust
pub fn build_schedulers(
    schedulers_src: &Path,            // dir whose <name>/wrapper.c subdirs are discovered + built
    defs: &[SchedulerDefinition],     // one per scheduler (section 4)
    out: &Path,                       // OUT_DIR subdir for the libscx_<name>.so
    csrc_dir: &Path,                  // scx_simulator's vendored csrc (reuse by path)
    scxtest_dir: &Path,               // scx_simulator's vendored scxtest (reuse by path)
    include_paths: &[PathBuf],        // see scx_include_paths (section 5)
    scx_root: &Path,                  // explicit scx source root (section 6)
    compiler: &str,                   // BPF_CLANG or "clang"
    coverage: bool,                   // false for a normal embed (standalone-only instrumentation)
    cgroup_bw_new_api: bool,          // scxsim_build::cgroup_bw_new_api(scx_root)
    kernel_config: &KernelConfig,     // kernel-config / version overrides (section 7)
)
```

Each `<name>/wrapper.c` subdir under `schedulers_src` is discovered and built into
`libscx_<name>.so` under `out`. The embedder reuses scx_simulator's vendored
`csrc`/`scxtest` by path (no copy); see `crates/embed_harness/build.rs` for the
exact assembly.

## 4. ktstr Scheduler -> SchedulerDefinition

A ktstr `declare_scheduler!` static carries VM-execution metadata only (name,
`SchedulerSpec`, topology, sched_args) — NO BPF source location, NO sim build
flags, NO rodata. The embedder synthesizes a `scxsim_build::SchedulerDefinition`
(`crates/scxsim-build/src/lib.rs`) per scheduler, constructed from the name it
already holds — there is no cargo-metadata derivation:

```rust
use scxsim_build::{SchedulerDefinition, ConfigValue};
let def = SchedulerDefinition::new(name)            // defaults: strip_const+scx_bpf_dir true,
                                                    // extra_local_include false, codegen None
    .with_rodata(vec![                              // per-scheduler const-volatile globals
        ("nr_cpu_ids".to_string(), ConfigValue::NumCpus),
        // ...
    ]);
// Exceptions are set via the pub fields:
//   simple        => def.strip_const = false; def.scx_bpf_dir = false;
//   lavd / cosmos => def.extra_local_include = true;
//   cosmos        => def.codegen = Some(Codegen::CosmosDivZeroGuard);
```

`new(name)` (`SchedulerDefinition::new`) encodes the common non-`simple` build
profile; `name` is both the `.so`/ops prefix and the key from which
`build_schedulers` derives the BPF source dir (`scx_root`/scheds/rust/scx_<name>/
src/bpf) and the wrapper.c dir (`schedulers_src`/<name>). `ConfigValue::NumCpus`
resolves to the sim CPU count at apply time.

Honest limits (verify against the source before relying on them):

- Per-scheduler `wrapper.c` is still required: `build_schedulers` discovers
  schedulers as `schedulers_src/<name>/wrapper.c` and cross-checks every `def`
  against a discovered dir. A scheduler with no `wrapper.c` is not buildable
  today.
- Map-type coverage is partial: the sim's `scx_test_map` registry covers
  ARRAY/HASH/PERCPU_ARRAY/TASK_STORAGE/CGRP_STORAGE; RINGBUF / LRU_HASH / QUEUE /
  STACK / PERCPU_HASH / ARRAY_OF_MAPS are not yet covered.
- `codegen` is a closed enum (only `CosmosDivZeroGuard`); a scheduler needing a
  different source transform uses the pre-transformed-copy escape hatch
  (`extra_local_include`).

## 5. build.rs: include paths + the kernel-derived vmlinux override

Compose the `-I` set with `scxsim_build::scx_include_paths`
(`crates/scxsim-build/src/lib.rs`):

```rust
pub fn scx_include_paths(scx_root: &Path, bpf_include: &Path, vmlinux_override: Option<&Path>) -> Vec<PathBuf>
```

`-I` resolution is first-match, so order is a build contract: put the
crate-local csrc/scxtest dirs first, then `scx_include_paths(...)`:

```rust
let include_paths: Vec<PathBuf> = [csrc_dir.clone(), scxtest_dir.clone()]
    .into_iter()
    .chain(scxsim_build::scx_include_paths(&scx_root, &bpf_include, vmlinux_override))
    .collect();
```

`vmlinux_override`: ktstr boots a real kernel and can derive the matching
`vmlinux.h`; pass `Some(dir)` (a dir containing `vmlinux.h`) to compile each `.so`
against the kernel-under-test's ABI. `None` uses scx-sim's vendored, scx-versioned
vmlinux (pinned), which is the standalone default. An embedder that overrides the
vmlinux should also `cargo:rerun-if-changed` its override dir (scx-sim's standalone
build watches only the vendored tree).

## 6. scx_root (an explicit input)

`build_schedulers` requires `scx_root: &Path`, and every scheduler's scx sources
derive from it: the headers, the scheduler BPF source, and the scx/lib bodies
lavd compiles in. Note that scx-sim resolves those lib bodies from
`<scx_root>/lib` — `build_schedulers` appends it to the include set, and e.g.
lavd's `wrapper.c` does `#include "ravg.bpf.c"` found via `-I<scx_root>/lib`. That
is a DIFFERENT directory than where production lavd compiles its lib bodies (its
own `scheds/rust/scx_<name>/src/bpf/lib/`). In the current scx tree those two
copies are byte-identical, but for fidelity `scx_root` must be the tree whose
`/lib` matches the scheduler's `src/bpf/lib`.

`scx_root` is an EXPLICIT input the embedder supplies — it is NOT derivable from
cargo metadata. In the current scx tree, `scx_cargo` bundles only headers (an
embedded tar extracted at build time) and a scheduler's lib bodies are LOCAL
`src/bpf/lib/*.bpf.c` files (compiled via `add_source`), with no git/fetch step
and no `SCX_TAG`; `scx_cargo` emits no scx-checkout path in
`cargo build --message-format=json`, so there is nothing to parse.

Recommended: a required explicit `scx_root` the ktstr user/config supplies —
never silently defaulted (No Silent Failures). `scxsim_build::resolve_scx_root`
(`crates/scxsim-build/src/lib.rs`) resolves the `SCX_ROOT` env override (validated
to look like an scx checkout) or a supplied default; it is the same resolver the
standalone build uses.

(If a future scx_cargo gains a fetch step that exposes the checkout path in
cargo-json, capturing it to rev-match each `.so` to the scheduler's exact scx
bodies would be a higher-fidelity option — but that mechanism does not exist in
the current scx tree.)

## 7. KernelConfig: kernel-config / version scalars

`scxsim_build::KernelConfig` (`crates/scxsim-build/src/lib.rs`) lets ktstr compile
each `.so` (and scx_simulator's host sim_task lib) against the kernel-under-test's
`CONFIG_HZ`, `CONFIG_NO_HZ_IDLE`, `CONFIG_PREEMPT_RCU`, and `LINUX_KERNEL_VERSION`:

```rust
let kc = KernelConfig {
    kernel_version: Some(0x06_1200),  // major<<16 | minor<<8 | patch
    preempt_rcu:    Some(false),
    hz:             Some(1000),
    no_hz_idle:     Some(true),
};
```

`KernelConfig::default()` (all `None`) keeps scx-sim's standalone defaults. Two
caveats the embedder must know (from the KernelConfig docs):

- `kernel_version` and `preempt_rcu` are a coupled pair (preempt_rcu
  short-circuits the version check in scx's `is_migration_disabled`); set them
  together.
- In the current sim only `no_hz_idle` changes scheduling (lavd sys_stat); the
  version/preempt_rcu pair and `hz` are shadowed by sim overrides today (the pair
  feeds only the dump banner; `hz` is the `tick_freq==0` fallback). The values
  are still compiled into the `.so` (fidelity-forward).

Thread the SAME `KernelConfig` to `build_schedulers` AND to scx_simulator's host
sim_task compile if the embedder rebuilds it (scx_simulator's build.rs already
does the latter for the standalone default).

## 8. Runtime: load + run

After build.rs produces `libscx_<name>.so` (path exposed via a
`cargo:rustc-env`), the runtime loads it with the SAME `SchedulerDefinition` and
runs a `Scenario`:

```rust
use scx_simulator::prelude::*;    // DynamicScheduler, LoadError, Simulator, Scenario, TaskDef, ExitKind, SchedulerDefinition, ...

// In a fn returning Result<_, LoadError> -- embedders prefer the fallible entry:
let sched = DynamicScheduler::try_load_with_definition(so_path, &def, nr_cpus)?;  // ffi.rs
let trace = Simulator::new(sched).run(scenario);                                 // engine.rs
assert_eq!(trace.exit_kind(), &ExitKind::Normal);
```

Constructing the `Scenario` (lowering ktstr's workload DSL to `Scenario` via
`Scenario::builder()...build()`) is ktstr-side and out of scope for this embed
contract; `crates/embed_harness/tests/embed.rs` has a minimal 1-CPU/1-task
`Scenario` to copy.

Use `load_with_definition` (`crates/scx_simulator/src/unsafe_impl/ffi.rs`), NOT
`load`: `load` resolves the definition from the BUNDLED `standalone_definitions()`
and panics on a prefix it doesn't know, so an embedder loading a scheduler the
bundled set never knew about must pass its own `def`.

An embedder should prefer the FALLIBLE twin
`try_load_with_definition(so_path, &def, nr_cpus) -> Result<DynamicScheduler, LoadError>`
(and `try_load`): a bad `.so`, a missing exported symbol, or an undefined rodata
global is then a recoverable `LoadError` (`LibraryOpen` / `UnknownPrefix` /
`MissingOp` / `MissingRodataGlobal`, exported from `scx_simulator`) instead of a
process abort across the FFI boundary. The infallible `load_with_definition` /
`load` are thin `unwrap`-panic wrappers over the `try_*` forms (correct for the
standalone binary). Either way config fails loud, never silent: the load applies
the definition's const-volatile rodata globals before any ops body runs and
reports (`MissingRodataGlobal`) / panics on any global the `.so` doesn't define,
and a missing exported symbol fails the `RTLD_NOW` load.

## What scx-sim provides vs what the embedder owns

- scx-sim owns: the `.so` build driver, the kfunc/SDT/arena substrate +
  `EXPORTED_SYMS`, the deterministic engine, the `SchedulerDefinition` /
  `KernelConfig` input types and their build-policy defaults.
- The embedder owns: re-emitting the link args, supplying `scx_root` (and an
  optional kernel-derived vmlinux + KernelConfig), and synthesizing each
  `SchedulerDefinition` (name + rodata + the simple/lavd/cosmos exceptions) from
  its own scheduler registry.
