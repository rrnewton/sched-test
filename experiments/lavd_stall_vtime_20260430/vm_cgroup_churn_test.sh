#!/bin/bash
# LAVD Stall Reproduction — Cgroup Churn + High Contention
#
# Tests whether cgroup creation/destruction during high contention
# with --enable-cpu-bw causes stalls. Production has systemd scopes
# being created/destroyed for each XAR mount (squashfuse_ll).
#
# Usage: (inside vng VM)
#   bash vm_cgroup_churn_test.sh /path/to/scx_lavd [duration_mins] [bg_workers]

set -euo pipefail

LAVD_BIN="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
DURATION_MINS="${2:-5}"
BG_WORKERS="${3:-800}"
LOGDIR="/tmp/lavd_cgroup_churn_$(date +%Y%m%d_%H%M%S)"

mkdir -p "$LOGDIR"

echo "============================================="
echo "  LAVD Cgroup Churn Stall Test (in-VM)"
echo "============================================="
echo "  CPUs:       $(nproc)"
echo "  Binary:     $LAVD_BIN"
echo "  Workers:    $BG_WORKERS"
echo "  Duration:   ${DURATION_MINS} min"
echo "  Log dir:    $LOGDIR"
echo ""

CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
if [ -z "$CG_ROOT" ]; then
    echo "ERROR: cgroup2 not mounted"
    exit 1
fi

# Enable cpu controller
echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true

DMESG_BEFORE=$(dmesg | wc -l)
STALL_COUNT=0
CHURN_COUNT=0
RESTART_COUNT=0
START_TIME=$(date +%s)

start_lavd() {
    $LAVD_BIN --performance --enable-cpu-bw --log-level warn 2>"$LOGDIR/lavd_${RESTART_COUNT}.log" &
    LAVD_PID=$!
    for i in $(seq 1 20); do
        STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
        [ "$STATE" = "enabled" ] && return 0
        sleep 0.5
    done
    return 1
}

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    kill $LAVD_PID 2>/dev/null || true
    kill $CHURN_PID 2>/dev/null || true
    pkill stress-ng 2>/dev/null || true
    # Clean up any leftover cgroups
    for d in "$CG_ROOT"/churn_*; do
        [ -d "$d" ] && rmdir "$d" 2>/dev/null || true
    done
    sleep 1
}
trap cleanup EXIT

# Start LAVD
echo "Starting LAVD..."
if ! start_lavd; then
    echo "LAVD failed to start. Aborting."
    exit 1
fi
echo "  LAVD enabled (PID $LAVD_PID)"

# Background CPU load
echo "Starting $BG_WORKERS background workers..."
stress-ng --cpu $BG_WORKERS --cpu-method matrixprod \
    --timeout $((DURATION_MINS * 60 + 60))s --quiet 2>/dev/null &
sleep 2
echo "  Background load running"

# Cgroup churn: rapidly create cgroups, move a short-lived process into them, destroy
echo "Starting cgroup churn..."
(
    SEQ=0
    while true; do
        SEQ=$((SEQ + 1))
        CG_NAME="churn_${SEQ}"
        CG_PATH="$CG_ROOT/$CG_NAME"

        # Create cgroup
        mkdir -p "$CG_PATH" 2>/dev/null || continue

        # Spawn a short-lived process in the cgroup
        # This mimics squashfuse_ll being launched in a systemd scope
        (
            echo $$ > "$CG_PATH/cgroup.procs" 2>/dev/null || true
            # Brief work then exit — like squashfuse_ll mounting then sleeping
            /bin/sleep 0.01
        ) &
        CHILD=$!

        # Let it run briefly
        sleep 0.05

        # Wait for child to finish and destroy cgroup
        # This is the critical race window — cgroup destruction while
        # the child might still be in the BPF scheduler's state
        wait $CHILD 2>/dev/null || true
        rmdir "$CG_PATH" 2>/dev/null || true

        # Rate limit: ~20 churns/sec
        sleep 0.05
    done
) &
CHURN_PID=$!

echo ""
echo "=== Monitoring for stalls (${DURATION_MINS} minutes) ==="
echo ""

END_TIME=$((START_TIME + DURATION_MINS * 60))
LAST_REPORT=0

while [ $(date +%s) -lt $END_TIME ]; do
    sleep 1
    CHURN_COUNT=$((CHURN_COUNT + 20))  # ~20 churns/sec

    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
    if [ "$STATE" != "enabled" ]; then
        ELAPSED=$(($(date +%s) - START_TIME))
        STALL_COUNT=$((STALL_COUNT + 1))

        echo ""
        echo "  ★★★ STALL #$STALL_COUNT at ${ELAPSED}s! state=$STATE ★★★"

        DMESG_NOW=$(dmesg | wc -l)
        if [ "$DMESG_NOW" -gt "$DMESG_BEFORE" ]; then
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) | grep -iE "stall|watchdog|sched_ext|lavd|ERROR" | tail -15
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) > "$LOGDIR/dmesg_stall_${STALL_COUNT}.log"
            DMESG_BEFORE=$DMESG_NOW
        fi

        echo "[${ELAPSED}s] STALL #$STALL_COUNT state=$STATE churns≈$CHURN_COUNT" >> "$LOGDIR/stalls.log"

        RESTART_COUNT=$((RESTART_COUNT + 1))
        echo "  Restarting LAVD..."
        start_lavd || true
    fi

    ELAPSED=$(($(date +%s) - START_TIME))
    if [ $((ELAPSED - LAST_REPORT)) -ge 30 ]; then
        LAST_REPORT=$ELAPSED
        echo "  [${ELAPSED}s] churns≈$CHURN_COUNT stalls=$STALL_COUNT"
    fi
done

echo ""
echo "============================================="
echo "  RESULTS"
echo "============================================="
echo "  Duration:   ${DURATION_MINS} minutes"
echo "  CPUs:       $(nproc)"
echo "  Workers:    $BG_WORKERS"
echo "  Churns:     ≈$CHURN_COUNT cgroups"
echo "  Stalls:     $STALL_COUNT"
echo "  Restarts:   $RESTART_COUNT"
echo ""

if [ $STALL_COUNT -gt 0 ]; then
    echo "  ★ STALL REPRODUCED!"
    cat "$LOGDIR/stalls.log" 2>/dev/null || true
else
    echo "  No stalls detected."
fi

echo ""
echo "=== Final dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  No relevant messages"
