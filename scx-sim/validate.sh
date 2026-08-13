#!/bin/bash
# validate.sh - Local validation script for scx_simulator workspace
# Run this before committing to ensure code quality.
set -euo pipefail

cd "$(dirname "$0")"

# Track skipped checks so we can warn at the end.
SKIPPED=()
record_skip() { SKIPPED+=("$1"); }

echo "=== Checking dependencies ==="
# A missing dependency must produce an explicit, actionable message here —
# never a silent skip, and never a confusing failure five checks later.
# deps.sh exits non-zero only when a REQUIRED dependency is missing; missing
# OPTIONAL ones are printed and then show up again as SKIPPED at the end.
if ! ./scripts/deps.sh check; then
    echo ""
    echo "ERROR: validate.sh cannot run with required dependencies missing."
    echo "       Fix with: make install-deps    (then re-run ./validate.sh)"
    exit 1
fi

echo ""
echo "=== Checking for merge conflict markers ==="
CONFLICT_FILES=$(grep -rl --include='*.rs' --include='*.py' --include='*.sh' \
    --include='*.c' --include='*.h' --include='*.toml' --include='Makefile' \
    -E '^(<{7}|={7}|>{7})' . \
    --exclude-dir=.venv --exclude-dir=target --exclude-dir=.git \
    --exclude-dir=third_party 2>/dev/null || true)
if [ -n "$CONFLICT_FILES" ]; then
    echo "ERROR: Merge conflict markers found in:"
    echo "$CONFLICT_FILES"
    exit 1
fi
echo "  No merge conflict markers — OK"

echo ""
echo "=== Checking for host-specific absolute paths ==="
# Harness rule: nothing committed may carry an absolute path under a user's
# home directory. Four offenders reached the tree before anything checked for
# them — one made `scxsim vm-run` fail on every machine but its author's, and
# two more sat in repm/ keying tests to a capture directory that stopped
# existing.
#
# Tracked files only, via git grep, because the rule is about what is
# COMMITTED. A filesystem grep would drown in target/, .venv/ and local
# scratch until somebody turned the check off.
#
# Repo-wide rather than scx-sim-only: validate.sh lives here but two of the
# four offenders were under repm/, so a check scoped to this directory would
# have caught half of them.
#
# This is NARROWER than the rule it enforces. It catches absolute
# /home/<user>/ and /Users/<user>/ paths, not the rule's tilde-rooted or
# hardcoded-username clauses; `~/bin/rt-app` in prose is legitimate and
# flagging it would get the whole check disabled, which is worse than the gap.
# Placeholder users (/home/user/ and friends) are documentation, not leaks.
HOSTPATH_ROOT=$(git rev-parse --show-toplevel)
# EXEMPT: captured output. These are recorded stdout/stderr and lldb
# transcripts where the path is what the tool actually printed on the day it
# ran. Rewriting them would make a captured record say something that did not
# happen — worse than the leak, and they break no clone because no code reads
# them. Listed by exact path so a new artifact must be exempted deliberately.
HOST_PATHS=$(git -C "$HOSTPATH_ROOT" grep -nIE '(/home|/Users)/[a-z][a-z0-9_-]*/' -- \
    ':(exclude)repm/tests/blind_e2e/' \
    ':(exclude)scx-sim/lldb_debug/*.transcript.txt' \
    | grep -vE '(/home|/Users)/(user|username|youruser|someuser)/' || true)
if [ -n "$HOST_PATHS" ]; then
    echo "ERROR: host-specific absolute paths in tracked files:"
    echo "$HOST_PATHS" | head -20
    echo ""
    echo "  A committed home-directory path breaks every other clone. Resolve"
    echo "  it at runtime (env var, then PATH, then a clear error), or make the"
    echo "  reference repo-relative. If it is CAPTURED OUTPUT, add its exact"
    echo "  path to the exemption list in validate.sh rather than editing the"
    echo "  record."
    exit 1
fi
echo "  No host-specific absolute paths — OK"

echo ""
echo "=== Checking Makefile syntax ==="
# Dry-run the Makefile to catch parse errors (missing separators, conflict
# markers, etc.). make -n prints commands without running them; a parse error
# causes a non-zero exit with "missing separator" or similar on stderr.
if make -n --warn-undefined-variables 2>&1 | grep -qi 'missing separator\|parse error\|unterminated'; then
    echo "ERROR: Makefile has syntax errors:"
    make -n 2>&1 | grep -i 'error\|separator' | head -5
    exit 1
fi
echo "  Makefile syntax OK"

echo ""
echo "=== Running cargo fmt --check ==="
cargo fmt --all -- --check

