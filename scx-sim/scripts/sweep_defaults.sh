#!/bin/bash
# sweep_defaults.sh - Run 2D parameter sweep with machine-appropriate defaults.
#
# Detects physical core count and generates a power-of-2 thread list up to
# the hardware thread limit (no SMT). Delegates to sweep_benchmark.py.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Use venv python if available
PYTHON="python3"
if [ -x "$PROJECT_ROOT/.venv/bin/python3" ]; then
    PYTHON="$PROJECT_ROOT/.venv/bin/python3"
fi

NPROC=$(nproc)
# Only use physical cores (no SMT) -- nproc/2 on this machine
HW_THREADS=$((NPROC / 2))
if [ "$HW_THREADS" -lt 1 ]; then
    HW_THREADS=1
fi

# Generate thread list: 1,2,4,8,...,HW_THREADS
THREADS=""
T=1
while [ "$T" -le "$HW_THREADS" ]; do
    if [ -n "$THREADS" ]; then
        THREADS="${THREADS},"
    fi
    THREADS="${THREADS}${T}"
    PREV=$T
    T=$((T * 2))
done

# Add exact HW_THREADS if not already included (not a power of 2)
if [ "$PREV" -ne "$HW_THREADS" ] && [ "$HW_THREADS" -gt "$PREV" ]; then
    THREADS="${THREADS},${HW_THREADS}"
fi

echo "=== Sweep defaults ==="
echo "  nproc:      $NPROC"
echo "  HW threads: $HW_THREADS (physical cores)"
echo "  Threads:    $THREADS"
echo ""

mkdir -p "$PROJECT_ROOT/debug"

exec "$PYTHON" "$SCRIPT_DIR/sweep_benchmark.py" \
    --scheduler lavd \
    --workload dsq_contention \
    --timeslices 1,50,100,200,300,400,500,600,700,800,900,1000,1500,2000 \
    --threads "$THREADS" \
    --end-time 200ms \
    --reps 3 \
    --csv "$PROJECT_ROOT/debug/sweep_results.csv" \
    --html "$PROJECT_ROOT/debug/sweep_plots.html" \
    --no-build \
    "$@"
