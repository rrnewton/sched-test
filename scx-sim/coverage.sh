#!/bin/bash
# coverage.sh — C code coverage for scx_simulator scheduler .so files
#
# Builds with clang source-based coverage instrumentation, runs tests,
# merges profile data, and generates reports.
#
# Usage:
#   ./coverage.sh [--html] [--lcov] [--keep-profraw] [--all] [--no-record]
#
# The --no-record flag skips appending coverage data to data/coverage.csv.
# By default, each run appends per-file and per-scheduler totals to the CSV.
#
# Environment:
#   SCX_SIM_COVERAGE=1 is set automatically by this script.
set -euo pipefail

cd "$(dirname "$0")"
SCRIPT_DIR="$(pwd)"
PROJ_ROOT="$(cd .. && pwd)"
COVERAGE_OUT="$SCRIPT_DIR/coverage-out"
CSV_FILE="$SCRIPT_DIR/data/coverage.csv"
CSV_HEADER="timestamp,gitdepth,commit,scheduler,category,file,lines,covered,missed,coverage_pct"

# --- Parse flags ---
FLAG_HTML=0
FLAG_LCOV=0
FLAG_KEEP_PROFRAW=0
FLAG_ALL=0
FLAG_NO_RECORD=0
for arg in "$@"; do
    case "$arg" in
        --html)         FLAG_HTML=1 ;;
        --lcov)         FLAG_LCOV=1 ;;
        --keep-profraw) FLAG_KEEP_PROFRAW=1 ;;
        --all)          FLAG_ALL=1 ;;
        --no-record)    FLAG_NO_RECORD=1 ;;
        -h|--help)
            echo "Usage: $0 [--html] [--lcov] [--keep-profraw] [--all] [--no-record]"
            echo ""
            echo "  --html          Generate HTML coverage report"
            echo "  --lcov          Generate LCOV coverage report"
            echo "  --keep-profraw  Keep raw .profraw files after merging"
            echo "  --all           Show all instrumented files (default: scheduler sources only)"
            echo "  --no-record     Skip appending results to data/coverage.csv"
            exit 0
            ;;
        *) echo "Unknown flag: $arg"; exit 1 ;;
    esac
done

# Default to --html if no report format specified
if [[ $FLAG_HTML -eq 0 && $FLAG_LCOV -eq 0 ]]; then
    FLAG_HTML=1
fi

# --- Prerequisite checks ---
echo "=== Checking prerequisites ==="

if ! command -v llvm-profdata &>/dev/null; then
    echo "ERROR: llvm-profdata not found. Install compiler-rt / llvm-tools." >&2
    exit 1
fi

if ! command -v llvm-cov &>/dev/null; then
    echo "ERROR: llvm-cov not found. Install compiler-rt / llvm-tools." >&2
    exit 1
fi

COMPILER="${BPF_CLANG:-clang}"
if ! command -v "$COMPILER" &>/dev/null; then
    echo "ERROR: $COMPILER not found." >&2
    exit 1
fi

RT_DIR=$("$COMPILER" --print-runtime-dir 2>/dev/null || true)
if [[ -z "$RT_DIR" ]] || ! ls "$RT_DIR"/libclang_rt.profile*.a &>/dev/null; then
    echo "ERROR: libclang_rt.profile not found. Install compiler-rt." >&2
    echo "  Try: dnf install compiler-rt" >&2
    exit 1
fi

echo "  llvm-profdata: $(command -v llvm-profdata)"
echo "  llvm-cov:      $(command -v llvm-cov)"
echo "  compiler:      $(command -v "$COMPILER")"
echo "  runtime dir:   $RT_DIR"

# --- Clean previous coverage data ---
echo ""
echo "=== Cleaning previous coverage data ==="
rm -rf "$COVERAGE_OUT"
mkdir -p "$COVERAGE_OUT"

# Remove stale profraw files from the project tree
find "$PROJ_ROOT" -name '*.profraw' -delete 2>/dev/null || true