echo ""
echo "=== Checking the scx submodule is unmodified ==="
# Several crates compile upstream scx sources directly as path dependencies
# (scx_layered's alloc.rs and layer_core_growth.rs today). That puts those
# files in rustfmt's and clippy's module graph, so a bare `cargo fmt` rewrites
# them in place — silently destroying the "compiled byte-identical to the pin"
# guarantee that is the whole reason we link them instead of vendoring copies.
# rustfmt's `ignore` option is nightly-only, so guard the invariant itself.
if ! git -C ../scx diff --quiet HEAD 2>/dev/null; then
    echo "ERROR: the scx submodule has local modifications:"
    git -C ../scx status --short
    echo ""
    echo "scx is a pinned upstream checkout and must stay byte-identical to"
    echo "the gitlink. If 'cargo fmt' did this, revert with:"
    echo "    git -C scx checkout -- ."
    exit 1
fi
echo "  scx submodule clean"

echo ""
echo "=== Checking safe/ contains no unsafe code ==="
# Belt-and-suspenders: safe/mod.rs has #![forbid(unsafe_code)] which the
# compiler enforces, but this grep catches it before compilation even starts.
# Match unsafe blocks, fns, impls, and traits — skip comment-only lines.
SAFE_DIR="crates/scx_simulator/src/safe"
if grep -rn --include='*.rs' -E '\bunsafe\s+(fn|impl|trait|\{)' "$SAFE_DIR" \
   | grep -v '^\S*:\s*//' ; then
    echo "ERROR: unsafe code found in $SAFE_DIR — this directory must remain 100% safe."
    exit 1
fi
echo "  No unsafe code found in $SAFE_DIR — OK"

echo ""
echo "=== Running cargo clippy ==="
# --all-targets matters: plain `cargo clippy --all` lints only lib and bin
# targets (4 here), silently skipping all 69 test targets plus the bench and
# example. That made this gate WEAKER than the pre-commit hook, which has always
# used --all-targets, and it is how clippy::manual_checked_ops sat unnoticed in
# tests/csv_experiment.rs: CI structurally could not see test code.
cargo clippy --all-targets --workspace -- -D warnings

echo ""
echo "=== Building the embed surface without the standalone feature ==="
# Proves an embedder building scx_simulator with default-features = false still
# compiles: the library + binary reach schedulers via load_with_definition and
# never the standalone-gated simple()/tickless()/.../cosmos_with_numa() ctors
# (which bake in the compile-time SCHEDULER_SO_DIR). Because `-p` selects a single
# package, no other workspace member is built to request `standalone`, so this is
# immune to the cross-member feature unification that turns it back on in the
# --workspace runs above and below (separate invocations are cargo's own
# prescribed remedy for avoiding that unification).
cargo build -p scx_simulator --no-default-features

echo ""
echo "=== Running cargo llvm-cov nextest (instrumented; Rust library coverage) ==="
# Instrumented run REPLACES the plain `cargo nextest run --workspace`: it runs
# the identical nextest suite (same pass/fail) under llvm source-based coverage,
# so coverage is not additive cost. --no-report defers report generation; the
# per-crate ratchet below reuses this run's profile data (target/llvm-cov-target).
# --no-fail-fast surfaces ALL failing tests in one run. This
# instruments RUST only (SCX_SIM_COVERAGE unset) — scheduler .so C coverage stays
# coverage.sh's separate concern.
#
# embed_harness (a workspace member) is built and its link-contract test runs
# here: a broken EXPORTED_SYMS re-emission -> RTLD_NOW load failure -> test
# failure -> this aborts under set -e. That IS the embedder link-contract guard
# (no separate embed step needed).
command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "ERROR: cargo-llvm-cov is required for the Rust coverage gate." >&2
    echo "       Install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview" >&2
    exit 1
}
# CARGO_PROFILE_DEV_DEBUG=line-tables-only: the instrumented tree is what
# exhausts the GitHub runner's disk. Measured on this exact sequence with a
# fresh target dir:
#
#   after clippy --all-targets ......   667 MB
#   after build --no-default-features   2.1 GB
#   after llvm-cov nextest ..........  30 GB   (debug/ 2.1 GB + llvm-cov-target/ 28 GB)
#
# against 32 GB free on the runner after its cleanup step -- so the job died
# with SIGBUS in ld, which is what a linker mmap'ing its output onto a full
# filesystem gets, rather than a clean ENOSPC. The cost is one tree, not two:
# clippy --all-targets never links the 85 test executables, so target/debug
# contributes only ~2 GB here.
#
# Dropping full DWARF for this build alone takes llvm-cov-target/ from 28 GB to
# 16 GB, i.e. peak ~18 GB, and costs the gate NOTHING: LLVM source-based
# coverage carries its line information in __llvm_covmap, not in DWARF. Verified
# by comparing `cargo llvm-cov report --summary-only` between the two profiles --
# byte-identical (36006 regions / 76.44%, 2414 functions / 73.78%, 24182 lines /
# 74.91%).
#
# Scoped to this invocation on purpose rather than set in Cargo.toml: a plain
# `cargo test` / lldb session keeps full debuginfo, so local debuggability is
# unaffected. Running `cargo llvm-cov` by hand without this variable will
# rebuild its tree once (different profile fingerprint).
CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    cargo llvm-cov nextest --workspace --no-fail-fast --no-report

