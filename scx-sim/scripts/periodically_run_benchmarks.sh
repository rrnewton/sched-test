#!/bin/bash
# Periodically run benchmarks when git depth advances by N+ commits
#
# Usage: ./scripts/periodically_run_benchmarks.sh [min_depth_delta]
#   min_depth_delta: Minimum commits since last benchmark (default: 5)
#
# This is a thin wrapper around run_benchmark.sh that only runs if
# we've advanced enough commits since the last recorded benchmark.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MIN_DEPTH_DELTA="${1:-5}"

# Get CPU name (same logic as run_benchmark.sh)
get_cpu_name() {
    local cpu_name
    cpu_name=$(grep "model name" /proc/cpuinfo | head -1 | cut -d':' -f2 | sed 's/^[ \t]*//')
    echo "$cpu_name" | sed 's/ /_/g' | sed 's/[^a-zA-Z0-9_-]//g'
}

CPU_NAME=$(get_cpu_name)
CSV_FILE="$REPO_ROOT/data/benchmarks/$CPU_NAME/perf_history.csv"

# Get current git depth
current_depth=$("$SCRIPT_DIR/gitdepth.sh")

# Get last recorded git depth from CSV (0 if file doesn't exist)
if [ -f "$CSV_FILE" ]; then
    last_depth=$(tail -n 1 "$CSV_FILE" | cut -d',' -f3)
    if [ -z "$last_depth" ] || ! [[ "$last_depth" =~ ^[0-9]+$ ]]; then
        last_depth=0
    fi
else
    last_depth=0
fi

depth_delta=$((current_depth - last_depth))

echo "Current git depth: $current_depth"
echo "Last recorded depth: $last_depth"
echo "Depth delta: $depth_delta (minimum: $MIN_DEPTH_DELTA)"

if [ "$depth_delta" -lt "$MIN_DEPTH_DELTA" ]; then
    echo "Skipping benchmarks - need $((MIN_DEPTH_DELTA - depth_delta)) more commits"
    exit 0
fi

echo "Running benchmarks..."
exec "$SCRIPT_DIR/run_benchmark.sh"
