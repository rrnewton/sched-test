#!/bin/bash
# validate.sh - Local validation script for scx_simulator workspace
# Run this before committing to ensure code quality.
set -euo pipefail

cd "$(dirname "$0")"

echo "=== Running cargo fmt --check ==="
cargo fmt --all -- --check

echo ""
echo "=== Running cargo clippy ==="
cargo clippy --all -- -D warnings

echo ""
echo "=== Running cargo nextest ==="
cargo nextest run --workspace

echo ""
echo "=== Running doc-tests ==="
cargo test --workspace --doc

echo ""
echo "=== Running stress.py smoke tests ==="
# Smoke tests to catch CLI bitrot in stress.py (sim-e0791).
# These verify the script parses correctly and constructs valid commands.

echo "  stress.py --help ..."
python3 bug_finding/stress.py --help > /dev/null

echo "  stress.py --list-workloads ..."
python3 bug_finding/stress.py --list-workloads > /dev/null

echo "  stress.py minimal run (~3s) ..."
# A minimal run: 0.05 min (~3s), 1 worker, 1 scheduler, no e9patch.
# Exit code 0 = no bugs found, 1 = bugs found; both mean stress.py itself
# ran correctly. Only exit code >= 2 indicates a stress.py failure (e.g.
# bad CLI flags, Python exception).
rc=0
python3 bug_finding/stress.py \
    --duration 0.05 --jobs 1 --schedulers simple --no-e9patch \
    2>/dev/null || rc=$?
if [ "$rc" -ge 2 ]; then
    echo "FAIL: stress.py exited with code $rc (expected 0 or 1)"
    exit 1
fi
echo "  stress.py smoke tests passed (exit code: $rc)"

echo ""
echo "=== All checks passed ==="
