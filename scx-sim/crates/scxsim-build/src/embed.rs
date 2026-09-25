//! The embedding half of the build contract: what a build script needs beyond
//! compiling schedulers. [`emit_host_link_args`] makes the host binary able to
//! load a scheduler `.so`; [`resolve_scx_root`] and [`bundled_scx_root`] find the
//! scx sources, and [`emit_upstream_module`] compiles upstream Rust out of them;
//! and [`SimBuildInputs`] carries `scx_simulator`'s own build inputs
//! to a dependent's build script through cargo's `links` metadata.

use std::path::{Path, PathBuf};

use crate::{
    build_schedulers, cgroup_bw_new_api, scx_include_paths, KernelConfig, SchedulerDefinition,
    HOST_EXPORTS,
};

/// The linker arguments that put [`HOST_EXPORTS`] in a binary's dynamic symbol
/// table: `-rdynamic`, then `-Wl,--undefined=<sym>` for each symbol (so the
/// link keeps the definitions even though nothing in the binary calls them).
pub fn host_link_args() -> impl Iterator<Item = String> {
    std::iter::once("-rdynamic".to_owned()).chain(
        HOST_EXPORTS
            .iter()
            .map(|sym| format!("-Wl,--undefined={}", sym.name)),
    )
}

/// Emit [`host_link_args`] as `cargo:rustc-link-arg` directives, which apply to
/// every binary, test, example and bench of the calling package.
///
/// Call this from the build script of EVERY package whose binaries load a
/// scheduler `.so`, including packages that only reach scx_simulator through
/// another crate. Cargo does not propagate link arguments to dependents (the
/// transitive `link-arg` library kind is nightly-only), so scx_simulator's own
/// build script cannot do this for you. Without it scx_simulator refuses to load
/// a scheduler, returning `LoadError::HostSymbolsNotExported` rather than letting
/// the `.so` fail `dlopen` on an undefined symbol or -- worse, because it loads
/// and runs -- bind its own NULL/0-returning fallbacks in place of the
/// simulator's definitions (see [`IfUnexported::OwnDefinitionBinds`]) or bind
/// its weak kfunc references to NULL (see [`IfUnexported::ResolvesToNull`]).
///
/// [`IfUnexported::OwnDefinitionBinds`]: crate::IfUnexported::OwnDefinitionBinds
/// [`IfUnexported::ResolvesToNull`]: crate::IfUnexported::ResolvesToNull
pub fn emit_host_link_args() {
    for arg in host_link_args() {
        println!("cargo:rustc-link-arg={arg}");
    }
}

/// Resolve the scx source root for a scheduler `.so` build. Returns the
/// `SCX_ROOT` env override if set -- canonicalized and asserted to look like an
/// scx checkout (must contain `scheds/include`), failing loud otherwise -- else
/// `default()`, which is evaluated only when `SCX_ROOT` is unset (so a default
/// that panics on a missing tree, like [`bundled_scx_root`], never fires for an
/// overridden build). Emits `cargo:rerun-if-env-changed=SCX_ROOT`, so call it
/// only from a build script. Shared by every build script that compiles scx
/// sources so the override and the validity check are one source of truth.
pub fn resolve_scx_root(default: impl FnOnce() -> PathBuf) -> PathBuf {
    println!("cargo:rerun-if-env-changed=SCX_ROOT");
    scx_root_or(std::env::var_os("SCX_ROOT").map(PathBuf::from), default)
}

/// [`resolve_scx_root`] with the `SCX_ROOT` value passed in (unit-testable
/// without mutating the process environment).
fn scx_root_or(scx_root_env: Option<PathBuf>, default: impl FnOnce() -> PathBuf) -> PathBuf {
    let Some(v) = scx_root_env else {
        return default();
    };
    let canon = v
        .canonicalize()
        .unwrap_or_else(|e| panic!("SCX_ROOT={} is not accessible: {e}", v.display()));
    assert!(
        canon.join("scheds/include").is_dir(),
        "SCX_ROOT={} does not look like an scx checkout (missing scheds/include)",
        v.display()
    );
    canon
}

