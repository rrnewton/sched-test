#!/bin/bash
# gen_scheduler_fn_coverage.sh — generate a per-scheduler FUNCTION-level
# test-coverage report for the simple / lavd / cosmos schedulers.
#
# "A function has a test" is defined rigorously and reproducibly as: the
# clang source-based coverage execution count for that function is > 0 after
# running the full `cargo test` suite. This is coarser than line/branch
# coverage (a covered function may still have uncovered branches) — see
# `ai_docs/COVERAGE_AUDIT_20260722.md` for the line/branch breakdown.
#
# Prerequisite: a merged coverage profile produced by `./coverage.sh
# --keep-profraw` (leaves coverage-out/merged.profdata + the instrumented
# schedulers_cov/libscx_*.so objects in the cargo target tree).
#
# Usage:
#   ./scripts/gen_scheduler_fn_coverage.sh > ai_docs/OVERNIGHT_COVERAGE_REPORT.md
#
# The report is fully regenerated from the profile each run; do not hand-edit
# the generated tables.
set -euo pipefail

cd "$(dirname "$0")/.."          # scx-sim/
PROF="coverage-out/merged.profdata"

if [[ ! -f "$PROF" ]]; then
    echo "ERROR: $PROF not found. Run ./coverage.sh --keep-profraw first." >&2
    exit 1
fi
command -v llvm-cov >/dev/null || { echo "ERROR: llvm-cov not found." >&2; exit 1; }
command -v jq >/dev/null || { echo "ERROR: jq not found." >&2; exit 1; }

# scheduler -> body-source filename regex (isolates each scheduler's OWN
# .bpf.c body, excluding shared libs (scx/lib/*), wrappers, headers, and the
# other schedulers). cosmos's body is the sed-generated cosmos_main_patched.c,
# which coverage.sh's default report deliberately excludes — we lift that here.
declare -A SCHED_RE=(
    [simple]='schedulers/simple/scx_simple\.bpf\.c'
    [lavd]='scx_lavd/src/bpf/'
    [cosmos]='cosmos_main_patched\.c'
)
SCHEDS=(simple lavd cosmos)

find_so() { find target -path "*/schedulers_cov/libscx_$1.so" 2>/dev/null | head -1; }

# Emit the deduped per-function JSON array for one scheduler:
#   [ { "name", "file", "count" }, ... ]  (one entry per function, max count)
sched_fn_json() {
    local sched="$1" so re
    so="$(find_so "$sched")"
    re="${SCHED_RE[$sched]}"
    [[ -n "$so" ]] || { echo "ERROR: libscx_$sched.so not found." >&2; exit 1; }
    llvm-cov export "$so" -instr-profile="$PROF" -format=text 2>/dev/null | jq --arg re "$re" '
        [ .data[0].functions[]
          | select(.filenames[0] | test($re))
          | { name: .name, file: (.filenames[0] | sub(".*/"; "")), count: .count } ]
        | group_by(.name + "@" + .file)
        | map({ name: .[0].name, file: .[0].file, count: (map(.count) | max) })
    '
}

TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git rev-parse --short HEAD)"
GITDEPTH="$(git rev-list --count HEAD)"
SCX_SHA="$(git -C .. rev-parse --short HEAD:scx 2>/dev/null || echo '?')"

# Stash each scheduler's function JSON in a temp dir.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
for s in "${SCHEDS[@]}"; do sched_fn_json "$s" > "$TMP/$s.json"; done

col() { local s="$1"; shift; jq "$@" "$TMP/$s.json"; }   # col <sched> [jq-flags] <jq-expr>

# ---- Header ----------------------------------------------------------------
cat <<EOF
# Scheduler Function-Level Test-Coverage Report

