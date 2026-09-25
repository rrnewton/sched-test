---
title: Packaged crates carry repo-local references (87 tg refs, sim-IDs, experiments/ paths), repo-bound CLI help, and four copies of the vendored lib/ tree
status: open
priority: 3
issue_type: task
labels:
- cratesio
created_at: 2026-09-25T03:43:12.614424627+00:00
updated_at: 2026-09-25T06:29:50.250659528+00:00
---

# Description

The six crates' own files carry references that only resolve inside this repo or on this machine. The published scxsim binary has help text and one mode that assume the repo layout. Two unit tests read files outside the package, and the vendored scx lib/ tree ships four times. Nothing found is secret. All of it is what an external reader of the published crates would trip over.

Measured on the .crate files packaged from the release-candidate branch (integration 24d864c6), extracted, with vendor/ excluded unless stated:

1. Dangling references.
   - 87 references to tg tasks, naming 21 distinct tasks, all in scx_simulator, across 16 files. The most are in safe/trace.rs (16), safe/perfetto_pb.rs (14), unsafe_impl/ffi.rs (9), schedulers/lavd/wrapper.c (9) and safe/engine.rs (8). tg is machine-local, so no external reader can resolve any of them.
   - 62 sim-IDs, 24 distinct, 12 of them closed: scx_simulator 51, scxsim-workload-ir 6, scxsim-build 2, and one each in scx_perf, scx_layered_alloc and scx_layered_growth. They resolve in scx-sim/.beads on the public mirror, but nothing in the crate says so.
   - 17 experiments/ paths, 11 distinct. Nine exist in the public harness repo (rrnewton/dev-sched-test). experiments/bin_cache/ and experiments/lavd_cpubw_stalls_202604/scxsim_stress_runs/errors/ exist nowhere.
   - 14 ai_docs/ paths, 8 distinct. Seven are under scx-sim/ai_docs/ on the mirror. The eighth, cited in safe/starvation.rs, is only in the harness repo's ai_docs/.

2. The scxsim binary. It has no required-features, so cargo install scx_simulator builds it.
   - scxsim run --help prints three experiments/ paths: the --trace-format help cites the wprof baseline report, and the --scheduler-file help cites experiments/bin_cache/<sha>/ and the bug1 version-matrix README. clap takes both texts from doc comments in bin/scxsim/main.rs.
   - RIP-targeted replay cannot work from a registry build. find_e9tool, find_rbc_trampoline and find_rip_trampoline (unsafe_impl/backend/e9patch.rs) look under the grandparent of env!("CARGO_MANIFEST_DIR"), which for a registry build is $CARGO_HOME/registry/src. e9tool falls back to $PATH. The two trampolines have no fallback. The error then says 'Build with: make -C schedulers e9', but no schedulers/Makefile is packaged. Only the CLI reaches this path (bin/scxsim/main.rs calls create_e9rip_so), and the library path an embedder uses does not.
   - sim-ybrh9 already covers the compile-time SCHEDULER_SO_DIR that the same binary bakes in.

3. Two unit tests in safe/layered_control.rs read files that are not in the package, all relative to env!("CARGO_MANIFEST_DIR"):
   - upstream_growth_cannot_gain_unreviewed_host_sys_access reads ../../../scx/scheds/rust/scx_layered/src/layer_core_growth.rs and ../scx_layered_growth/src/lib.rs.
   - flat_target_formulae_have_not_drifted_upstream reads ../../../scx/scheds/rust/scx_layered/src/main.rs.
   Both panic with 'cannot read ...' when the file is missing. So a distro packager or crater running cargo test on the published crate gets two failures that say nothing about the code. This is from reading the code; the tests were not run from the extracted crate.

4. The vendored lib/ tree ships four times. Upstream's scheds/rust/scx_{cosmos,lavd,layered}/src/bpf/lib are symlinks to the top-level lib/, and cargo package stores each one as a full copy. So scx_simulator carries its 23 lib files four times: 69 extra entries and 987,699 extra bytes uncompressed. The three copies gzip to 265,555 bytes on their own, about 13% of the 2,036,244-byte .crate.

5. Two phrases an external reader may ask about. This is the owner's call, since neither is secret. A comment in scxtest/kern_types.h names an internal kernel build as the source of its CONFIG_NR_CPUS value, and a comment in schedulers/lavd/wrapper.c names an internal class of build hosts. The exact phrases are in the task's notes. A case-insensitive search of own files found only these two. It searched for Meta-internal host, tool, repository and domain names, Meta email domains, diff and task numbers, and facebookexperimental. The term list is in the task's notes, not here, since this issue is public. The search found two other hits, both public: David Vernet's upstream copyright line (dvernet@meta.com) in schedulers/simple/scx_simple.bpf.c, and a link to the public facebookexperimental/hermit repo.

Fix, most valuable first:
- Replace the tg references with a commit or a public doc where the provenance matters, and drop them where it does not.
- Make the two --help texts and the e9 error self-contained.
- Move the two layered_control guards into tests/, which the package's include list already leaves out, so they keep guarding in the repo.
- Stop shipping the three lib/ copies: resolve lib/ through the one vendored copy, and check that the six fingerprints do not move.
- Decide the two phrases in item 5.
Every crate's repository field already points at rrnewton/sched-test, so commit-pinned mirror URLs are the natural replacement for the sim-IDs and the ai_docs/ paths.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
