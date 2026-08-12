#!/bin/bash
# validate.sh - Local validation script for scx_simulator workspace
# Run this before committing to ensure code quality.
set -euo pipefail

cd "$(dirname "$0")"

# Track skipped checks so we can warn at the end.
SKIPPED=()
record_skip() { SKIPPED+=("$1"); }

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
cargo llvm-cov nextest --workspace --no-fail-fast --no-report

echo ""
echo "=== Running doc-tests ==="
# nextest cannot run doctests, so they stay a separate run (the one sanctioned
# `cargo test` use). Doctest-covered lines are not counted by the ratchet below.
cargo test --workspace --doc

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
if [ -x "$RELEASE_BIN" ]; then
    ./scripts/test_aslr.sh "$RELEASE_BIN"
else
    echo "  (skipped: $RELEASE_BIN not found; run: cargo build --release)"
    record_skip "ASLR stability test (release binary not found)"
fi

echo ""
./scripts/typecheck.sh

echo ""
echo "=== All checks passed ==="

if [ ${#SKIPPED[@]} -gt 0 ]; then
    echo ""
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo "!!! WARNING: The following checks were SKIPPED:"
    for skip in "${SKIPPED[@]}"; do
        echo "!!!   - $skip"
    done
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
fi
