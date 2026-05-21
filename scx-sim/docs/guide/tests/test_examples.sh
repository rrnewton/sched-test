#!/usr/bin/env bash
#
# test_examples.sh — verify every scxsim guide example is runnable.
#
# What it does
# ------------
#   For each `scx-sim/examples/*.json` rt-app workload, run it through
#   `scxsim run` for a short simulated duration with a short watchdog,
#   and assert exit code 0. Failures print the captured stderr/stdout
#   so the breakage is diagnosable from a CI log alone.
#
#   This is a smoke test for the GUIDE's example fixtures only — it is
#   intentionally separate from `validate.sh` (the full cargo build +
#   nextest suite) and from `crates/scx_simulator/tests/` (the integration
#   tests against `tests/fixtures/`). The point is to catch the specific
#   failure mode "I updated scxsim's CLI / rt-app loader / scheduler
#   defaults and silently broke the example a reader is about to copy
#   from the guide".
#
# Tunables (all overrideable from the environment)
# ------------------------------------------------
#   SCXSIM            Path to the scxsim binary.
#                     Default: scx-sim/target/release/scxsim (relative
#                     to repo root). If the release binary is absent,
#                     falls back to `cargo build --release -p
#                     scx_simulator --bin scxsim`.
#   SCXSIM_TEST_DURATION   Simulated duration per run. Default: 100ms.
#   SCXSIM_TEST_WATCHDOG   Wall-clock watchdog. Default: 5s.
#   SCXSIM_TEST_CPUS       Simulated CPU count. Default: 4.
#   SCXSIM_TEST_SCHEDULERS Space-separated scheduler list to sweep.
#                          Default: "simple" (one cheap, fast, default
#                          scheduler). Set to "simple lavd" or similar
#                          to widen the matrix.
#   SCXSIM_TEST_EXAMPLES_DIR   Directory of example JSON workloads.
#                              Default: scx-sim/examples relative to
#                              repo root.
#   SCXSIM_TEST_TIMEOUT_S     Per-run wall-clock timeout. Default: 60s.
#                             This is a belt to scxsim's own
#                             --watchdog suspenders.
#
# Exit codes
# ----------
#   0  every (example × scheduler) cell exited 0.
#   1  at least one cell failed; see captured output above the summary.
#   2  setup failure (binary not buildable, examples dir missing, etc.).
#
# Invocation
# ----------
#   From scx-sim/:
#       bash docs/guide/tests/test_examples.sh
#   Or via the Makefile shortcut:
#       make test-examples
#
# This script must stay portable bash — it runs on Ubuntu CI runners
# without sourcing anything else from scx-sim/scripts/.

set -u
set -o pipefail

# ----------------------------------------------------------------------
# Locate repo root and scx-sim/.
# ----------------------------------------------------------------------
# Resolve regardless of where the user invoked us from. The script
# lives at scx-sim/docs/guide/tests/test_examples.sh, so the scx-sim
# root is three levels up from its own directory.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCXSIM_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"   # scx-sim/

EXAMPLES_DIR="${SCXSIM_TEST_EXAMPLES_DIR:-$SCXSIM_ROOT/examples}"
DURATION="${SCXSIM_TEST_DURATION:-100ms}"
WATCHDOG="${SCXSIM_TEST_WATCHDOG:-5s}"
CPUS="${SCXSIM_TEST_CPUS:-4}"
SCHEDULERS="${SCXSIM_TEST_SCHEDULERS:-simple}"
TIMEOUT_S="${SCXSIM_TEST_TIMEOUT_S:-60}"

# ----------------------------------------------------------------------
# Color helpers (auto-disable if stdout is not a TTY or NO_COLOR is set).
# ----------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    BOLD=$(printf '\033[1m')
    RESET=$(printf '\033[0m')
else
    RED=""; GREEN=""; YELLOW=""; BOLD=""; RESET=""
fi

log()     { printf '%s\n' "$*"; }
log_ok()  { printf '%s\n' "${GREEN}PASS${RESET} $*"; }
log_err() { printf '%s\n' "${RED}FAIL${RESET} $*" >&2; }
log_info(){ printf '%s\n' "${BOLD}>>>${RESET} $*"; }
log_warn(){ printf '%s\n' "${YELLOW}WARN${RESET} $*" >&2; }

