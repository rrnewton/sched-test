#!/bin/bash
# check_ub_fidelity.sh — enforce the BPF/C undefined-behaviour policy.
#
# scxsim compiles BPF scheduler source as ordinary userspace C. Where the two
# languages disagree about undefined behaviour, scxsim's semantics can silently
# diverge from the kernel's. This script is the mechanical gate for that
# policy; the reasoning, with kernel-source citations, is in
# ai_docs/BPF_UB_FIDELITY_POLICY.md.
#
# Three checks:
#   1. DIVISION SEMANTICS — crates/scx_simulator/csrc/sim_sigfpe.c must reproduce the verifier's own
#      chk_and_{div,mod,sdiv,smod} results. Runs at -O0 and -O2, because the
#      handler decodes instructions and codegen differs between the two.
#   2. DETECTOR WIRED ON — the uninitialised-read warning must actually fire on
#      a known-bad canary when compiled with the scheduler build's flags.
#      Guards against the flag being silently dropped from schedulers/Makefile.
#   3. WARNING INVENTORY — report every uninitialised-read warning in the real
#      scheduler build, so new ones are visible in CI output rather than
#      scrolling past. Advisory: not a hard failure, because the flag has a
#      known false-positive rate against the bpf_for()/can_loop() macro shape
#      and the source it inspects is vendored upstream scx.
#
# Usage: ./scripts/check_ub_fidelity.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SIM_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CC="${BPF_CLANG:-clang}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

rc=0

# The single flag this policy turns on. Kept in sync with the
# -Wconditional-uninitialized entry in schedulers/Makefile CFLAGS_BASE.
UNINIT_FLAG="-Wconditional-uninitialized"

echo "=== 1/3: BPF division and modulo semantics ==="
for opt in -O0 -O2; do
	bin="$TMP/ub_semantics$opt"
	if ! "$CC" "$opt" -g -Wall -o "$bin" \
		"$SIM_DIR/csrc/tests/sim_bpf_ub_semantics_test.c" \
		"$SIM_DIR/crates/scx_simulator/csrc/sim_sigfpe.c" 2>"$TMP/build.log"; then
		echo "  ERROR: failed to build the semantics test at $opt" >&2
		cat "$TMP/build.log" >&2
		rc=1
		continue
	fi
	echo "  --- $opt ---"
	if "$bin"; then
		echo "  $opt OK"
	else
		echo "  FAIL: BPF division semantics not reproduced at $opt" >&2
		rc=1
	fi
done

echo ""
echo "=== 2/3: uninitialised-read detector is wired on ==="
canary_out="$("$CC" -fsyntax-only $UNINIT_FLAG \
	"$SIM_DIR/csrc/tests/uninit_canary.c" 2>&1)"
if echo "$canary_out" | grep -q "may be uninitialized"; then
	echo "  Canary detected by $UNINIT_FLAG — OK"
else
	echo "  FAIL: $UNINIT_FLAG did not flag csrc/tests/uninit_canary.c." >&2
	echo "  The uninitialised-read detector is not doing anything. Check that" >&2
	echo "  CFLAGS_BASE in schedulers/Makefile still carries $UNINIT_FLAG." >&2
	echo "$canary_out" >&2
	rc=1
fi

# The flag must be in the real compile line, not merely mentioned in a comment,
# so ask make for the expanded value rather than grepping the Makefile text.
# tail -1: the Makefile emits an $(info ...) banner on every invocation, so the
# variable value is the last line of stdout.
effective_cflags="$(make -s -C "$SIM_DIR/schedulers" print-CFLAGS_BASE 2>/dev/null | tail -1)"
if [ -z "$effective_cflags" ]; then
	echo "  FAIL: could not read CFLAGS_BASE from schedulers/Makefile" >&2
	rc=1
elif printf '%s' "$effective_cflags" | grep -qw -- "$UNINIT_FLAG"; then
	echo "  $UNINIT_FLAG present in the effective CFLAGS_BASE — OK"
else
	echo "  FAIL: $UNINIT_FLAG missing from the effective CFLAGS_BASE." >&2
	echo "  CFLAGS_BASE = $effective_cflags" >&2
	rc=1
fi

echo ""
echo "=== 3/3: uninitialised-read inventory in the scheduler build ==="
# Reuse whatever build tree cargo already produced; a from-scratch scheduler
# build here would need BPF_INCLUDE plumbing that build.rs owns.
inventory="$(grep -rho "[^ ]*\.bpf\.c:[0-9]*:[0-9]*: warning: variable '[^']*' may be uninitialized" \
	"$SIM_DIR/target" 2>/dev/null | sort -u)"
if [ -n "$inventory" ]; then
	echo "$inventory" | sed 's/^/  /'
	echo "  ($(echo "$inventory" | wc -l) site(s) — advisory, see ai_docs/BPF_UB_FIDELITY_POLICY.md)"
else
	echo "  No cached scheduler-build warnings found (build first to populate)."
fi

echo ""
if [ "$rc" -eq 0 ]; then
	echo "=== UB fidelity checks passed ==="
else
	echo "=== UB fidelity checks FAILED ===" >&2
fi
exit "$rc"