echo ""
echo "=== Running feature-gated tests (not reachable from --workspace) ==="
# `cargo nextest --workspace` builds with DEFAULT features, so any target with
# required-features is silently never built. scxsim-workload-ir's
# sched_basic_proportional is exactly that: required-features = ["ingest"],
# which loads a real scheduler .so and RUNS a lowered ktstr scenario. Measured:
# the package exposes 39 tests by default and 55 with the feature on, so 16
# tests -- including the only end-to-end ktstr-on-simulator check -- had never
# executed anywhere.
#
# Run as its own invocation rather than adding --all-features to the coverage
# gate above: that gate feeds the ratchet, and turning on every optional feature
# workspace-wide would move the coverage numbers it enforces for reasons
# unrelated to anyone's change.
cargo nextest run -p scxsim-workload-ir --features ingest --no-fail-fast

echo ""
echo "=== Running doc-tests ==="
# nextest cannot run doctests, so they stay a separate run (the one sanctioned
# `cargo test` use). Doctest-covered lines are not counted by the ratchet below.
cargo test --workspace --doc

echo ""
echo "=== Running feature-gated ktstr<->simulator tests ==="
# `--workspace` above builds every member with DEFAULT features, so a target
# marked `required-features` is silently skipped rather than run. Two of them
# are, and both load a real scheduler .so and execute a scenario end to end —
# exactly the tests that must not rot unnoticed:
#
#   scxsim-workload-ir  `ingest`  ktstr ops -> IR -> Scenario, then RUN it
#   scxsim-calibration  `sim`     that run compared against a live guest run,
#                                 under the pre-registered rejection rule
#
# The calibration one is a ratchet: `the_findings_as_first_measured` pins the
# current per-metric verdicts, so a change in simulator fidelity fails here
# instead of quietly moving. That is worthless if nothing runs it.
#
# Separate invocations because the features are per-crate and enabling them in
# the --workspace runs would unify them across members. Not instrumented, so
# they contribute no coverage and cannot perturb the ratchet below.
cargo nextest run -p scxsim-workload-ir --features ingest --test sched_basic_proportional
cargo nextest run -p scxsim-calibration --features sim --test calibrate_sched_basic_proportional

echo ""
echo "=== Running feature-gated example builds ==="
# The trace-comparison example is how the simulated half of a wprof comparison
# is produced. It is behind `sim` for the same reason the test is, so the
# --workspace build never compiles it and a break would go unnoticed.
#
# `--examples`, not `--example dump_sim_perfetto`. This named one target and so
# covered one; `slice_sweep` sits behind the same feature and was compiled by
# nothing at all — not here, not in any workflow, not by the Makefile. A guard
# whose coverage is a hand-maintained list drifts the moment a target is added,
# which is exactly what happened. `--examples` builds every example in the
# package whose required-features are satisfied, so the next one is covered on
# the day it is written.
cargo build -p scxsim-calibration --features sim --examples

echo ""
echo "=== Rust library coverage ratchet (self-test + gate) ==="
# Verify the ratchet's own logic, then gate. The gate reuses the profile data
# from the instrumented `cargo llvm-cov nextest` run above and hard-fails if any
# library crate regresses below its committed baseline
# (data/rust_coverage_baseline.csv, raise-only via
# `python3 scripts/coverage_ratchet.py --update-baseline`).
python3 scripts/test_coverage_ratchet.py
python3 scripts/coverage_ratchet.py

