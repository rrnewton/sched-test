# scx_simulator on crates.io: release candidate, 2026-09-24

**Status: candidate, not published.** Nothing described here has been uploaded
to crates.io. The candidate is the branch
`feat/scxsim-cratesio-release-candidate` on
<https://github.com/rrnewton/sched-test>. It is this document's commit plus
one change: `publish = false` removed from the six crates under "What gets
published". Whether and when to publish is the owner's decision. The
decisions that come first are listed under "Decisions before the first
publish".

Every number below comes from a row of the evidence table in the dev
harness, cited as "row N":
<https://github.com/rrnewton/dev-sched-test/tree/main/experiments/scxsim_cratesio_rc_20260924>.
Each row names the commit it measured. Rows 1–3, 13, 14, 16, 17 and 22–24
measured the packages that `cargo package` built at bf66f841. Rows 4–11
measured the earlier f1643749 packages; section 8 says how the two differ. No
file under `scx-sim/crates/` has changed since bf66f841
(`git diff --stat bf66f841.. -- scx-sim/crates` prints nothing), so the
candidate packages the sources the final gate measured.

The reference for consumers is `ktstr_scxsim_embed_contract.md`, next to this
file. This document summarises the release and does not repeat the contract.

## 1. The host-export trap, and the probe that closes it

**Publishing without the load-time probe would have exposed external users to
silent stub-binding. The probe turns it into one actionable error that names
the fix.**

A scheduler `.so` calls back into the simulator through 55 host symbols,
listed in `scxsim_build::HOST_EXPORTS`. The `.so` finds them only if the
binary that loads it exports them, and a Rust binary does not export them by
default. What a missing export does depends on the name, and each
`HOST_EXPORTS` entry records it (`IfUnexported`):

| class | names | what happens when the binary does not export the name |
|---|---|---|
| `DlopenFails` | 11 | `dlopen` fails, which is visible |
| `OwnDefinitionBinds` | 24 | the `.so`'s own copy runs instead; for most of these names it is a stub (sim-3geoh) |
| `ResolvesToNull` | 20 | a weak reference resolves to NULL and the scheduler takes its fallback path |

For 44 of the 55 names the load reports nothing. The run may then crash, or
it may finish with a different simulation and no sign that anything is wrong.
Which one happens depends on the scheduler and the name. These measurements
used a measurement copy of the crate whose probe can be switched off at run
time (the method is described above the evidence table):

- **Leave one out** (row 4). Each of the 55 names was withheld in turn, from
  each of 6 schedulers: 330 runs. With the probe off:
  - 259 ran normally with the baseline result;
  - 18 ran normally with a different result, and nothing reported it;
  - 53 failed visibly: 30 `dlopen` failures, 14 signals, 7 panics and 2
    abnormal exits.

  With the probe on, all 330 were refused, each naming exactly the withheld
  symbol.
- **Chasing errors until everything runs** (rows 5–8). This models a user who
  adds an export only when something fails.
  - Round r2-0 (90 names exported): five of the six schedulers are killed by
    SIGSEGV. The two examined under gdb, simple and tickless, call address 0.
    In compat.bpf.h, `scx_bpf_dsq_insert` and `scx_bpf_dsq_insert_vtime` fall
    back to weak `___compat` names, and those resolved to NULL (row 7).
  - Round r2-1 (92 names, those two added): all six run to completion without
    an error, but only simple is right. tickless is missing 33.4% of its
    events, cosmos 26.9%, layered 10.6% and mitosis 5.2%. lavd records 367
    events instead of 16277 and schedules no task at all (row 8).

  Fixing every visible error leads a user to r2-1, and r2-1 is silently
  wrong. The probe refuses both rounds, naming what is missing (33 names in
  the largest r2-1 refusal).
- **Final gate** (rows 1–3, bf66f841):
  - With no link arguments at all (N-none), all six loads were refused.
  - With a selective export list that leaves out four weak names (L-weak4),
    all six were refused, each naming exactly those four: `scx_task_alloc`,
    `scx_task_data`, `scx_task_free` and `scx_atq_create_internal`.
  - With the full list (L-all), all six ran at baseline.

The refusal is `LoadError::HostSymbolsNotExported { path, missing }`.
`DynamicScheduler::try_load_with_definition` returns it before `dlopen`. The
other three loaders (`load`, `try_load`, `load_with_definition`) go through
it. The message names every missing symbol and the fix: call
`scxsim_build::emit_host_link_args()` from the build script of the package
that builds the binary. Row 15 shows the refusal as an embedder sees it.