# ----------------------------------------------------------------------
# Locate (or build) the scxsim binary.
# ----------------------------------------------------------------------
SCXSIM_DEFAULT="$SCXSIM_ROOT/target/release/scxsim"
SCXSIM="${SCXSIM:-$SCXSIM_DEFAULT}"

if [ ! -x "$SCXSIM" ]; then
    log_warn "scxsim binary not found at $SCXSIM"
    log_info "building scxsim (cargo build --release -p scx_simulator --bin scxsim)"
    if ! (cd "$SCXSIM_ROOT" && cargo build --release -p scx_simulator --bin scxsim); then
        log_err "cargo build failed; cannot run example tests"
        exit 2
    fi
    SCXSIM="$SCXSIM_DEFAULT"
fi

if [ ! -x "$SCXSIM" ]; then
    log_err "scxsim still not executable at $SCXSIM after build attempt"
    exit 2
fi

if [ ! -d "$EXAMPLES_DIR" ]; then
    log_err "examples directory not found: $EXAMPLES_DIR"
    exit 2
fi

# ----------------------------------------------------------------------
# Enumerate example workloads.
# ----------------------------------------------------------------------
mapfile -t EXAMPLES < <(find "$EXAMPLES_DIR" -maxdepth 1 -type f -name '*.json' | sort)

if [ "${#EXAMPLES[@]}" -eq 0 ]; then
    log_err "no *.json examples found under $EXAMPLES_DIR"
    exit 2
fi

# ----------------------------------------------------------------------
# Run the matrix.
# ----------------------------------------------------------------------
log_info "scxsim:        $SCXSIM"
log_info "examples dir:  $EXAMPLES_DIR  (${#EXAMPLES[@]} workloads)"
log_info "schedulers:    $SCHEDULERS"
log_info "duration:      $DURATION   watchdog: $WATCHDOG   cpus: $CPUS"
log_info "wall timeout:  ${TIMEOUT_S}s per run"
echo

FAILED=()
PASSED=0
TOTAL=0

# Each failure's captured output goes into its own file so the summary
# can list them, and the CI log shows every error inline.
TMPDIR_RUN="$(mktemp -d -t scxsim-examples.XXXXXX)"
trap 'rm -rf "$TMPDIR_RUN"' EXIT

for sched in $SCHEDULERS; do
    for example in "${EXAMPLES[@]}"; do
        TOTAL=$((TOTAL + 1))
        name="$(basename "$example" .json)"
        label="[$sched] $name"
        log_file="$TMPDIR_RUN/${sched}__${name}.log"

        # `timeout --foreground` so a Ctrl-C in an interactive run
        # still kills the child. `--kill-after=5s` ensures we don't
        # leave a wedged scxsim behind if SIGTERM is ignored.
        if timeout --foreground --kill-after=5s "${TIMEOUT_S}s" \
                "$SCXSIM" run \
                    --scheduler "$sched" \
                    --cpus "$CPUS" \
                    --duration "$DURATION" \
                    --watchdog "$WATCHDOG" \
                    "$example" \
                    >"$log_file" 2>&1
        then
            log_ok "$label"
            PASSED=$((PASSED + 1))
        else
            rc=$?
            log_err "$label  (exit=$rc)"
            echo "--- captured output ($log_file) ---" >&2
            # Tail rather than full dump in case a long-running scheduler
            # left a huge log; the last ~120 lines are almost always
            # where the diagnostic sits.
            tail -n 120 "$log_file" >&2 || true
            echo "--- end captured output ---" >&2
            FAILED+=("$label  (exit=$rc, log=$log_file)")
        fi
    done
done

# ----------------------------------------------------------------------
# Summary.
# ----------------------------------------------------------------------
echo
log_info "summary: $PASSED / $TOTAL passed"

if [ "${#FAILED[@]}" -ne 0 ]; then
    echo
    log_err "failures:"
    for f in "${FAILED[@]}"; do
        printf '  - %s\n' "$f" >&2
    done
    # Make sure failed logs are not auto-deleted before the user can
    # inspect them when invoked interactively.
    if [ -t 1 ]; then
        echo
        log_warn "captured logs preserved at: $TMPDIR_RUN"
        # Cancel the cleanup trap so the user can read them.
        trap - EXIT
    fi
    exit 1
fi

log_ok "all examples runnable"
