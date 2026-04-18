#!/usr/bin/env bash
# demo_blind_synthesis.sh — Full blind synthesis pipeline demo
#
# This script demonstrates repm's blind synthesis: given ONLY scheduling
# trace data (rt-app logs), reconstruct a workload config that reproduces
# the observed scheduling behavior.
#
# Usage:
#   cd sched-test1/repm
#   bash scripts/demo_blind_synthesis.sh [--real | --sim]
#
#   --real  Use real ucache rt-app logs from experiments/0.10_v2 (default)
#   --sim   Use a simple 2-thread synthetic workload through scxsim
#
# Prerequisites:
#   - repm built: cargo build --release (or debug)
#   - scxsim built (for --sim mode): ../scx-sim/target/release/scxsim
#   - Real rt-app logs exist (for --real mode)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPM_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPM="$REPM_DIR/target/release/repm"
SCXSIM="$REPM_DIR/../scx-sim/target/release/scxsim"

# Fall back to debug build
if [ ! -f "$REPM" ]; then
    REPM="$REPM_DIR/target/debug/repm"
fi

if [ ! -f "$REPM" ]; then
    echo "ERROR: repm not built. Run: cd $REPM_DIR && cargo build --release"
    exit 1
fi

MODE="${1:---real}"
WORKDIR=$(mktemp -d /tmp/repm_blind_demo.XXXXXX)
trap "echo ''; echo 'Workdir preserved at: $WORKDIR'" EXIT

echo "╔══════════════════════════════════════════════════════╗"
echo "║     repm — Blind Synthesis Pipeline Demo             ║"
echo "╚══════════════════════════════════════════════════════╝"
echo ""
echo "Mode:    $MODE"
echo "Workdir: $WORKDIR"
echo "repm:    $REPM"
echo ""

T_START=$(date +%s)

# ───────────────────────────────────────────────────────────
# STEP 1: Get trace data
# ───────────────────────────────────────────────────────────

if [ "$MODE" = "--sim" ]; then
    echo "━━━ Step 1/4: Create synthetic ground truth and run through scxsim ━━━"
    echo ""

    if [ ! -f "$SCXSIM" ]; then
        echo "ERROR: scxsim not built. Run: cd $REPM_DIR/../scx-sim && cargo build --release"
        exit 1
    fi

    # Create a simple 2-thread ground truth config
    cat > "$WORKDIR/ground_truth.json" << 'GTEOF'
{
  "global": {"default_policy": "SCHED_OTHER", "duration": 10},
  "tasks": {
    "fast_waker": {"run": 100, "sleep": 4900, "loop": -1, "cpus": [0,1,2,3]},
    "slow_hog":   {"run": 50000, "sleep": 50000, "loop": -1, "cpus": [0,1,2,3]}
  }
}
GTEOF

    echo "Ground truth config:"
    echo "  fast_waker: run=100µs, sleep=4900µs (200 Hz)"
    echo "  slow_hog:   run=50ms,  sleep=50ms   (10 Hz)"
    echo ""

    echo "Running ground truth through scxsim..."
    "$SCXSIM" run "$WORKDIR/ground_truth.json" \
        --scheduler lavd --cpus 4 --seed 42 \
        --end-time 10s --warmup-ms 1000 --verbose-summary \
        2>"$WORKDIR/trace_stderr.txt" 1>"$WORKDIR/trace_stdout.txt"
    cat "$WORKDIR/trace_stdout.txt" "$WORKDIR/trace_stderr.txt" > "$WORKDIR/trace.txt"

    TRACE_INPUT="$WORKDIR/trace.txt"
    CORES=4
    DURATION=10
    echo "  scxsim complete → $TRACE_INPUT"

