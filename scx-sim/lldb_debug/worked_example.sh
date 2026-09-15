#!/usr/bin/env bash
# Worked example: scxsim debug helpers exercised against the LAVD Bug-1
# (cgroup-bandwidth runnable-task stall) reproducer.
#
# Phase A — auto-print four formatter targets across one simulation run,
#           letting each breakpoint disable itself after first hit.
# Phase B — keep the process stopped at one breakpoint and invoke the
#           `bug1_diagnose` custom command.
#
# Driver: lldb (the same single driver chosen in
# experiments/agent_debugger_setup_20260512/README.md). gdb 9.1 on this
# host crashes on the scxsim binary.
#
# Outputs (relative to the dbg-scripts worktree root):
#   scx-sim/lldb_debug/phaseA.transcript.txt
#   scx-sim/lldb_debug/phaseB.transcript.txt
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SIMCRATE="${REPO_ROOT}/scx-sim/crates/scx_simulator"
DBG_DIR="${REPO_ROOT}/scx-sim/lldb_debug"

# By default reuse the prebuilt scxsim binary from the primary integration
# checkout (built from the same simulator.v6 SHA we are sitting on).
PARENT_ROOT="${PARENT_ROOT:-$(cd "${REPO_ROOT}/../../.." && pwd)}"
SCXSIM="${SCXSIM:-${PARENT_ROOT}/sched-test1/scx-sim/target/release/scxsim}"

if [[ ! -x "${SCXSIM}" ]]; then
  echo "FATAL: scxsim binary not found at ${SCXSIM}" >&2
  echo "Build with: (cd ${REPO_ROOT}/scx-sim && CARGO_PROFILE_RELEASE_DEBUG=2 cargo build --release -p scx_simulator --bin scxsim)" >&2
  exit 1
fi
if ! command -v lldb >/dev/null 2>&1; then
  echo "FATAL: lldb not on PATH" >&2
  exit 1
fi

PHASE_A="$(mktemp --suffix=.lldb)"
PHASE_B="$(mktemp --suffix=.lldb)"
trap 'rm -f "${PHASE_A}" "${PHASE_B}"' EXIT

# Breakpoint line numbers track the simulator.v6 tip at the time the
# example was last refreshed. Re-run after large engine.rs / cgroup_bw.rs
# refactors and update if a breakpoint resolves to an unexpected location
# (visible in the per-bp `where = ...` line in the transcript).
cat > "${PHASE_A}" << 'EOF'
breakpoint set --file cgroup_bw.rs --line 78
breakpoint set --file cgroup_bw.rs --line 219
breakpoint set --file dsq.rs --line 217
breakpoint set --file engine.rs --line 1266
breakpoint command add 1 -F lldb_lavd_formatters.print_once_and_disable
breakpoint command add 2 -F lldb_lavd_formatters.print_once_and_disable
breakpoint command add 3 -F lldb_lavd_formatters.print_once_and_disable
breakpoint command add 4 -F lldb_lavd_formatters.print_once_and_disable
run
breakpoint list
quit
EOF

cat > "${PHASE_B}" << 'EOF'
breakpoint set --file engine.rs --line 1266
run
script print("=== bug1_diagnose at first check_watchdog stop ===")
bug1_diagnose
script print("=== frame variable task (SimTask formatter) ===")
frame variable task
quit
EOF

REPRO_ARGS=(
  --no-disable-aslr run
  tests/fixtures/h6/bug1_canonical.json
  --config tests/fixtures/h6/bug1_canonical.toml
  --watchdog 80ms -s lavd --cpus 4 --duration 500ms
)

cd "${SIMCRATE}"

echo ">>> Phase A: 4 breakpoints, formatters auto-printed and disabled"
lldb -b \
  -o "command source ${DBG_DIR}/init.lldb" \
  -o "command script import ${DBG_DIR}/lldb_lavd_formatters.py" \
  -s "${PHASE_A}" \
  -- "${SCXSIM}" "${REPRO_ARGS[@]}" \
  > "${DBG_DIR}/phaseA.transcript.txt" 2>&1
echo "Phase A done. Formatter hits:"
grep -E "^=== HIT|^  [a-z_]+:.*=" "${DBG_DIR}/phaseA.transcript.txt" | head -40

echo ""
echo ">>> Phase B: stop at check_watchdog and invoke bug1_diagnose"
lldb -b \
  -o "command source ${DBG_DIR}/init.lldb" \
  -o "command script import ${DBG_DIR}/lldb_lavd_formatters.py" \
  -s "${PHASE_B}" \
  -- "${SCXSIM}" "${REPRO_ARGS[@]}" \
  > "${DBG_DIR}/phaseB.transcript.txt" 2>&1
echo "Phase B done. bug1_diagnose output:"
sed -n '/bug1_diagnose at first check/,/frame variable task/p' "${DBG_DIR}/phaseB.transcript.txt" | head -30

echo ""
echo "Outputs in: ${DBG_DIR}/"