`-rdynamic` on its own also works today. It exports every global symbol
linked into the binary, and today's link happens to pull in all 55 (final
gate N-rdynamic; row 9, where bare `-rdynamic` matched at three build
profiles). Nothing guarantees that, so `emit_host_link_args()` adds one
`--undefined` per name and the probe checks the result. The contract on
`integration` before this release asked consumers to re-emit 13 names
(`EXPORTED_SYMS`) by hand. The other 42 of today's 55 were missing, including
all 20 `ResolvesToNull` (embed contract, "Read this first").

What the probe does not catch, all open:

| issue | P | gap |
|---|---|---|
| sim-4b77c | 1 | it checks the bundled schedulers' names only; a `.so` an embedder builds can still bind its own fallbacks silently |
| sim-c7ukt | 2 | it checks that a name is present, not that it is the simulator's; a same-named export elsewhere in the process takes the binding |
| sim-o3kct | 1 | glibc takes the `.so`'s own `calloc`/`free`/`memcpy`/`memset`/`strncmp` bindings, bypassing `sim_deterministic_mem.c` |
| sim-kuhrw | 1 | `scxtest/overrides.c` gives every `.so` weak `scx_minheap_*` bodies that report success, which hides an elided `lib/minheap.bpf.c` |

## 2. The test: ktstr calls the simulator in-process

The task's question: can ktstr add a normal Cargo dependency, call the
simulator in-process, and delete the export bridge? **Yes, against the
packages as built.** They are not on crates.io yet, so ktstr resolves them
through a `[patch.crates-io]` table (row 23). Two commits on branch
`feat/ktstr-scxsim-typed-source` of <https://github.com/rrnewton/ktstr> do
it:

1. `72e5f1f7` "export: build the record as scxsim-workload-ir's
   SourceScenario, not json!". The exported record is now the simulator's
   own input type, `scxsim_workload_ir::SourceScenario`. The 25
   `serde_json::json!` sites in `src/scenario/export.rs` are gone; the one
   remaining mention is a doc comment recording the change. This half needs
   only `scxsim-workload-ir` with default features. Its dependency graph has
   no simulator, no scxsim-build, no clang and no libbpf (final gate, DSL
   cell).
2. `a6abb6c7` "scxsim: opt-in feature that runs an exported scenario on the
   simulator in-process". Under the `scxsim` feature,
   `tests/scxsim_in_process.rs` lowers a scenario with
   `scxsim_workload_ir::lower` and `to_scenario` and runs it with
   `scx_simulator` inside the test binary. There is no file and no simulator
   binary in between. `build.rs` builds the `simple` scheduler `.so` and passes
   `scxsim_build::host_link_args()` to the test targets with
   `cargo:rustc-link-arg-tests`.

The test cannot pass without the exports: with the link-argument loop removed
from `build.rs` it exits 100, refused by the probe, and with the loop
restored it exits 0 (row 23). ktstr's lint is green on every leg, and all 116
test binaries link with `--features scxsim` (row 24).

With these commits, running a scenario on the simulator no longer needs the
exported file, a separate simulator binary or `ktstr-scenario-replay`. The
commits keep the file export itself; removing it is a ktstr change for when
ktstr is ready. ktstr's dependency lines name `scx_simulator` 1.0.0 and
today's crate names. If the owner chooses 0.1.0 or renames crates (section
7), those lines change too.

## 3. What gets published

Six crates. `cargo publish` requires every normal and build dependency to be
on crates.io, so the set is the in-process consumer's dependency closure.

| crate | version | publish | why |
|---|---|---|---|
| scx_simulator | 1.0.0 (0.1.0 recommended, section 7) | yes | the simulator |
| scxsim-workload-ir | 0.1.0 | yes | the typed scenario record and its lowering; the `ingest` feature adds `to_scenario` and the optional scx_simulator dependency |
| scxsim-build | 0.1.0 | yes | builds scheduler `.so` files and supplies `emit_host_link_args()`; scx_simulator depends on it (normal and build), and so does the build script of every embedder that loads a scheduler |
| scx_perf | 0.1.0 | yes | a dependency of scx_simulator |
| scx_layered_growth | 0.1.0 | yes | a dependency of scx_simulator |
| scx_layered_alloc | 0.1.0 | yes | a dependency of scx_simulator; not one of the nine crates the task listed |
| ktstr-scenario-replay | | no | reads ktstr's exported files, which is the bridge that in-process use replaces |
| scxsim-calibration | | no | a tool built on the simulator; nothing depends on it |
| scx_cgroup_tree | | no | nothing in the published closure depends on it |
| embed_harness | | no | this workspace's embedding test harness |
| embed_unexported | | no | the probe's negative-control test crate |