/// The scx source root a crate carries INSIDE itself, under `vendor_scx`
/// (conventionally `<crate>/vendor/scx`): the subset of scx the crate compiles,
/// laid out exactly as in scx. A published `.crate` cannot reach outside its own
/// directory, so this is what makes the crate buildable from a registry.
///
/// In a sched-test checkout the subset is committed as relative symlinks into
/// the `scx` submodule; `cargo package` materializes them into real files. One
/// build script therefore serves both layouts: `anchor` (a vendored path,
/// relative to the scx root, e.g. `scheds/include`) is canonicalized and its
/// components are stripped off again. In a checkout that yields the physical
/// submodule root, so compile paths -- and the `.so` bytes -- are identical to
/// building straight from the submodule; in a packaged crate it yields
/// `vendor_scx` itself.
///
/// Panics, naming the fix, if the anchor does not resolve (typically an
/// uninitialized submodule) or resolves somewhere that does not end in `anchor`.
/// Pass it to [`resolve_scx_root`] as the lazy default so an `SCX_ROOT` override
/// never reaches these checks.
pub fn bundled_scx_root(vendor_scx: &Path, anchor: &Path) -> PathBuf {
    let vendored = vendor_scx.join(anchor);
    let physical = vendored.canonicalize().unwrap_or_else(|e| {
        panic!(
            "bundled scx source {} does not resolve: {e}. In a sched-test checkout it is \
             a symlink into the scx submodule: run `git submodule update --init scx`, or \
             set SCX_ROOT to an scx checkout.",
            vendored.display()
        )
    });
    assert!(
        physical.ends_with(anchor),
        "bundled scx source {} resolves to {}, which does not end in {}: the vendored \
         tree no longer mirrors the scx layout",
        vendored.display(),
        physical.display(),
        anchor.display()
    );
    let mut root = physical;
    for _ in anchor.components() {
        root.pop();
    }
    root
}

/// Compile one upstream scx Rust source verbatim as a module of the calling
/// crate: writes `OUT_DIR/<module>_mod.rs` holding
/// `#[path = "<scx_root>/<rel>"] pub mod <module>;`, which the crate pulls in
/// with `include!(concat!(env!("OUT_DIR"), "/<module>_mod.rs"))`, and emits
/// `cargo:rerun-if-changed` for the source. Call it only from a build script.
///
/// Why generated: a `#[path]` attribute takes a string LITERAL, so written in
/// source it is nailed to the bundled submodule and blind to `SCX_ROOT` -- an
/// embedder pointing `SCX_ROOT` at their own scx tree got the scheduler `.so`
/// from their tree and the upstream Rust from ours, in one binary, silently.
/// `include!`ing the file straight into an inline `mod` is not an option
/// either: upstream files open with `//!` module docs, and inner doc comments
/// are illegal in a macro expansion (E0753). The one-line wrapper keeps the
/// upstream file an ordinary file module, byte-identical to the pin.
///
/// The file is parsed under the CALLING crate's edition, so that crate must be
/// on the upstream crate's edition (scx_layered: 2024).
pub fn emit_upstream_module(scx_root: &Path, rel: &str, module: &str) {
    let src = scx_root.join(rel);
    assert!(
        src.is_file(),
        "{rel} not found at {} (is SCX_ROOT an scx checkout?)",
        src.display()
    );
    let out_dir =
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR unset: not a build script"));
    std::fs::write(
        out_dir.join(format!("{module}_mod.rs")),
        format!("#[path = {src:?}]\npub mod {module};\n"),
    )
    .unwrap_or_else(|e| panic!("write {module}_mod.rs: {e}"));
    println!("cargo:rerun-if-changed={}", src.display());
}