# Force a rebuild of the coverage-instrumented artifacts
echo ""
echo "=== Building with coverage instrumentation ==="
export SCX_SIM_COVERAGE=1
export LLVM_PROFILE_FILE="$COVERAGE_OUT/scxsim-%p-%m.profraw"

# Clean the build cache so we get a fresh instrumented build
cargo clean -p scx_simulator -p scx_perf -p scx_cgroup_tree 2>/dev/null || true

# Build tests (this also triggers build.rs with SCX_SIM_COVERAGE=1)
cargo test --all --no-run --message-format=json 2>/dev/null \
    | tee "$COVERAGE_OUT/test-build.json" \
    | jq -r 'select(.executable != null) | .executable' \
    > "$COVERAGE_OUT/test-binaries.txt" || true

# Verify we found test binaries
if [[ ! -s "$COVERAGE_OUT/test-binaries.txt" ]]; then
    echo "ERROR: No test binaries found." >&2
    exit 1
fi

echo "  Found $(wc -l < "$COVERAGE_OUT/test-binaries.txt") test binary/binaries"

# --- Run tests ---
echo ""
echo "=== Running tests to generate coverage data ==="
# Run tests sequentially to avoid PMU timer signal leaks between tests
# (see sim-c3fd09: SIGSTKFLT crash during preemptive interleaving).
cargo test --all -- --test-threads=1 2>&1 | tee "$COVERAGE_OUT/test-output.txt"

# --- Merge profraw files ---
echo ""
echo "=== Merging profile data ==="
PROFRAW_FILES=()
while IFS= read -r -d '' f; do
    PROFRAW_FILES+=("$f")
done < <(find "$COVERAGE_OUT" "$PROJ_ROOT" -name '*.profraw' -print0 2>/dev/null)