Publish order, dependencies first:

1. scxsim-build;
2. scx_perf, scx_layered_alloc and scx_layered_growth;
3. scx_simulator;
4. scxsim-workload-ir.

## 4. What a consumer has to do

The embed contract has the details. In short:

- **Export the host symbols.** Call `scxsim_build::emit_host_link_args()` from
  the build script of the package that builds the binary. Contract §2 covers
  scoping it to some targets only.
- **Rust 1.88 or later.** The six manifests declare `rust-version = "1.88"`.
  Cargo refuses 1.87.0, and 1.87.0 would fail anyway on two let chains in the
  vendored scx_layered sources (row 11; final gate cells F187 and F188).
  Nothing gates the declared version yet (sim-ojycs).
- **clang**, to build scheduler `.so` files. A consumer of the record alone
  needs neither clang nor libbpf (DSL cell).
- **One simulation per process at a time.** Hold `scx_simulator::SIM_LOCK`
  around every load and run. Without it, concurrent runs crash the process
  or return wrong results without an error (row 13). Nothing in the library
  enforces this (sim-0qpv9).
- `Simulator::run` prints an end-of-run report to stdout on every run
  (sim-yrtox). scx_simulator turns libbpf-sys's vendored and static features
  on for every embedder (sim-dneoq).

## 5. Known defects

The issues sim-rzyx9 lists as blocking the first publish:

| issue | P | defect | evidence |
|---|---|---|---|
| sim-4xdj4 | 1 | the safe API is unsound: from a `#![forbid(unsafe_code)]` file, five calls abort (heap corruption) or segfault | row 14 |
| sim-0qpv9 | 1 | concurrent simulations corrupt each other; `SIM_LOCK` is a caller contract | row 13 |
| sim-3geoh | 1 | `.so` files carry their own bodies for 24 of the 55 host names | section 1 |
| sim-kuhrw | 1 | weak `scx_minheap_*` bodies hide an elided `lib/minheap.bpf.c` | |
| sim-6vhbq | 1 | sim_atq's taskc field offsets went stale with the scx pin bump (56/64, now 48/56) | |
| sim-4b77c | 1 | the probe covers only the bundled schedulers' symbols | |
| sim-ld1ro | 1 | a definition's source patches are silently dropped for tickless, lavd and layered | row 19 |
| sim-cp5c0 | 2 | `KernelConfig::hz` changes HZ for scx_tickless alone | |
| sim-yrtox | 2 | `Simulator::run` prints to stdout | |
| sim-dneoq | 2 | libbpf-sys features forced on every embedder | |
| sim-jnh26 | 2 | `IngestError` and `LoweringError` are not `#[non_exhaustive]` | |
| sim-fmijm | 2 | the `YieldNotRepresentable` refusal is obsolete and its message is false | |
| sim-kqri1 | 2 | licensing (section 6) | row 22 |

The first seven are unsound or silently wrong. The next five are API changes
that would break callers if made after publishing. The same holds for the
crate root, which re-exports 162 names besides its 25-name prelude, among
them upstream scx_layered items such as `unified_alloc` and `LayerDemand`
(sim-rzyx9).

Other open findings:

| issue | P | defect | evidence |
|---|---|---|---|
| sim-g4kix | 1 | the engine never consumes a running task's slice: tickless counts 128 preemptions, the simulator 0, and the run exits `Normal` | row 16 |
| sim-o3kct | 1 | glibc binds the `.so`'s own allocator and string functions | |
| sim-c7eca | 2 | `libscx_lavd.so` has an `RWE` LOAD segment, so a process under MDWE cannot load it | row 17 |
| sim-cu830 | 2 | `cargo doc` with `-D warnings` fails with 20 rustdoc errors, and no gate runs it | row 18 |
| sim-c7ukt | 2 | the probe checks presence, not identity | |
| sim-65zbt | 2 | rodata width is not checked against the symbol's size | |
| sim-mlv09 | 2 | some kfuncs have two definitions per process | |
| sim-6gqss | 2 | `sim_atq.c` hand-ports upstream `lib/atq` with no differential test | |
| sim-u731e | 2 | the simulated CSS iterator drops cgroups beyond 2048 | |
| sim-ojycs | 2 | `rust-version = 1.88` was measured once and is not gated | row 11 |
| sim-ybrh9 | 2 | the standalone scheduler `.so` build is not gated behind its feature | |
| sim-004y6 | 2 | a sound vmlinux override, to replace the one removed below | row 20 |
| sim-yc6z3 | 3 | repository-local references in the packages (section 6) | row 22 |
| sim-dab4w | 3 | the build script emits 6 C warnings | |
| sim-5z1nt | 3 | `extra_local_include` looks vestigial | |

