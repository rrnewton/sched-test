//! Drift test for the host-export contract, [`scxsim_build::HOST_EXPORTS`].
//!
//! A scheduler `.so` is dlopen'd `RTLD_NOW | RTLD_LOCAL` into a host linked with
//! `-rdynamic`. The dynamic linker binds each symbol the `.so` reaches through a
//! dynamic relocation to the first definition in the process's global scope --
//! the host binary, then the libraries it started with -- and falls back to the
//! `.so`'s own definition only when the scope has none. So a host export takes
//! the binding whether the `.so` leaves the symbol undefined or carries its own
//! copy; and when the host does not export it, a strong reference fails the
//! load, a weak one binds NULL, and a relocated own copy binds that copy -- the
//! last two silently ([`IfUnexported`]). `HOST_EXPORTS` drives the host link
//! args and the probe that refuses to load a scheduler without them
//! (`LoadError::HostSymbolsNotExported`), so a silently-binding symbol missing
//! from it is a No-Stub hazard that neither covers.
//!
//! These tests read each bundled `.so`'s dynamic symbols and relocations, ask
//! this process's global scope where each symbol binds, and fail when
//!
//! - a symbol a `.so` would bind silently binds to this binary but is not
//!   listed: a host linked with only the listed exports can drop it, and the
//!   probe does not check it;
//! - an entry's [`IfUnexported`] is not what a `.so` that names it shows;
//! - an entry does not bind to this binary.
//!
//! Not covered: a host definition this binary's own link dropped (nothing here
//! exports it, so the `.so` binds silently in this binary too); a scheduler
//! `.so` an embedder builds itself; and a same-named export an embedder's binary
//! carries for its own reasons, which takes the binding just the same. A
//! library does exactly that today: see
//! `known_gap_libc_takes_the_so_memory_functions`.
//!
//! The link args and C static libs reach only a test binary that LINKS
//! `scx_simulator` -- cargo does not pass link args on to dependents, which is
//! why an embedder calls `scxsim_build::emit_host_link_args` itself -- so
//! `host_exports_bind_to_this_binary` runs a real simulation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{c_void, CString};
use std::path::Path;

use object::elf::STV_DEFAULT;
use object::read::elf::ElfFile64;
use object::{Endianness, Object, ObjectSymbol, RelocationTarget, SymbolKind};
use scx_simulator::*;
use scxsim_build::{IfUnexported, HOST_EXPORTS, SCHEDULERS};

mod common;

/// How one `.so`'s dynamic symbol table names a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SoReference {
    /// The `.so` defines it. `interposable`: with default visibility and reached
    /// through one of the `.so`'s dynamic relocations, so a definition in the
    /// global scope takes the binding.
    Defined { interposable: bool },
    /// A weak undefined reference: binds NULL when the global scope has no
    /// definition.
    WeakUndefined,
    /// A strong undefined reference: `dlopen` fails when the global scope has no
    /// definition.
    StrongUndefined,
}

impl SoReference {
    /// The class of a host export that a `.so` names this way.
    fn class(self) -> IfUnexported {
        match self {
            Self::Defined { .. } => IfUnexported::OwnDefinitionBinds,
            Self::WeakUndefined => IfUnexported::ResolvesToNull,
            Self::StrongUndefined => IfUnexported::DlopenFails,
        }
    }

    /// Whether the symbol binds to a global-scope definition when there is one,
    /// and to something else, without complaint, when there is not.
    fn binds_silently(self) -> bool {
        matches!(
            self,
            Self::Defined { interposable: true } | Self::WeakUndefined
        )
    }
}

/// A bundled scheduler `.so`: its scheduler name and the symbols its dynamic
/// symbol table names.
struct BundledSo {
    name: &'static str,
    references: BTreeMap<String, SoReference>,
}

