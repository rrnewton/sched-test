#!/usr/bin/env bash
#
# The scheduler<->simulator connection gate.
#
# WHAT THIS DECIDES
#
# Whether the real scheduler code still connects to scx-sim well enough for the
# downstream scheduler tests to mean anything. If this fails, those tests are
# not run at all: a suite that cannot load the scheduler produces failures that
# describe the loader, not the scheduler.
#
# WHY IT ALSO ASSIGNS BLAME, AND WHY THAT IS THE HARDER HALF
#
# `sched-test` submodules `scx`. The schedulers `#include` the genuine upstream
# BPF C from that submodule and compile it as userspace C. When the pin moves
# forward, the scheduler can gain code scx-sim does not yet support. The
# connection then breaks and **scx-sim is at fault, not the scheduler and not
# whoever bumped the pin.**
#
# If a pin bump gets a red X that reads like "your import is broken", people
# learn to ignore this gate, and an ignored gate is worse than no gate — it
# green-lights nothing while costing CI minutes and attention. So every failure
# path below names the likely owner explicitly.
#
# HOW BLAME IS DECIDED: BY FAILURE STAGE FIRST
#
# The stage a failure occurs at is stronger evidence than what the commit
# touched, because it points at the mechanism rather than at a correlation:
#
#   BUILD   the scheduler's C would not compile against scx-sim's shims
#           -> scx-sim is missing something the scheduler now needs
#   LOAD    the .so has an unresolved symbol under RTLD_NOW
#           -> scx-sim does not export a kfunc/helper the scheduler now calls
#   RUN     it loaded and inited but did not schedule, or errored
#           -> the connection is live but broken; needs a human
#
# What the commit touched is reported too, as corroboration, never as the
# primary signal.
#
# Usage: scx-sim/scripts/scheduler_connection_gate.sh
# Run from anywhere; it locates its own scx-sim root.

set -uo pipefail

SCXSIM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$SCXSIM_DIR/.." && pwd)"
cd "$SCXSIM_DIR"

TEST_TARGET="scheduler_connection_smoke"
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

# GitHub Actions sinks, harmless no-ops when running locally.
SUMMARY="${GITHUB_STEP_SUMMARY:-/dev/null}"
annotate() { printf '::%s title=%s::%s\n' "$1" "$2" "$3"; }

say() { printf '%s\n' "$*"; }
summary() { printf '%s\n' "$*" >>"$SUMMARY"; }

# ---------------------------------------------------------------------------
# Corroborating evidence: what did this change actually touch?
# ---------------------------------------------------------------------------
#
# Reported to help a reader, NOT used to decide the verdict. A pin bump that
# breaks the build is scx-sim's problem whether or not the diff is "just" the
# gitlink, and a scx-sim commit can break the connection without touching the
# pin at all.
changed_paths() {
    local base
    base="$(git -C "$REPO_DIR" rev-parse HEAD^ 2>/dev/null)" || return 0
    git -C "$REPO_DIR" diff --name-only "$base" HEAD 2>/dev/null || true
}

describe_change() {
    local paths pin_moved=no scxsim_moved=no
    paths="$(changed_paths)"
    [ -z "$paths" ] && { echo "unknown (no parent commit to diff against)"; return; }
    grep -qx 'scx' <<<"$paths" && pin_moved=yes
    grep -q '^scx-sim/' <<<"$paths" && scxsim_moved=yes
    if [ "$pin_moved" = yes ] && [ "$scxsim_moved" = no ]; then
        echo "the scx pin moved and scx-sim did not"
    elif [ "$pin_moved" = yes ]; then
        echo "both the scx pin and scx-sim changed"
    elif [ "$scxsim_moved" = yes ]; then
        echo "scx-sim changed, the scx pin did not"
    else
        echo "neither the scx pin nor scx-sim changed in this commit"
    fi
}

PIN="$(git -C "$REPO_DIR" rev-parse --short HEAD:scx 2>/dev/null || echo unknown)"
CONTEXT="$(describe_change)"