/// [`emit_upstream_module`] for a crate that bundles `rel` under its own
/// `vendor/scx` (see [`bundled_scx_root`], anchored at `rel` itself): the whole
/// build script of a crate that compiles one upstream scx Rust file. An
/// `SCX_ROOT` override still wins, via [`resolve_scx_root`].
pub fn emit_bundled_upstream_module(rel: &str, module: &str) {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: not a build script"),
    );
    let scx_root =
        resolve_scx_root(|| bundled_scx_root(&manifest_dir.join("vendor/scx"), Path::new(rel)));
    emit_upstream_module(&scx_root, rel, module);
}

/// The build inputs `scx_simulator`'s build script publishes, through its
/// `links = "scxsim"` key, to the build script of every package that depends on
/// it directly (cargo sets them there as `DEP_SCXSIM_<KEY>`). With them an
/// embedder compiles its own scheduler `.so` via [`build_schedulers`] against
/// exactly the substrate `scx_simulator` was built with -- the same C sources,
/// the same scx tree (an `SCX_ROOT` override included), the same libbpf headers
/// -- wherever cargo unpacked `scx_simulator`, and with no libbpf-sys dependency
/// of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimBuildInputs {
    /// `scx_simulator`'s `csrc/`: the sim C translation units every `.so` links.
    pub csrc: PathBuf,
    /// `scx_simulator`'s `scxtest/`: map/cpumask emulation and `overrides.c`.
    pub scxtest: PathBuf,
    /// The scx source root `scx_simulator` compiled against.
    pub scx_root: PathBuf,
    /// libbpf's header directory (libbpf-sys's `DEP_BPF_INCLUDE`).
    pub bpf_include: PathBuf,
    /// `scx_simulator`'s bundled scheduler sources, one `<name>/wrapper.c` each.
    pub schedulers: PathBuf,
}

impl SimBuildInputs {
    /// The `links` name `scx_simulator` declares; cargo prefixes each key with it.
    pub const LINKS: &'static str = "scxsim";

