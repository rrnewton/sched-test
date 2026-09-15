#!/bin/bash
# Run benchmarks and append results to performance history CSV
#
# Usage: ./scripts/run_benchmark.sh
#
# Detects CPU model, collects git metadata, runs the benchmark matrix,
# and appends results to data/benchmarks/<CPU_NAME>/perf_history.csv.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Use the venv python if available, otherwise system python3
if [ -x "$REPO_ROOT/.venv/bin/python3" ]; then
    PYTHON="$REPO_ROOT/.venv/bin/python3"
else
    PYTHON="python3"
fi

# Get CPU name and normalize for directory paths
get_cpu_name() {
    local cpu_name
    cpu_name=$(grep "model name" /proc/cpuinfo | head -1 | cut -d':' -f2 | sed 's/^[ \t]*//')
    echo "$cpu_name" | sed 's/ /_/g' | sed 's/[^a-zA-Z0-9_-]//g'
}

CPU_NAME=$(get_cpu_name)
RESULTS_DIR="$REPO_ROOT/data/benchmarks/$CPU_NAME"
HISTORY_FILE="$RESULTS_DIR/perf_history.csv"

# Ensure results directory exists
mkdir -p "$RESULTS_DIR"

# Collect git metadata for display
GIT_COMMIT_SHORT=$(git -C "$REPO_ROOT" rev-parse --short HEAD)
GIT_DEPTH=$("$SCRIPT_DIR/gitdepth.sh")
GIT_BRANCH=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)
GIT_DIRTY=""
if ! git -C "$REPO_ROOT" diff-index --quiet HEAD --; then
    GIT_DIRTY="_dirty"
fi

TIMESTAMP_READABLE=$(date +"%Y-%m-%d %H:%M:%S %Z")
TIMESTAMP_FILE=$(date +"%Y%m%d")
LOG_FILE="$RESULTS_DIR/benchmark_log_${TIMESTAMP_FILE}_#${GIT_DEPTH}.log"

echo "=== Running Benchmarks ==="
echo "CPU: $CPU_NAME"
echo "Timestamp: $TIMESTAMP_READABLE"
echo "Git commit: $GIT_COMMIT_SHORT (depth: $GIT_DEPTH, branch: $GIT_BRANCH)${GIT_DIRTY}"
echo "Results will be appended to: $HISTORY_FILE"
echo "Log: $LOG_FILE"
echo ""

# Run the benchmark suite, appending to history CSV with git metadata
"$PYTHON" "$REPO_ROOT/scripts/benchmark.py" run \
    --csv "$HISTORY_FILE" \
    --append \
    --git-metadata \
    --html "$RESULTS_DIR/benchmark_latest.html" \
    2>&1 | tee "$LOG_FILE"

echo ""
echo "=== Generating history dashboard ==="
if [ -f "$HISTORY_FILE" ]; then
    "$PYTHON" "$REPO_ROOT/scripts/benchmark.py" plot-history \
        "$HISTORY_FILE" \
        -o "$RESULTS_DIR/perf_history.html"
fi

echo ""
echo "=== Results Saved ==="
echo "Performance history: $HISTORY_FILE"
echo "Log: $LOG_FILE"
echo ""
echo "Recent CSV entries:"
tail -n 5 "$HISTORY_FILE"