Removed on this branch: scxsim-build's per-`.so` vmlinux override (ccc1d8eb).
It compiled one scheduler against a header that need not match the rest of
the build. The host header differs from the vendored one in 15 of 17 layout
quantities, and the build failed. The arm64 header differs in 7 of 17; with
it lavd, mitosis and cosmos exit 139 and simple exits 0 (row 20).

## 6. What goes public

These are listed so the owner can decide before publishing. They are
described here, not quoted.

- **Package contents.** Row 22 lists every packaged file, with an SPDX
  inventory.
- **Repository-local references.** Comments and docs carry task IDs, `sim-`
  issue IDs and `experiments/` paths. CLI help text assumes the repository.
  There are four copies of the vendored scx `lib/` tree (sim-yc6z3). None of
  it is secret, but outside the repository none of it resolves.
- **Two comments name internal infrastructure**, one in `scxtest/kern_types.h`
  and one in `schedulers/lavd/wrapper.c`. Neither is secret. Each needs a yes
  or no (sim-rzyx9).
- **Licensing** (sim-kqri1). scx_simulator's license field lacks the
  `(LGPL-2.1-only OR BSD-2-Clause)` term, and `scxtest/` is GPL-2.0-derived
  code without a marking. The vendored Reverie license carries its copyright
  notice because BSD-2-Clause requires it; that notice stays.
- **Names.** None of the six names is taken: all 12 spellings return 404
  from the crates.io index (row 12). 32 published crates already use the
  `scx_` or `scx-` prefix (sim-rzyx9).

## 7. Decisions before the first publish

From sim-rzyx9, with recommendations:

1. **Names.** Rename scx_perf, scx_layered_alloc and scx_layered_growth with
   a `scxsim-` prefix. Settle `scx_simulator` with the sched-ext maintainers,
   whose projects use the `scx_` prefix. A `[lib] name` keeps the Rust paths
   unchanged.
2. **Version.** scx_simulator has been 1.0.0 since sched-test#18. Recommend
   0.1.0: five open fixes change the API (sim-4xdj4, sim-cp5c0, sim-0qpv9,
   sim-yrtox, sim-dneoq), and under 1.0.0 each would force a 2.0.0.
3. **Public surface.** Trim the crate root's re-exports before publishing
   (section 5).
4. **Licensing.** The license expression includes GPL-2.0-only. ktstr is
   GPL-2.0-only, so the two are compatible. Settle sim-kqri1.
5. **What blocks the first publish.** The thirteen issues in section 5's
   first table and the crate root, plus a yes or no on the two comments in
   section 6.

## 8. How the candidate was checked

- **Packaging** (`captures/packaging/`): `cargo package` on the six crates at
  f1643749 and again at bf66f841. The two packagings differ in 21 files, none
  under `vendor/`: `rust-version = "1.88"`, the vmlinux override's removal,
  scxsim-workload-ir's `scxsim-build` build dependency made optional behind
  `ingest`, the refusal's wording, and comments and documentation.
  `HOST_EXPORTS` is identical in both.
- **Consumer gate** (rows 1–3): a ktstr-shaped scratch consumer depends on
  the six packages as registry versions. The gate has 54 rows and 0 failures.
  At rustc 1.97.1 and 1.94.1 with the consumer as committed, and at 1.88.0
  with a fresh lockfile, all six schedulers run in-process and match the
  baseline. The other rows are the link-mode cells in section 1. The 1.87.0
  refusal and the DSL-only build are checked alongside.
- **ktstr** (rows 23–24), as in section 2.
- **Not done:** `cargo publish`. Every run was on one x86_64 Linux host,
  including row 20's arm64 figures, which used the arm64 header there.
