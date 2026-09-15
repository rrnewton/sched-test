#!/bin/bash
# test_aslr.sh — Verify that ASLR disable produces deterministic addresses.
#
# Runs `scxsim print-addresses` multiple times and asserts:
#   1. With ASLR disabled (default): .so, heap, and stack addresses are stable.
#   2. With ASLR enabled (--no-disable-aslr): addresses vary between runs.
#
# Usage: ./scripts/test_aslr.sh [path-to-scxsim]
set -euo pipefail

SCXSIM="${1:-}"

# Auto-detect the scxsim binary if not provided.
if [ -z "$SCXSIM" ]; then
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
    for candidate in \
        "$REPO_DIR/target/release/scxsim" \
        "$REPO_DIR/target/debug/scxsim"; do
        if [ -x "$candidate" ]; then
            SCXSIM="$candidate"
            break
        fi
    done
fi

if [ -z "$SCXSIM" ] || [ ! -x "$SCXSIM" ]; then
    echo "ERROR: scxsim binary not found. Build with: cargo build --release" >&2
    exit 1
fi

RUNS=3
PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $1" >&2; }

# ---------------------------------------------------------------------------
# Collect addresses
# ---------------------------------------------------------------------------
collect_addresses() {
    # $1 = extra flags (empty string or "--no-disable-aslr")
    local flags="$1"
    local results=()
    for _ in $(seq "$RUNS"); do
        # Do NOT discard stderr here. This runs under `set -e` inside a
        # command substitution, so a failing scxsim used to kill the whole
        # script with zero output — the exact silent-failure mode this repo
        # forbids. Capture stderr and report it before bailing out.
        # shellcheck disable=SC2086
        local out err rc=0
        err=$(mktemp)
        out=$("$SCXSIM" print-addresses -s simple $flags 2>"$err") || rc=$?
        if [ "$rc" -ne 0 ]; then
            echo "ERROR: '$SCXSIM print-addresses -s simple $flags' exited $rc" >&2
            sed 's/^/    /' "$err" >&2
            rm -f "$err"
            exit 1
        fi
        rm -f "$err"
        results+=("$out")
    done
    printf '%s\n' "${results[@]}"
}

extract_field() {
    # $1 = field name (so_base, heap, stack), stdin = collected output
    grep "^${1}=" | sed "s/^${1}=//"
}

# ---------------------------------------------------------------------------
# Test 1: ASLR disabled — addresses must be identical across runs
# ---------------------------------------------------------------------------
echo "Test 1: ASLR disabled — addresses stable across $RUNS runs"
DISABLED_OUTPUT=$(collect_addresses "")

for field in so_base heap stack; do
    UNIQUE=$(echo "$DISABLED_OUTPUT" | extract_field "$field" | sort -u | wc -l)
    ADDR=$(echo "$DISABLED_OUTPUT" | extract_field "$field" | head -1)
    if [ "$UNIQUE" -eq 1 ]; then
        pass "$field is stable ($ADDR)"
    else
        fail "$field varies across runs:"
        echo "$DISABLED_OUTPUT" | extract_field "$field" | sed 's/^/    /' >&2
    fi
done

# ---------------------------------------------------------------------------
# Test 2: ASLR enabled — addresses must vary across runs
# ---------------------------------------------------------------------------
echo "Test 2: ASLR enabled — addresses vary across $RUNS runs"
ENABLED_OUTPUT=$(collect_addresses "--no-disable-aslr")

for field in so_base heap stack; do
    UNIQUE=$(echo "$ENABLED_OUTPUT" | extract_field "$field" | sort -u | wc -l)
    if [ "$UNIQUE" -gt 1 ]; then
        pass "$field varies (${UNIQUE} distinct values)"
    else
        ADDR=$(echo "$ENABLED_OUTPUT" | extract_field "$field" | head -1)
        fail "$field is unexpectedly stable ($ADDR) — ASLR may not be effective"
    fi
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
TOTAL=$((PASS + FAIL))
echo "ASLR test: $PASS/$TOTAL passed"
if [ "$FAIL" -gt 0 ]; then
    echo "ASLR test FAILED" >&2
    exit 1
fi
echo "ASLR test PASSED"