/// Every bundled scheduler `.so`, parsed.
fn bundled_sos() -> Vec<BundledSo> {
    SCHEDULERS
        .iter()
        .map(|manifest| BundledSo {
            name: manifest.name,
            references: so_references(Path::new(&format!(
                "{}/libscx_{}.so",
                env!("SCHEDULER_SO_DIR"),
                manifest.name
            ))),
        })
        .collect()
}

/// The global symbols the dynamic symbol table of the `.so` at `path` names,
/// and how.
fn so_references(path: &Path) -> BTreeMap<String, SoReference> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let elf = ElfFile64::<Endianness>::parse(&*data)
        .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
    let relocated: HashSet<_> = elf
        .dynamic_relocations()
        .into_iter()
        .flatten()
        .filter_map(|(_, relocation)| match relocation.target() {
            RelocationTarget::Symbol(index) => Some(index),
            _ => None,
        })
        .collect();
    assert!(
        !relocated.is_empty(),
        "{} has no symbol relocations, which no scheduler .so lacks: the parse is wrong",
        path.display()
    );
    elf.dynamic_symbols()
        .filter(|sym| {
            !sym.is_local() && !matches!(sym.kind(), SymbolKind::Section | SymbolKind::File)
        })
        .filter_map(|sym| {
            let name = sym.name().ok().filter(|name| !name.is_empty())?;
            let reference = match (sym.is_undefined(), sym.is_weak()) {
                (true, true) => SoReference::WeakUndefined,
                (true, false) => SoReference::StrongUndefined,
                (false, _) => SoReference::Defined {
                    interposable: sym.elf_symbol().st_visibility() == STV_DEFAULT
                        && relocated.contains(&sym.index()),
                },
            };
            Some((name.to_owned(), reference))
        })
        .collect()
}

/// Identifies a loaded object (the host binary, a library) by its load base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoadedObject(usize);

/// The loaded object that maps `addr`.
fn object_mapping(addr: *const c_void) -> LoadedObject {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    // SAFETY: dladdr only reads `addr`, and fills `info` when it returns non-zero.
    let found = unsafe { libc::dladdr(addr, info.as_mut_ptr()) };
    assert_ne!(found, 0, "dladdr found no loaded object mapping {addr:p}");
    // SAFETY: dladdr returned non-zero, so it initialised `info`.
    LoadedObject(unsafe { info.assume_init() }.dli_fbase as usize)
}

/// Where a `.so` loaded now would bind `name`: the object holding the first
/// definition in this process's global scope, or `None` when the scope has
/// none. The load-time probe asks the global scope the same way.
fn global_binding(name: &str) -> Option<LoadedObject> {
    let name = CString::new(name).expect("symbol names contain no NUL");
    // SAFETY: dlsym only reads the NUL-terminated name; RTLD_DEFAULT searches
    // the global scope and loads nothing.
    let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!addr.is_null()).then(|| object_mapping(addr))
}

/// This test binary, the host the bundled `.so` files bind their exports to.
fn this_binary() -> LoadedObject {
    object_mapping(this_binary as *const c_void)
}

/// Every listed host export binds to this binary, after a real simulation has
/// loaded a scheduler that resolves them and run it to completion.
#[test]
fn host_exports_bind_to_this_binary() {
    let _lock = common::setup_test();

    let scenario = Scenario::builder()
        .cpus(1)
        .instant_timing()
        .task(TaskDef {
            name: "worker".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000)],
                repeat: RepeatMode::Once,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .duration_ms(50)
        .build();
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "sim did not complete normally"
    );

    let this_binary = this_binary();
    let elsewhere: Vec<_> = HOST_EXPORTS
        .iter()
        .filter(|export| global_binding(export.name) != Some(this_binary))
        .map(|export| (export.name, global_binding(export.name)))
        .collect();
    assert!(
        elsewhere.is_empty(),
        "host exports that do not bind to this binary, as (symbol, object that \
         binds it instead, None for nothing): {elsewhere:?}. The build.rs link \
         args (scxsim_build::emit_host_link_args) should have forced every one \
         into this binary's dynamic symbol table"
    );
}