# The message every drift failure must carry. Written once so the BUILD and
# LOAD paths cannot drift apart in wording.
blame_scxsim() {
    local stage="$1" detail="$2"
    say ""
    say "============================================================"
    say "SCHEDULER<->SIMULATOR CONNECTION GATE: FAILED at $stage"
    say "============================================================"
    say ""
    say "  >>> scx-sim is BEHIND the scheduler. <<<"
    say ""
    say "This is NOT a defect in the scheduler source, and NOT a defect in"
    say "the scx import or in whoever bumped the pin. The pinned scx revision"
    say "contains scheduler code that scx-sim does not yet support."
    say ""
    say "  scx pin:        $PIN"
    say "  this change:    $CONTEXT"
    say "  failing stage:  $stage"
    say ""
    say "TO FIX: update scx-sim to support the scheduler at this pin."
    say "DO NOT: revert the pin bump or treat the import as broken, unless"
    say "        you have separately established the scheduler itself is wrong."
    say ""
    say "$detail"
    summary "## ❌ scheduler↔sim connection gate: FAILED at $stage"
    summary ""
    summary "**scx-sim is BEHIND the scheduler.** Not a defect in the scheduler"
    summary "source or in the scx import."
    summary ""
    summary "| | |"
    summary "|---|---|"
    summary "| scx pin | \`$PIN\` |"
    summary "| this change | $CONTEXT |"
    summary "| failing stage | $stage |"
    summary ""
    summary "Downstream scheduler tests were **skipped, not failed** — they"
    summary "cannot produce a meaningful result on a broken connection."
    annotate error "scx-sim is behind the scheduler ($stage)" \
        "Update scx-sim to support the scheduler at scx pin $PIN. Do not treat this as a broken import."
}

# ---------------------------------------------------------------------------
# STAGE 1 — BUILD. Compiles the real upstream scheduler C against scx-sim's
# substrate and links the six .so.
# ---------------------------------------------------------------------------
say "=== STAGE 1/2: building the schedulers (real upstream BPF C as userspace C) ==="
if ! cargo test --no-run -p scx_simulator --test "$TEST_TARGET" >"$LOG" 2>&1; then
    tail -60 "$LOG"
    blame_scxsim "BUILD" \
"The scheduler's C did not compile against scx-sim's substrate. Typically a
new kernel struct field, helper, or header the simulator does not provide yet.
Full build log above."
    exit 1
fi
say "    ok"

# ---------------------------------------------------------------------------
# STAGE 2 — LOAD + RUN. dlopen(RTLD_NOW) each .so, run ops.init and a small
# workload, require observed scheduling.
# ---------------------------------------------------------------------------
say ""
say "=== STAGE 2/2: loading each scheduler and running a smoke workload ==="
if ! cargo test -p scx_simulator --test "$TEST_TARGET" -- --nocapture >"$LOG" 2>&1; then
    tail -80 "$LOG"
    if grep -q "undefined symbol" "$LOG"; then
        blame_scxsim "LOAD" \
"A scheduler .so has an unresolved symbol under RTLD_NOW: the scheduler calls
a kfunc or helper that scx-sim does not export. See 'undefined symbol' above."
    else
        # Loaded and inited, but did not behave. This one is genuinely
        # ambiguous, so it says so rather than guessing an owner.
        say ""
        say "============================================================"
        say "SCHEDULER<->SIMULATOR CONNECTION GATE: FAILED at RUN"
        say "============================================================"
        say ""
        say "The schedulers built and loaded, but at least one did not schedule"
        say "as expected. Unlike a BUILD or LOAD failure this does NOT by itself"
        say "identify the owner:"
        say ""
        say "  * if the scx pin moved, suspect scx-sim being behind first;"
        say "  * if only scx-sim changed, suspect that change first."
        say ""
        say "  scx pin:     $PIN"
        say "  this change: $CONTEXT"
        say ""
        summary "## ❌ scheduler↔sim connection gate: FAILED at RUN"
        summary ""
        summary "Schedulers built and loaded but did not schedule as expected."
        summary "**Owner is ambiguous at this stage** — see the log."
        summary ""
        summary "| | |"
        summary "|---|---|"
        summary "| scx pin | \`$PIN\` |"
        summary "| this change | $CONTEXT |"
        annotate error "scheduler<->sim connection broken at RUN" \
            "Built and loaded but did not schedule. scx pin $PIN; $CONTEXT."
    fi
    exit 1
fi

grep -E "^(scheduler<->simulator connection OK|  [a-z]+ +exit=)" "$LOG" || true
say ""
say "=== connection gate PASSED — downstream scheduler tests may run ==="
summary "## ✅ scheduler↔sim connection gate: PASSED"
summary ""
summary "All six schedulers built from the real upstream BPF C at scx pin"
summary "\`$PIN\`, loaded under \`RTLD_NOW\`, and scheduled observable work."
summary ""
summary '```'
grep -E "^  [a-z]+ +exit=" "$LOG" >>"$SUMMARY" || true
summary '```'
summary ""
summary "Covers the **scheduler-C-compiled-natively** path only. No kernel, no"
summary "BPF verifier, no real \`sched_ext\`. See the module docs in"
summary "\`tests/$TEST_TARGET.rs\` for what this does and does not catch."
exit 0