if [[ ${#PROFRAW_FILES[@]} -eq 0 ]]; then
    echo "ERROR: No .profraw files found. Coverage instrumentation may not be working." >&2
    exit 1
fi

echo "  Found ${#PROFRAW_FILES[@]} profraw file(s)"
llvm-profdata merge -sparse "${PROFRAW_FILES[@]}" -o "$COVERAGE_OUT/merged.profdata"
echo "  Merged into $COVERAGE_OUT/merged.profdata"

# --- Find coverage-instrumented .so files ---
# The scheduler .so files contain the coverage mapping data
SO_FILES=()
while IFS= read -r bin; do
    # Find the OUT_DIR used by this build — it's the parent of the test binary's
    # deps directory, but we need the build script's OUT_DIR which contains
    # schedulers_cov/
    BUILD_OUT=$(dirname "$bin")
    # Search for coverage .so files in the build tree
    while IFS= read -r -d '' so; do
        SO_FILES+=("$so")
    done < <(find "$BUILD_OUT" -name 'libscx_*.so' -print0 2>/dev/null)
done < "$COVERAGE_OUT/test-binaries.txt"

# Also search the cargo target directory directly
while IFS= read -r -d '' so; do
    SO_FILES+=("$so")
done < <(find "$PROJ_ROOT/target" -path '*/schedulers_cov/libscx_*.so' -print0 2>/dev/null)

# Deduplicate
if [[ ${#SO_FILES[@]} -gt 0 ]]; then
    readarray -t SO_FILES < <(printf '%s\n' "${SO_FILES[@]}" | sort -u)
fi

# Build the -object flags for llvm-cov
OBJECT_FLAGS=()
# First object is the test binary itself (for static lib coverage)
FIRST_BIN=$(head -1 "$COVERAGE_OUT/test-binaries.txt")
OBJECT_FLAGS+=("$FIRST_BIN")
# Additional objects are the .so files
for so in "${SO_FILES[@]}"; do
    OBJECT_FLAGS+=("-object" "$so")
done

echo "  Coverage objects: ${#OBJECT_FLAGS[@]} (1 binary + ${#SO_FILES[@]} .so files)"

# --- Source filter ---
# By default, only show scheduler BPF source files (the code under test).
# Use --all to see everything including stubs, infrastructure, and headers.
SOURCE_FILTER=()
if [[ $FLAG_ALL -eq 0 ]]; then
    # Exclude infrastructure, stubs, headers, and wrappers — keep only
    # scheds/rust/scx_*/src/bpf/* and schedulers/simple/scx_simple.bpf.c
    SOURCE_FILTER+=(
        "-ignore-filename-regex=lib/scxtest/"
        "-ignore-filename-regex=csrc/sim_"
        "-ignore-filename-regex=scheds/include/"
        "-ignore-filename-regex=bpf_experimental\\.h"
        "-ignore-filename-regex=bpf_arena_common"
        "-ignore-filename-regex=libbpf-sys-.*/out/include/"
        "-ignore-filename-regex=schedulers/.*/wrapper\\.c"
        "-ignore-filename-regex=cosmos_main_patched\\.c"
        "-ignore-filename-regex=/intf\\.h$"
    )
fi

# --- Generate reports ---
echo ""
echo "=== Generating coverage reports ==="

if [[ $FLAG_HTML -eq 1 ]]; then
    echo "  Generating HTML report..."
    llvm-cov show "${OBJECT_FLAGS[@]}" \
        -instr-profile="$COVERAGE_OUT/merged.profdata" \
        "${SOURCE_FILTER[@]}" \
        -format=html \
        -output-dir="$COVERAGE_OUT/html" \
        -show-line-counts-or-regions \
        -show-instantiations=false \
        -Xdemangler=c++filt
    echo "  HTML report: $COVERAGE_OUT/html/index.html"
fi

if [[ $FLAG_LCOV -eq 1 ]]; then
    echo "  Generating LCOV report..."
    llvm-cov export "${OBJECT_FLAGS[@]}" \
        -instr-profile="$COVERAGE_OUT/merged.profdata" \
        "${SOURCE_FILTER[@]}" \
        -format=lcov \
        > "$COVERAGE_OUT/coverage.lcov"
    echo "  LCOV report: $COVERAGE_OUT/coverage.lcov"
fi

# --- Discover schedulers (same convention as schedulers/Makefile) ---
SCHEDULERS=()
for wrapper in "$SCRIPT_DIR"/schedulers/*/wrapper.c; do
    sched_dir=$(dirname "$wrapper")
    SCHEDULERS+=("$(basename "$sched_dir")")
done

# --- Helper: build exclusion filters for a single scheduler ---
# Outputs filter flags that exclude every OTHER scheduler's source files.
build_sched_exclusion_filters() {
    local sched="$1"
    for other in "${SCHEDULERS[@]}"; do
        if [[ "$other" != "$sched" ]]; then
            echo "-ignore-filename-regex=schedulers/${other}/"
            echo "-ignore-filename-regex=scheds/rust/scx_${other}/"
        fi
    done
}

# --- Per-scheduler coverage tables ---
# For each scheduler, show a separate table by excluding all OTHER
# schedulers' source directories.
generate_scheduler_report() {
    local sched="$1"
    shift
    local extra_filters=("$@")

    local sched_filters=()
    while IFS= read -r f; do
        sched_filters+=("$f")
    done < <(build_sched_exclusion_filters "$sched")

    echo ""
    echo "=== Coverage: $sched ==="
    llvm-cov report "${OBJECT_FLAGS[@]}" \
        -instr-profile="$COVERAGE_OUT/merged.profdata" \
        "${extra_filters[@]}" \
        "${sched_filters[@]}" \
        -show-region-summary=false
}

# --- CSV recording ---
# Appends per-file and total rows for a scheduler to the CSV file.
# Uses llvm-cov export (JSON) for reliable machine-readable parsing.
record_scheduler_csv() {
    local sched="$1"
    local timestamp="$2"
    local gitdepth="$3"
    local commit="$4"
    shift 4
    local extra_filters=("$@")

    local sched_filters=()
    while IFS= read -r f; do
        sched_filters+=("$f")
    done < <(build_sched_exclusion_filters "$sched")

    # Export JSON summary for this scheduler
    local json
    json=$(llvm-cov export "${OBJECT_FLAGS[@]}" \
        -instr-profile="$COVERAGE_OUT/merged.profdata" \
        "${extra_filters[@]}" \
        "${sched_filters[@]}" \
        --summary-only \
        --skip-functions \
        --skip-branches 2>/dev/null)

    # Parse per-file rows and total from the JSON using jq.
    # The JSON structure: { data: [{ files: [{ filename, summary: { lines: { count, covered, ... } } }], totals: { lines: { count, covered, ... } } }] }
    # Strip PROJ_ROOT prefix from filenames to keep paths relative (avoids
    # committing machine-specific absolute paths).
    echo "$json" | jq -r --arg ts "$timestamp" --arg gd "$gitdepth" \
        --arg cm "$commit" --arg sc "$sched" --arg root "$PROJ_ROOT/" '
        .data[0] as $d |
        # Per-file rows
        ($d.files[] |
            (.filename | if startswith($root) then .[$root | length:] else . end) as $fn |
            .summary.lines as $l |
            ($l.count - $l.covered) as $missed |
            (if $l.count > 0 then ($l.covered * 100.0 / $l.count) else 0 end) as $pct |
            [$ts, $gd, $cm, $sc, "per_file", $fn, ($l.count|tostring), ($l.covered|tostring), ($missed|tostring), ($pct * 10 | round / 10 | tostring)]
            | join(",")
        ),
        # Total row
        ($d.totals.lines as $l |
            ($l.count - $l.covered) as $missed |
            (if $l.count > 0 then ($l.covered * 100.0 / $l.count) else 0 end) as $pct |
            [$ts, $gd, $cm, $sc, "total", "", ($l.count|tostring), ($l.covered|tostring), ($missed|tostring), ($pct * 10 | round / 10 | tostring)]
            | join(",")
        )
    ' >> "$CSV_FILE"
}

for sched in "${SCHEDULERS[@]}"; do
    generate_scheduler_report "$sched" "${SOURCE_FILTER[@]}"
done

# Overall summary (always shown)
echo ""
echo "=== Coverage Summary (all schedulers) ==="
llvm-cov report "${OBJECT_FLAGS[@]}" \
    -instr-profile="$COVERAGE_OUT/merged.profdata" \
    "${SOURCE_FILTER[@]}" \
    -show-region-summary=false

# --- Record CSV ---
if [[ $FLAG_NO_RECORD -eq 0 ]]; then
    echo ""
    echo "=== Recording coverage to CSV ==="
    mkdir -p "$(dirname "$CSV_FILE")"

    # Write header if the file doesn't exist or is empty
    if [[ ! -s "$CSV_FILE" ]]; then
        echo "$CSV_HEADER" > "$CSV_FILE"
    fi

    # Collect git metadata once
    CSV_TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    CSV_GITDEPTH=$(git rev-list --count HEAD)
    CSV_COMMIT=$(git rev-parse --short HEAD)

    for sched in "${SCHEDULERS[@]}"; do
        record_scheduler_csv "$sched" "$CSV_TIMESTAMP" "$CSV_GITDEPTH" "$CSV_COMMIT" "${SOURCE_FILTER[@]}"
    done
    echo "  Appended coverage data to $CSV_FILE"
fi

# --- Cleanup ---
if [[ $FLAG_KEEP_PROFRAW -eq 0 ]]; then
    rm -f "${PROFRAW_FILES[@]}"
    echo ""
    echo "  Cleaned up profraw files (use --keep-profraw to retain)"
fi

echo ""
echo "=== Done ==="
