#!/bin/bash
# scripts/modes/simulator.sh - Run ucache cartoon experiments via scx-sim.
#
# Provides run_simulator() which runs scx-sim with LAVD and tickless
# schedulers, with and without nice hints, producing CSV output per
# METRICS_SPECIFICATION.md.
#
# Usage:
#   source scripts/modes/simulator.sh
#   run_simulator 16 4 3 /tmp/sim_results
#
# Or standalone:
#   ./scripts/modes/simulator.sh 16 4 3 /tmp/sim_results

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Test matrix: scheduler × condition
SCHEDULERS=(lavd tickless)
CONDITIONS=(level1_nice0 level2_nice_hints)

# Default simulation parameters
DEFAULT_DURATION_MS=500

# run_simulator CORES CCXS REPS OUTPUT_DIR [DURATION_MS]
#
# Args:
#   CORES      - Number of simulated CPUs (e.g. 16, 24, 48)
#   CCXS       - Number of CCXs (currently informational; controls core topology)
#   REPS       - Number of repetitions per (scheduler × condition) pair
#   OUTPUT_DIR - Directory to write CSV results and perfetto traces
#   DURATION_MS - Optional simulation duration in ms (default: 500)
run_simulator() {
    local cores="${1:?Usage: run_simulator CORES CCXS REPS OUTPUT_DIR}"
    local ccxs="${2:?Usage: run_simulator CORES CCXS REPS OUTPUT_DIR}"
    local reps="${3:?Usage: run_simulator CORES CCXS REPS OUTPUT_DIR}"
    local output_dir="${4:?Usage: run_simulator CORES CCXS REPS OUTPUT_DIR}"
    local duration_ms="${5:-$DEFAULT_DURATION_MS}"
    local timestamp
    timestamp="$(date +%Y-%m-%d)"

    echo "=== Simulator Experiment ==="
    echo "  Cores:      $cores"
    echo "  CCXs:       $ccxs"
    echo "  Reps:       $reps"
    echo "  Output:     $output_dir"
    echo "  Duration:   ${duration_ms}ms"
    echo "  Schedulers: ${SCHEDULERS[*]}"
    echo "  Conditions: ${CONDITIONS[*]}"
    echo ""

    mkdir -p "$output_dir"

    local csv_file="$output_dir/scxsim_${cores}c_results.csv"
    local perfetto_dir="$output_dir/perfetto"
    mkdir -p "$perfetto_dir"

    # Write CSV header
    echo "timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes" > "$csv_file"

    local total=$(( ${#SCHEDULERS[@]} * ${#CONDITIONS[@]} * reps ))
    local done=0
    local failed=0

    for scheduler in "${SCHEDULERS[@]}"; do
        for condition in "${CONDITIONS[@]}"; do
            for rep in $(seq 1 "$reps"); do
                done=$((done + 1))
                local seed=$((42 + rep - 1))
                local label="${scheduler}/${condition}/rep${rep}"
                local perfetto_file="$perfetto_dir/${scheduler}_${condition}_rep${rep}.json"

                printf "  [%d/%d] %s (seed=%d) ..." "$done" "$total" "$label" "$seed"

                # Run the csv_experiment test, capturing stdout (CSV) and
                # suppressing stderr (sim progress/tracing).
                local csv_output
                if csv_output=$(
                    cd "$PROJECT_ROOT" && \
                    SCX_SIM_CORES="$cores" \
                    SCX_SIM_SCHEDULER="$scheduler" \
                    SCX_SIM_CONDITION="$condition" \
                    SCX_SIM_DURATION_MS="$duration_ms" \
                    SCX_SIM_SEED="$seed" \
                    SCX_SIM_REP="$rep" \
                    SCX_SIM_PERFETTO="$perfetto_file" \
                    SCX_SIM_TIMESTAMP="$timestamp" \
                    RUST_LOG=warn \
                    cargo test --release --test csv_experiment csv_experiment_run \
                        -- --nocapture 2>/dev/null
                ); then
                    # Extract only CSV data lines (skip cargo/test harness output)
                    echo "$csv_output" \
                        | grep "^${timestamp},simulator," \
                        >> "$csv_file"
                    local n_rows
                    n_rows=$(echo "$csv_output" | grep -c "^${timestamp},simulator," || true)
                    echo " OK ($n_rows rows)"
                else
                    echo " FAILED"
                    failed=$((failed + 1))
                fi
            done
        done
    done

    echo ""
    echo "=== Results ==="
    local total_rows
    total_rows=$(( $(wc -l < "$csv_file") - 1 ))
    echo "  CSV:     $csv_file ($total_rows data rows)"
    echo "  Traces:  $perfetto_dir/"
    echo "  Passed:  $((done - failed))/$done"
    if [ "$failed" -gt 0 ]; then
        echo "  FAILED:  $failed"
    fi

    # Verify non-empty CSV
    if [ "$total_rows" -le 0 ]; then
        echo "ERROR: CSV file has no data rows!" >&2
        return 1
    fi

    return 0
}

# Allow standalone execution: ./scripts/modes/simulator.sh CORES CCXS REPS OUTPUT_DIR
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    if [ $# -lt 4 ]; then
        echo "Usage: $0 CORES CCXS REPS OUTPUT_DIR [DURATION_MS]" >&2
        echo "" >&2
        echo "Run ucache cartoon experiment through scx-sim simulator." >&2
        echo "Produces CSV per METRICS_SPECIFICATION.md and Perfetto traces." >&2
        echo "" >&2
        echo "Example:" >&2
        echo "  $0 16 4 3 /tmp/sim_results" >&2
        exit 1
    fi
    run_simulator "$@"
fi
