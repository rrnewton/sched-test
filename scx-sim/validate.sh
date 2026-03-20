#!/bin/bash
# validate.sh - Local validation script for scx_simulator workspace
# Run this before committing to ensure code quality.
set -euo pipefail

cd "$(dirname "$0")"

# Track skipped checks so we can warn at the end.
SKIPPED=()
record_skip() { SKIPPED+=("$1"); }

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
cargo clippy --all -- -D warnings

echo ""
echo "=== Running cargo nextest ==="
cargo nextest run --workspace

echo ""
echo "=== Running doc-tests ==="
cargo test --workspace --doc

# --- Build e9-instrumented schedulers if e9patch is available ---
# The cargo commands above have already built the base .so files.
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
echo "=== Running Python type checks ==="
# Find all Python files (excluding venv and build dirs)
PYTHON_FILES=$(find . -name "*.py" -not -path "./.venv/*" -not -path "./target/*" -not -path "./debug/*")
if [ -n "$PYTHON_FILES" ]; then
    if command -v mypy &>/dev/null; then
        mypy --strict $PYTHON_FILES
    elif .venv/bin/mypy --strict $PYTHON_FILES 2>/dev/null; then
        true
    else
        echo "WARNING: mypy not found, skipping Python type checks"
        echo "  Install with: pip install mypy (or .venv/bin/pip install mypy)"
        record_skip "Python type checks (mypy not found)"
    fi
else
    echo "  No Python files found."
fi

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