    /// Every field as `(metadata key, path)`: the one list both directions use.
    fn entries(&self) -> [(&'static str, &Path); 5] {
        [
            ("csrc", &self.csrc),
            ("scxtest", &self.scxtest),
            ("scx_root", &self.scx_root),
            ("bpf_include", &self.bpf_include),
            ("schedulers", &self.schedulers),
        ]
    }

    /// The env var cargo sets for metadata `key` in a dependent's build script.
    fn dep_var(key: &str) -> String {
        format!(
            "DEP_{}_{}",
            Self::LINKS.to_ascii_uppercase(),
            key.to_ascii_uppercase()
        )
    }

    /// Publish these inputs as `links` metadata. Called by `scx_simulator`'s
    /// build script; an embedder calls [`from_dep_env`](Self::from_dep_env).
    /// Panics on a non-UTF-8 path: build-script output is text, and a lossily
    /// printed path would reach the embedder as a different, wrong one.
    pub fn emit_metadata(&self) {
        for (key, path) in self.entries() {
            let path = path.to_str().unwrap_or_else(|| {
                panic!("scx_simulator build input {key} is not UTF-8: {path:?}")
            });
            println!("cargo:{key}={path}");
        }
    }

    /// Read what `scx_simulator` published, from the calling build script's
    /// environment. Panics, naming the fix, if a variable is missing: cargo sets
    /// them only for a package with `scx_simulator` in `[dependencies]` -- not in
    /// `[dev-dependencies]` or `[build-dependencies]`.
    pub fn from_dep_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name).map(PathBuf::from))
    }

    fn from_vars(get: impl Fn(&str) -> Option<PathBuf>) -> Self {
        let var = |key: &str| {
            let name = Self::dep_var(key);
            get(&name).unwrap_or_else(|| {
                panic!(
                    "{name} is not set. Cargo passes scx_simulator's build inputs only to \
                     the build script of a package that lists scx_simulator under \
                     [dependencies] (not dev- or build-dependencies)."
                )
            })
        };
        Self {
            csrc: var("csrc"),
            scxtest: var("scxtest"),
            scx_root: var("scx_root"),
            bpf_include: var("bpf_include"),
            schedulers: var("schedulers"),
        }
    }

    /// The `-I` set for [`build_schedulers`] (and the host static libs): the sim
    /// C dirs, then [`scx_include_paths`]. `-I` resolution is first-match, so this
    /// order is part of the build contract. Scheduler `.so` files must be built
    /// with exactly this set: the host static libs are, and the two share struct
    /// layouts through `vmlinux.h` (see [`scx_include_paths`]).
    pub fn include_paths(&self) -> Vec<PathBuf> {
        [self.csrc.clone(), self.scxtest.clone()]
            .into_iter()
            .chain(scx_include_paths(&self.scx_root, &self.bpf_include))
            .collect()
    }

    /// What a scheduler build over `defs` reads from these inputs, as paths to
    /// watch with `cargo:rerun-if-changed`: the sim C dirs, the scx trees every
    /// scheduler includes, and each definition's own scx BPF subtree. Generated
    /// from `defs`, so a new scheduler's subtree is covered with no edit here --
    /// a missing entry is how a scx SHA swap used to leave a stale `.so` behind.
    pub fn rerun_paths<'a>(
        &'a self,
        defs: &'a [SchedulerDefinition],
    ) -> impl Iterator<Item = PathBuf> + 'a {
        [
            self.csrc.clone(),
            self.scxtest.clone(),
            self.scx_root.join("lib"), // ravg.bpf.c, cgroup_bw.bpf.c, ...
            self.scx_root.join("scheds/include"),
            self.scx_root.join("scheds/vmlinux"),
        ]
        .into_iter()
        .chain(defs.iter().filter(|d| d.scx_bpf_dir).map(|d| {
            self.scx_root
                .join(format!("scheds/rust/scx_{}/src/bpf", d.name))
        }))
    }

    /// Compile `defs` -- each naming one of `scx_simulator`'s bundled schedulers
    /// (a `<name>/wrapper.c` under [`schedulers`](Self::schedulers)) -- into
    /// `<out_dir>/schedulers/libscx_<name>.so`, and return that directory. Call
    /// it from a build script with `out_dir` = `OUT_DIR`; with
    /// [`emit_host_link_args`] it is the whole build-side embedder contract.
    ///
    /// The compiler is `$BPF_CLANG`, else `clang`; the `-I` set is
    /// [`include_paths`](Self::include_paths), and `kernel_config` is passed
    /// through (see [`KernelConfig`]). Emits the rerun triggers for everything it
    /// reads.
    pub fn build_bundled(
        &self,
        defs: &[SchedulerDefinition],
        out_dir: &Path,
        kernel_config: &KernelConfig,
    ) -> PathBuf {
        let staged = out_dir.join("schedulers_src");
        self.stage_bundled(defs, &staged);
        let so_dir = out_dir.join("schedulers");
        std::fs::create_dir_all(&so_dir)
            .unwrap_or_else(|e| panic!("create {}: {e}", so_dir.display()));
        println!("cargo:rerun-if-env-changed=BPF_CLANG");
        let compiler = std::env::var("BPF_CLANG").unwrap_or_else(|_| "clang".into());
        build_schedulers(
            &staged,
            defs,
            &so_dir,
            &self.csrc,
            &self.scxtest,
            &self.include_paths(),
            &self.scx_root,
            &compiler,
            false, // coverage
            cgroup_bw_new_api(&self.scx_root),
            kernel_config,
        );
        let sources = defs.iter().map(|d| self.schedulers.join(&d.name));
        for path in self.rerun_paths(defs).chain(sources) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        so_dir
    }

    /// Stage the `defs` subset of [`schedulers`](Self::schedulers) in `dir`, one
    /// symlink per scheduler (a link, not a copy, so nothing can drift).
    /// [`build_schedulers`] compiles every `wrapper.c` subdir it is given, so
    /// links left by an earlier build with other `defs` are removed first -- and
    /// anything in `dir` that is not a link is refused rather than deleted.
    fn stage_bundled(&self, defs: &[SchedulerDefinition], dir: &Path) {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
                .path();
            assert!(
                path.is_symlink(),
                "{} is not a staged scheduler link; refusing to remove it",
                path.display()
            );
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("clear stale {}: {e}", path.display()));
        }
        for def in defs {
            let src = self.schedulers.join(&def.name);
            assert!(
                src.join("wrapper.c").is_file(),
                "{} is not a bundled scheduler: {} has no wrapper.c",
                def.name,
                src.display()
            );
            std::os::unix::fs::symlink(&src, dir.join(&def.name))
                .unwrap_or_else(|e| panic!("stage scheduler {}: {e}", def.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-rdynamic` first, then exactly one `--undefined` per host export, in
    /// `HOST_EXPORTS` order: the whole contract a loading binary must satisfy.
    #[test]
    fn host_link_args_are_rdynamic_then_one_undefined_per_host_export() {
        let args: Vec<String> = host_link_args().collect();
        assert_eq!(args.len(), 1 + HOST_EXPORTS.len());
        assert_eq!(args[0], "-rdynamic");
        for (arg, sym) in args[1..].iter().zip(HOST_EXPORTS) {
            assert_eq!(*arg, format!("-Wl,--undefined={}", sym.name));
        }
    }

    /// Each name appears once: a duplicate would make the list's two halves
    /// disagree about what an unexported symbol does.
    #[test]
    fn host_exports_name_each_symbol_once() {
        let mut names: Vec<&str> = HOST_EXPORTS.iter().map(|e| e.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate name in HOST_EXPORTS");
    }

    /// A per-test scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("scxsim-build-test-{}-{name}", std::process::id()));
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("clear stale {}: {e}", dir.display()),
            }
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let dir = self.0.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn symlink(&self, rel: &str, target: &str) {
            let link = self.0.join(rel);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, link).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// In a checkout the vendored anchor is a relative symlink into the
    /// submodule, and the root must come back as the PHYSICAL submodule root --
    /// that is what keeps in-repo compile paths (and so the `.so` bytes)
    /// unchanged. Covers a directory anchor and a multi-component file anchor.
    #[test]
    fn bundled_scx_root_resolves_a_checkout_symlink_to_the_submodule() {
        let t = Scratch::new("checkout");
        let scx = t.mkdir("scx");
        t.mkdir("scx/scheds/include");
        t.mkdir("scx/scheds/rust/scx_layered/src");
        std::fs::write(scx.join("scheds/rust/scx_layered/src/growth.rs"), "").unwrap();
        t.symlink(
            "crate/vendor/scx/scheds/include",
            "../../../../scx/scheds/include",
        );
        t.symlink(
            "crate/vendor/scx/scheds/rust/scx_layered/src/growth.rs",
            "../../../../../../../scx/scheds/rust/scx_layered/src/growth.rs",
        );
        let vendor = t.0.join("crate/vendor/scx");
        assert_eq!(bundled_scx_root(&vendor, Path::new("scheds/include")), scx);
        assert_eq!(
            bundled_scx_root(&vendor, Path::new("scheds/rust/scx_layered/src/growth.rs")),
            scx
        );
    }

    /// In a packaged crate the vendored tree is real files, and the root is the
    /// crate's own vendor dir -- nothing outside the crate is consulted.
    #[test]
    fn bundled_scx_root_of_a_packaged_crate_is_its_vendor_dir() {
        let t = Scratch::new("packaged");
        let vendor = t.mkdir("pkg/vendor/scx");
        t.mkdir("pkg/vendor/scx/scheds/include");
        assert_eq!(
            bundled_scx_root(&vendor, Path::new("scheds/include")),
            vendor
        );
    }

    /// An uninitialized submodule leaves the symlink dangling; the panic must
    /// say how to fix it rather than surface later as a missing header.
    #[test]
    #[should_panic(expected = "git submodule update --init scx")]
    fn bundled_scx_root_panics_naming_the_submodule_fix() {
        let t = Scratch::new("dangling");
        t.symlink(
            "crate/vendor/scx/scheds/include",
            "../../../../scx/scheds/include",
        );
        bundled_scx_root(&t.0.join("crate/vendor/scx"), Path::new("scheds/include"));
    }

    /// Stripping the anchor's components is only sound if the resolved path ends
    /// in the anchor; a symlink retargeted anywhere else must fail loud instead
    /// of yielding a wrong root.
    #[test]
    #[should_panic(expected = "no longer mirrors the scx layout")]
    fn bundled_scx_root_rejects_a_tree_that_no_longer_mirrors_scx() {
        let t = Scratch::new("mismatch");
        t.mkdir("elsewhere/headers");
        t.symlink(
            "crate/vendor/scx/scheds/include",
            "../../../../elsewhere/headers",
        );
        bundled_scx_root(&t.0.join("crate/vendor/scx"), Path::new("scheds/include"));
    }

    /// Inputs with fixed sim C and libbpf dirs, rooted at `scx_root` and
    /// `schedulers`.
    fn sample_inputs(scx_root: &Path, schedulers: &Path) -> SimBuildInputs {
        SimBuildInputs {
            csrc: PathBuf::from("/c/csrc"),
            scxtest: PathBuf::from("/c/scxtest"),
            scx_root: scx_root.to_path_buf(),
            bpf_include: PathBuf::from("/bpf"),
            schedulers: schedulers.to_path_buf(),
        }
    }

    /// What `emit_metadata` publishes is exactly what `from_dep_env` reads back,
    /// under cargo's `DEP_<LINKS>_<KEY>` naming -- so the two halves of the
    /// channel cannot drift apart key by key.
    #[test]
    fn sim_build_inputs_round_trip_through_dep_vars() {
        let inputs = sample_inputs(Path::new("/c/vendor/scx"), Path::new("/c/schedulers"));
        let vars: std::collections::HashMap<String, PathBuf> = inputs
            .entries()
            .into_iter()
            .map(|(key, path)| (SimBuildInputs::dep_var(key), path.to_path_buf()))
            .collect();
        assert!(vars.contains_key("DEP_SCXSIM_SCX_ROOT"));
        assert_eq!(SimBuildInputs::from_vars(|n| vars.get(n).cloned()), inputs);
    }

    /// A missing variable panics with the actual fix: the dependency kind.
    #[test]
    #[should_panic(expected = "DEP_SCXSIM_CSRC is not set")]
    fn sim_build_inputs_name_the_missing_var() {
        SimBuildInputs::from_vars(|_| None);
    }

    /// The embedder include set is the sim C dirs ahead of the shared scx set,
    /// in the order the standalone `.so` build uses.
    #[test]
    fn sim_build_inputs_include_paths_put_sim_dirs_first() {
        let inputs = sample_inputs(Path::new("/scx"), Path::new("/c/schedulers"));
        let want: Vec<PathBuf> = [PathBuf::from("/c/csrc"), PathBuf::from("/c/scxtest")]
            .into_iter()
            .chain(scx_include_paths(Path::new("/scx"), Path::new("/bpf")))
            .collect();
        assert_eq!(inputs.include_paths(), want);
    }

    /// With `SCX_ROOT` set, the default is never evaluated -- so a bundled
    /// default that would panic on a missing submodule cannot break an
    /// overridden build.
    #[test]
    fn scx_root_override_skips_the_default() {
        let t = Scratch::new("override");
        let scx = t.mkdir("scx");
        t.mkdir("scx/scheds/include");
        let got = scx_root_or(Some(scx.clone()), || {
            panic!("default evaluated despite SCX_ROOT")
        });
        assert_eq!(got, scx);
        assert_eq!(
            scx_root_or(None, || PathBuf::from("/bundled")),
            PathBuf::from("/bundled")
        );
    }

    /// Bundled scheduler sources for the staging tests, one
    /// `schedulers/<name>/wrapper.c` each; returns the `schedulers` dir.
    fn bundled(t: &Scratch, names: &[&str]) -> PathBuf {
        for name in names {
            let dir = t.mkdir(&format!("schedulers/{name}"));
            std::fs::write(dir.join("wrapper.c"), "").unwrap();
        }
        t.0.join("schedulers")
    }

    /// The names staged in `dir`, sorted.
    fn staged(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Staging links exactly the requested schedulers, each to its bundled
    /// source, and a later build with fewer definitions drops the stale links:
    /// `build_schedulers` compiles every subdir it is given, so a leftover link
    /// would build a scheduler nobody asked for (and panic on its missing
    /// definition).
    #[test]
    fn stage_bundled_links_exactly_the_requested_schedulers() {
        let t = Scratch::new("stage");
        let inputs = sample_inputs(Path::new("/scx"), &bundled(&t, &["lavd", "simple"]));
        let dir = t.0.join("out/schedulers_src");
        let defs = [
            SchedulerDefinition::new("lavd"),
            SchedulerDefinition::new("simple"),
        ];
        inputs.stage_bundled(&defs, &dir);
        assert_eq!(staged(&dir), ["lavd", "simple"]);
        assert_eq!(
            std::fs::read_link(dir.join("lavd")).unwrap(),
            inputs.schedulers.join("lavd")
        );
        inputs.stage_bundled(&defs[1..], &dir);
        assert_eq!(staged(&dir), ["simple"]);
    }

    /// Clearing stale links must never delete real content that happens to sit
    /// in the staging dir.
    #[test]
    #[should_panic(expected = "refusing to remove it")]
    fn stage_bundled_refuses_to_delete_a_real_file() {
        let t = Scratch::new("stage-real");
        let inputs = sample_inputs(Path::new("/scx"), &bundled(&t, &["simple"]));
        let dir = t.mkdir("out/schedulers_src");
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        inputs.stage_bundled(&[SchedulerDefinition::new("simple")], &dir);
    }

    /// A definition naming a scheduler scx_simulator does not bundle fails at
    /// staging, naming it, instead of surfacing later as a missing directory.
    #[test]
    #[should_panic(expected = "rusty is not a bundled scheduler")]
    fn stage_bundled_rejects_an_unbundled_scheduler() {
        let t = Scratch::new("stage-unbundled");
        let inputs = sample_inputs(Path::new("/scx"), &bundled(&t, &["simple"]));
        inputs.stage_bundled(&[SchedulerDefinition::new("rusty")], &t.0.join("out"));
    }

    /// The rerun set covers the shared scx trees plus each definition's own scx
    /// BPF subtree, and no subtree for a definition without one: the watch list
    /// that keeps a scx SHA swap from leaving a stale `.so` behind.
    #[test]
    fn rerun_paths_cover_each_definitions_scx_bpf_dir() {
        let inputs = sample_inputs(Path::new("/scx"), Path::new("/c/schedulers"));
        let defs = [
            SchedulerDefinition::new("lavd"),
            SchedulerDefinition::new("simple").with_scx_bpf_dir(false),
        ];
        let paths: Vec<PathBuf> = inputs.rerun_paths(&defs).collect();
        let shared = [
            "/c/csrc",
            "/c/scxtest",
            "/scx/lib",
            "/scx/scheds/include",
            "/scx/scheds/vmlinux",
        ];
        for dir in shared
            .into_iter()
            .chain(["/scx/scheds/rust/scx_lavd/src/bpf"])
        {
            assert!(paths.contains(&PathBuf::from(dir)), "{dir} is not watched");
        }
        assert_eq!(paths.len(), shared.len() + 1, "unexpected watch: {paths:?}");
    }
}