**Generated:** $TS (by \`scripts/gen_scheduler_fn_coverage.sh\`)
**Commit:** integration \`$COMMIT\` (gitdepth $GITDEPTH), scx submodule \`$SCX_SHA\`
**Profile:** \`coverage-out/merged.profdata\` from \`./coverage.sh --keep-profraw\`
(full \`cargo test --all -- --test-threads=1\` suite, clang source-based coverage).

## What "covered" means here

A scheduler C function counts as **tested** iff its clang coverage **execution
count is > 0** after the whole test suite runs — i.e. some test actually drives
the kernel/BPF substrate into that function. This is **function granularity**:
a covered function may still contain uncovered *branches/lines*. For the
line/branch breakdown and the specific uncovered code paths, see the companion
[\`COVERAGE_AUDIT_20260722.md\`](COVERAGE_AUDIT_20260722.md).

Scope is each scheduler's **own** BPF body only:
- simple → \`schedulers/simple/scx_simple.bpf.c\`
- lavd → \`scx/scheds/rust/scx_lavd/src/bpf/*.bpf.c\`
- cosmos → \`schedulers/cosmos/cosmos_main_patched.c\` (the sed-generated patched
  \`main.bpf.c\`; \`coverage.sh\`'s default filter hides it — this report lifts
  that exclusion, so cosmos is no longer under-reported)

Shared libraries linked into every scheduler (\`scx/lib/cgroup_bw.bpf.c\`,
\`ravg.bpf.c\`) and the scxsim wrappers are **out of scope** here (they are not
scheduler decision logic).

## Per-scheduler summary

| scheduler | total functions | tested (count>0) | untested | function coverage |
|-----------|-----------------|------------------|----------|-------------------|
EOF

# ---- Summary table ---------------------------------------------------------
TOT_ALL=0; COV_ALL=0
for s in "${SCHEDS[@]}"; do
    t=$(col "$s" 'length')
    c=$(col "$s" '[.[]|select(.count>0)]|length')
    u=$((t - c))
    pct=$(awk "BEGIN{ if($t>0) printf \"%.1f\", $c*100/$t; else print \"0.0\" }")
    printf '| %s | %d | %d | %d | %s%% |\n' "$s" "$t" "$c" "$u" "$pct"
    TOT_ALL=$((TOT_ALL + t)); COV_ALL=$((COV_ALL + c))
done
PCT_ALL=$(awk "BEGIN{ if($TOT_ALL>0) printf \"%.1f\", $COV_ALL*100/$TOT_ALL; else print \"0.0\" }")
printf '| **all three** | **%d** | **%d** | **%d** | **%s%%** |\n' \
    "$TOT_ALL" "$COV_ALL" "$((TOT_ALL - COV_ALL))" "$PCT_ALL"

# ---- Per-scheduler detail --------------------------------------------------
for s in "${SCHEDS[@]}"; do
    t=$(col "$s" 'length'); c=$(col "$s" '[.[]|select(.count>0)]|length'); u=$((t - c))
    echo ""
    echo "## $s — $c/$t functions tested"
    echo ""
    echo "### Per-source-file breakdown"
    echo ""
    echo "| source file | total | tested | untested |"
    echo "|-------------|-------|--------|----------|"
    col "$s" -r '
        group_by(.file) | sort_by(.[0].file) | .[]
        | "| \(.[0].file) | \(length) | \([.[]|select(.count>0)]|length) | \([.[]|select(.count==0)]|length) |"'
    if [[ "$u" -gt 0 ]]; then
        echo ""
        echo "### Untested functions ($u) — no test drives these"
        echo ""
        echo "| function | source file |"
        echo "|----------|-------------|"
        col "$s" -r '[.[]|select(.count==0)] | sort_by(.file+.name) | .[] | "| `\(.name)` | \(.file) |"'
    else
        echo ""
        echo "_All $t functions are exercised by at least one test._"
    fi
done

cat <<'EOF'

## Reproduce

```bash
cd scx-sim
./coverage.sh --keep-profraw          # instrumented build + full test suite + merge
./scripts/gen_scheduler_fn_coverage.sh > ai_docs/OVERNIGHT_COVERAGE_REPORT.md
```

## Caveats

- **Function vs line/branch.** 100% function coverage does NOT mean 100%
  line/branch coverage. E.g. `simple` executes all its functions but still has
  uncovered `fifo_sched=true` branches (see the companion audit). Function
  coverage is a floor: an untested function is a hard gap; a tested function may
  still hide untested paths.
- **Untested ≠ dead code.** Some untested functions are unreachable under the
  current scxsim substrate (e.g. `SEC("syscall")` init hooks not invoked by the
  harness, or futex raw-tracepoints the substrate does not yet deliver). Those
  are substrate gaps to file, not merely missing tests — the companion audit
  and the linked minibeads issues distinguish them.
- Counts come straight from `llvm-cov export`; re-running regenerates every
  number. Do not hand-edit the tables above.
EOF