elif [ "$MODE" = "--real" ]; then
    echo "━━━ Step 1/4: Locate real ucache rt-app log data ━━━"
    echo ""

    REAL_LOGS="$REPM_DIR/../../ucache_reproducer/experiments/0.10_v2/data/rtapp_pinned/lavd_baseline_level1_nice0_rep1/logs"

    if [ ! -d "$REAL_LOGS" ]; then
        echo "ERROR: Real rt-app logs not found at: $REAL_LOGS"
        echo "Try --sim mode instead, or check the path."
        exit 1
    fi

    LOG_COUNT=$(ls "$REAL_LOGS"/*.log 2>/dev/null | wc -l)
    echo "  Source: experiments/0.10_v2 (LAVD baseline, rep1)"
    echo "  Log files: $LOG_COUNT"
    echo "  Ground truth: 8 cache_worker + 56 background_hog + 4 irq_gen + 2 ssd_io = 70 threads"

    TRACE_INPUT="$REAL_LOGS"
    CORES=12
    DURATION=30

else
    echo "Usage: $0 [--real | --sim]"
    exit 1
fi

echo ""

# ───────────────────────────────────────────────────────────
# STEP 2: Blind synthesis (the key step!)
# ───────────────────────────────────────────────────────────

echo "━━━ Step 2/4: Blind synthesis — infer config from trace data ━━━"
echo ""
echo "  Command: repm gen-config --from-trace <trace> --verbose --format sim"
echo "  (The pipeline has NO access to the original config)"
echo ""

"$REPM" gen-config \
    --from-trace "$TRACE_INPUT" \
    --format sim \
    --verbose \
    --cores "$CORES" \
    --duration "$DURATION" \
    -o "$WORKDIR/synthesized.json" 2>&1

echo ""

# ───────────────────────────────────────────────────────────
# STEP 3: Show results
# ───────────────────────────────────────────────────────────

echo "━━━ Step 3/4: Synthesized config summary ━━━"
echo ""

python3 -c "
import json, sys
with open('$WORKDIR/synthesized.json') as f:
    config = json.load(f)
tasks = config['tasks']
classes = {}
for name, t in tasks.items():
    key = (t['run'], t['sleep'])
    if key not in classes:
        classes[key] = []
    classes[key].append(name)
print(f'  Total tasks: {len(tasks)}')
print(f'  Thread classes: {len(classes)}')
print()
for (run, sleep), members in sorted(classes.items(), key=lambda x: -x[0][0]):
    freq = 1_000_000 / (run + sleep) if (run + sleep) > 0 else 0
    print(f'  run={run:>6}µs  sleep={sleep:>6}µs  freq={freq:>6.0f}Hz  count={len(members):>3}  (e.g., {members[0]})')
"

echo ""

# ───────────────────────────────────────────────────────────
# STEP 4: Run synthesized config through scxsim (if available)
# ───────────────────────────────────────────────────────────

if [ -f "$SCXSIM" ]; then
    echo "━━━ Step 4/4: Validate — run synthesized config through scxsim ━━━"
    echo ""

    "$SCXSIM" run "$WORKDIR/synthesized.json" \
        --scheduler lavd --cpus "$CORES" --seed 42 \
        --end-time "${DURATION}s" --warmup-ms 1000 --verbose-summary \
        2>"$WORKDIR/synth_stderr.txt" 1>"$WORKDIR/synth_stdout.txt" || true

    # Show per-task comparison if we have ground truth
    if [ -f "$WORKDIR/trace.txt" ]; then
        echo "  Per-task comparison (ground truth vs synthesized):"
        python3 -c "
import os

def parse_summary(path):
    tasks = {}
    current = None
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line.startswith('Task PID='):
                current = line.split('=')[1].rstrip(':')
                tasks[current] = {}
            elif current and 'Run duration:' in line:
                val = line.split(':')[1].strip().split('ms')[0].strip()
                tasks[current]['run_ms'] = float(val)
            elif current and 'Inter-arrival:' in line:
                val = line.split(':')[1].strip().split('ms')[0].strip()
                tasks[current]['ia_ms'] = float(val)
    return tasks

gt_file = '$WORKDIR/trace.txt'
cat_stdout = '$WORKDIR/synth_stdout.txt'
cat_stderr = '$WORKDIR/synth_stderr.txt'

# Combine
import tempfile
combined = tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False)
for f in [cat_stdout, cat_stderr]:
    if os.path.exists(f):
        combined.write(open(f).read())
combined.close()

gt = parse_summary(gt_file)
syn = parse_summary(combined.name)
os.unlink(combined.name)

if gt and syn:
    import math
    ratios = []
    for (gk, gv), (sk, sv) in zip(
        sorted(gt.items(), key=lambda x: x[1].get('run_ms', 0)),
        sorted(syn.items(), key=lambda x: x[1].get('run_ms', 0))
    ):
        gr = gv.get('run_ms', 0)
        sr = sv.get('run_ms', 0)
        r = max(gr/sr, sr/gr) if gr > 0 and sr > 0 else 999
        ratios.append(r)
        print(f'    GT PID={gk:>2} run={gr:>8.3f}ms  vs  SYN PID={sk:>2} run={sr:>8.3f}ms  → {r:.3f}x')
    geomean = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
    print(f'')
    print(f'    Geomean: {geomean:.4f}x ({\"PASS\" if geomean < 2.0 else \"FAIL\"})')
else:
    print('    (comparison skipped — run scxsim for ground truth first)')
" 2>/dev/null || echo "  (Python comparison skipped)"
    fi
else
    echo "━━━ Step 4/4: Validate — skipped (scxsim not available) ━━━"
    echo ""
    echo "  To validate, build scxsim and run:"
    echo "    scxsim run $WORKDIR/synthesized.json --scheduler lavd --cpus $CORES --verbose-summary"
fi

echo ""

# ───────────────────────────────────────────────────────────
# Summary
# ───────────────────────────────────────────────────────────

T_END=$(date +%s)
T_ELAPSED=$((T_END - T_START))

echo "╔══════════════════════════════════════════════════════╗"
echo "║                    Summary                           ║"
echo "╚══════════════════════════════════════════════════════╝"
echo ""
echo "  Pipeline:  trace data → repm gen-config --from-trace → config"
echo "  Input:     $TRACE_INPUT"
echo "  Output:    $WORKDIR/synthesized.json"
echo "  Time:      ${T_ELAPSED}s"
echo "  Tokens:    0 (pure algorithmic synthesis)"
echo ""
echo "  The synthesized config can be used with:"
echo "    - scxsim (simulator):  scxsim run synthesized.json --scheduler lavd"
echo "    - rt-app (bare metal): sudo rt-app synthesized.json"
echo ""
echo "  To convert to phased rt-app format:"
echo "    repm gen-config --from-trace <trace> --format rtapp -o workload.json"
echo ""