# --- Build e9-instrumented schedulers if e9patch is available ---
# The cargo commands above have already built the base .so files. NOTE: the
# instrumented `cargo llvm-cov nextest` builds into target/llvm-cov-target/, not
# target/debug/ — the target/debug build the e9 discovery below relies on is
# populated by the normal-target-dir steps (clippy --all-targets and the doctest
# run). If those are reordered/removed, this step degrades to a record_skip.
# If e9tool is installed, build _e9.so variants so the stress.py smoke
# test exercises e9patch mode automatically.
E9TOOL="${E9TOOL:-$(ls third_party/e9patch/e9tool 2>/dev/null || which e9tool 2>/dev/null || true)}"
if [ -n "$E9TOOL" ] && [ -x "$E9TOOL" ]; then
    echo ""
    echo "=== Building e9-instrumented schedulers ==="
    SCHED_DIR=$(ls -d target/debug/build/scx_simulator-*/out/schedulers 2>/dev/null | head -1)
    if [ -n "$SCHED_DIR" ]; then
        make -C schedulers BUILD_DIR="$PWD/$SCHED_DIR" e9
    else
        echo "  (skipped: scheduler build directory not found)"
        record_skip "e9-instrumented scheduler build (scheduler build directory not found)"
    fi
else
    echo ""
    echo "=== Skipping e9-instrumented schedulers (e9tool not found) ==="
    record_skip "e9-instrumented schedulers (e9tool not found; run: make install-e9patch)"
fi

echo ""
echo "=== Running stress.py smoke tests ==="
# Smoke tests to catch CLI bitrot in stress.py (sim-e0791).
# These verify the script parses correctly and constructs valid commands.

echo "  stress.py --help ..."
python3 bug_finding/stress.py --help > /dev/null

echo "  stress.py --list-workloads ..."
python3 bug_finding/stress.py --list-workloads > /dev/null

echo "  stress.py minimal run (~3s) ..."
# A minimal run: 0.05 min (~3s), 1 worker, 1 scheduler.
# stress.py auto-detects e9patch (_e9.so files); pass --no-e9patch only if
# they are absent to avoid a noisy warning.
# Exit code 0 = no bugs found, 1 = bugs found; both mean stress.py itself
# ran correctly. Only exit code >= 2 indicates a stress.py failure (e.g.
# bad CLI flags, Python exception).
E9_FLAG=""
if ! compgen -G "target/*/build/scx_simulator-*/out/schedulers/*_e9.so" > /dev/null 2>&1; then
    E9_FLAG="--no-e9patch"
    record_skip "stress.py e9patch mode (no _e9.so files built; run: make install-e9patch && make -C schedulers e9)"
fi
rc=0
python3 bug_finding/stress.py \
    --duration 0.05 --jobs 1 --schedulers simple $E9_FLAG \
    2>/dev/null || rc=$?
if [ "$rc" -ge 2 ]; then
    echo "FAIL: stress.py exited with code $rc (expected 0 or 1)"
    exit 1
fi
echo "  stress.py smoke tests passed (exit code: $rc)"

echo ""
echo "=== Running ASLR stability test ==="
# The ASLR test needs a release binary (it tests the re-exec path).
RELEASE_BIN="target/release/scxsim"
# Build it rather than skipping. Whether this binary happens to be lying around
# is a property of the developer's last command, not of the tree, so skipping on
# its absence made the ASLR gate run on some machines and not others -- and CI,
# which never builds release, would silently never run it at all.
if [ ! -x "$RELEASE_BIN" ]; then
    echo "  ($RELEASE_BIN not found — building it; the gate runs either way)"
    cargo build --release -p scx_simulator --bin scxsim
fi
./scripts/test_aslr.sh "$RELEASE_BIN"

echo ""
./scripts/typecheck.sh

# A skipped check is a FAILURE, not a footnote.
#
# CI runs this exact script (.github/workflows/simulator.yml runs `bash
# validate.sh`), so the commands are identical by construction and the only way
# local and CI can disagree is if one of them quietly ran less than the other.
# This block used to print "All checks passed", warn about the skips, and exit
# 0 -- so a run that never executed the ASLR gate was indistinguishable from a
# run that passed it. "validate.sh is green" has to mean "CI will be green",
# and it cannot mean that while green is reachable without running everything.
#
# There is deliberately NO opt-out. An env var that turns a skip back into a
# zero exit would mean "validate.sh is green" depends on whether someone set
# it -- which is the same silent-divergence this whole change exists to remove,
# reintroduced as a flag. If something is missing, install it; every skip
# message above names the command that fixes it.
if [ ${#SKIPPED[@]} -gt 0 ]; then
    echo ""
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo "!!! The following checks were SKIPPED:"
    for skip in "${SKIPPED[@]}"; do
        echo "!!!   - $skip"
    done
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo ""
    echo "ERROR: validate.sh is incomplete — ${#SKIPPED[@]} check(s) did not run,"
    echo "       so this result says nothing about whether CI will pass."
    exit 1
fi

echo ""
echo "=== All checks passed ==="