/// Every symbol that a bundled `.so` would bind silently and that binds to this
/// binary is a listed host export.
#[test]
fn every_silent_binding_to_this_binary_is_listed() {
    let listed: BTreeSet<&str> = HOST_EXPORTS.iter().map(|export| export.name).collect();
    let this_binary = this_binary();
    let sos = bundled_sos();
    let mut unlisted: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for so in &sos {
        for (name, reference) in &so.references {
            if reference.binds_silently()
                && !listed.contains(name.as_str())
                && global_binding(name) == Some(this_binary)
            {
                unlisted
                    .entry(name)
                    .or_default()
                    .push(format!("libscx_{}.so: {reference:?}", so.name));
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "symbols that bind to this binary, and that a bundled .so would bind \
         silently in a host not exporting them -- to its own copy (Defined) or to \
         NULL (WeakUndefined) -- but that scxsim_build::HOST_EXPORTS does not \
         list, so neither the host link args nor the load-time probe cover them: \
         {unlisted:#?}\nList each with the IfUnexported class the .so shows. If the \
         .so's own copy is the one that must run, give it hidden visibility in the \
         .so instead (as LINUX_KERNEL_VERSION in csrc/sim_bpf_stubs.c) and stop \
         the host from exporting the name"
    );
}

/// Every listed host export's class is what each bundled `.so` that names it
/// shows. Vacuous for an entry no bundled `.so` names (today the two e9patch
/// symbols, which only instrumented builds reference).
#[test]
fn host_export_classes_match_the_so_files() {
    let mut mismatches = Vec::new();
    for so in &bundled_sos() {
        for export in HOST_EXPORTS {
            if let Some(reference) = so.references.get(export.name) {
                if reference.class() != export.if_unexported {
                    mismatches.push(format!(
                        "{} in libscx_{}.so: listed as {:?}, but the .so shows {reference:?}, \
                         which is {:?}",
                        export.name,
                        so.name,
                        export.if_unexported,
                        reference.class()
                    ));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "scxsim_build::HOST_EXPORTS entries whose IfUnexported class disagrees with \
         the .so files, so the load-time probe would describe a missing export \
         wrongly: {mismatches:#?}"
    );
}

/// KNOWN GAP: glibc, not the `.so`, supplies some of the `.so`'s own definitions.
///
/// WHY EXPECTED: `csrc/sim_deterministic_mem.c` gives every `.so` fixed-branch
/// `memcpy` / `memset` / ... and arena-backed `calloc` / `free`, to keep glibc's
/// alignment- and heap-dependent branches out of the PMU RBC count. They have
/// default visibility and the `.so` calls several through its PLT, so glibc,
/// earlier in the global scope, takes those bindings: `LD_DEBUG=bindings` shows
/// `libscx_simple.so` binding `calloc`, `free`, `memcpy` and `memset` to libc
/// (sim-o3kct).
///
/// WHEN THIS GOES RED: the gap has closed. Invert the assertion -- assert that
/// no bundled `.so` definition binds to another object -- and keep the test.
///
/// DO NOT: delete it (silently drops the coverage), or exclude symbols from the
/// check to make it pass (destroys the property that made it worth having).
#[test]
fn known_gap_libc_takes_the_so_memory_functions() {
    let this_binary = this_binary();
    let sos = bundled_sos();
    let mut taken: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for so in &sos {
        for (name, reference) in &so.references {
            if *reference == (SoReference::Defined { interposable: true })
                && global_binding(name).is_some_and(|object| object != this_binary)
            {
                taken.entry(name).or_default().push(so.name);
            }
        }
    }
    assert!(
        !taken.is_empty(),
        "no bundled .so definition binds to another loaded object any more \
         (sim-o3kct). KNOWN-GAP TEST: this going red means the gap CLOSED. Invert \
         this assertion to assert the property now holds. Do not delete it, and do \
         not loosen the bound."
    );
}
