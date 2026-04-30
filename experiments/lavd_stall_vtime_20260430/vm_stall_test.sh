#!/bin/bash
# LAVD Stall Reproduction — Inside vng VM (8 CPUs, high contention)
#
# This runs INSIDE the vng VM. Launch with:
#   sudo vng --memory 4G -r /boot/vmlinuz-$(uname -r) --disable-microvm \
#     --qemu-opts "-smp 8" --exec "/path/to/vm_stall_test.sh /path/to/scx_lavd"
#
# Args: $1 = path to scx_lavd binary
#       $2 = duration in minutes (default 10)
#       $3 = number of bg workers (default 800)

set -euo pipefail

LAVD_BIN="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
DURATION_MINS="${2:-10}"
BG_WORKERS="${3:-800}"
SPAWN_INTERVAL_MS=500  # Spawn short-lived processes every 500ms
LOGDIR="/tmp/lavd_stall_test_$(date +%Y%m%d_%H%M%S)"

mkdir -p "$LOGDIR"

echo "============================================="
echo "  LAVD Stall Reproduction Test (in-VM)"
echo "============================================="
echo "  CPUs:           $(nproc)"
echo "  Binary:         $LAVD_BIN"
echo "  BG Workers:     $BG_WORKERS ($(echo "$BG_WORKERS / $(nproc)" | bc) per CPU)"
echo "  Duration:       ${DURATION_MINS} minutes"
echo "  Spawn interval: ${SPAWN_INTERVAL_MS}ms"
echo "  Log dir:        $LOGDIR"
echo "  Kernel:         $(uname -r)"
echo "  sched_ext:      $(cat /sys/kernel/sched_ext/state 2>/dev/null || echo N/A)"
echo ""

# Enable +cpu in cgroup subtree if needed
CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
if [ -n "$CG_ROOT" ]; then
    echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true
fi

# Record baseline
DMESG_BEFORE=$(dmesg | wc -l)
STALL_COUNT=0
SPAWN_COUNT=0
RESTART_COUNT=0
START_TIME=$(date +%s)

start_lavd() {
    $LAVD_BIN --performance --enable-cpu-bw --log-level warn 2>"$LOGDIR/lavd_${RESTART_COUNT}.log" &
    LAVD_PID=$!

    # Wait for enablement
    for i in $(seq 1 20); do
        STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
        if [ "$STATE" = "enabled" ]; then
            return 0
        fi
        sleep 0.5
    done

    echo "ERROR: LAVD failed to enable after 10s (state=$STATE)"
    return 1
}

check_stall() {
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
    if [ "$STATE" != "enabled" ]; then
        ELAPSED=$(($(date +%s) - START_TIME))
        STALL_COUNT=$((STALL_COUNT + 1))

        echo ""
        echo "  ★★★ STALL #$STALL_COUNT at ${ELAPSED}s! state=$STATE ★★★"

        # Capture dmesg
        DMESG_NOW=$(dmesg | wc -l)
        if [ "$DMESG_NOW" -gt "$DMESG_BEFORE" ]; then
            echo "  --- dmesg (last $((DMESG_NOW - DMESG_BEFORE)) lines) ---"
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) | grep -iE "stall|watchdog|sched_ext|lavd|ERROR" | tail -15
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) > "$LOGDIR/dmesg_stall_${STALL_COUNT}.log"
            DMESG_BEFORE=$DMESG_NOW
        fi

        # Record
        echo "[${ELAPSED}s] STALL #$STALL_COUNT state=$STATE spawned=$SPAWN_COUNT" >> "$LOGDIR/stalls.log"

        # Restart
        RESTART_COUNT=$((RESTART_COUNT + 1))
        echo "  Restarting LAVD (attempt #$RESTART_COUNT)..."
        if ! start_lavd; then
            echo "  LAVD restart failed. Continuing without scheduler."
            return 1
        fi
        echo "  LAVD restarted (PID $LAVD_PID)"
        return 0
    fi
    return 0
}

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    kill $LAVD_PID 2>/dev/null || true
    kill $SPAWNER_PID 2>/dev/null || true
    pkill stress-ng 2>/dev/null || true
    sleep 1
}
trap cleanup EXIT

# Start LAVD
echo "Starting LAVD..."
if ! start_lavd; then
    echo "Initial LAVD start failed. Aborting."
    exit 1
fi
echo "  LAVD enabled (PID $LAVD_PID)"

# Start background CPU workers to create contention
echo "Starting $BG_WORKERS background workers..."
stress-ng --cpu $BG_WORKERS --cpu-method matrixprod \
    --timeout $((DURATION_MINS * 60 + 60))s --quiet 2>/dev/null &
STRESS_PID=$!
sleep 2
echo "  Background load running (PID $STRESS_PID)"

# Start the spawner: periodic short-lived processes
echo "Starting periodic spawner..."
(
    while true; do
        # Spawn a batch of short-lived processes
        for j in $(seq 1 5); do
            # Mix of process types:
            /bin/true &   # instant exit
            /bin/sleep 0.001 &  # very brief sleep then exit
        done
        sleep 0.$(printf '%03d' $SPAWN_INTERVAL_MS)
    done
) &
SPAWNER_PID=$!

echo ""
echo "=== Monitoring for stalls (${DURATION_MINS} minutes) ==="
echo ""

END_TIME=$((START_TIME + DURATION_MINS * 60))
LAST_REPORT=0

while [ $(date +%s) -lt $END_TIME ]; do
    sleep 1
    SPAWN_COUNT=$((SPAWN_COUNT + 10))  # ~10 spawns per second

    # Check for stall
    check_stall

    # Periodic status report
    ELAPSED=$(($(date +%s) - START_TIME))
    if [ $((ELAPSED - LAST_REPORT)) -ge 30 ]; then
        LAST_REPORT=$ELAPSED
        echo "  [${ELAPSED}s] spawned≈$SPAWN_COUNT stalls=$STALL_COUNT restarts=$RESTART_COUNT"
    fi
done

# Final summary
echo ""
echo "============================================="
echo "  RESULTS"
echo "============================================="
echo "  Duration:     ${DURATION_MINS} minutes"
echo "  CPUs:         $(nproc)"
echo "  BG Workers:   $BG_WORKERS"
echo "  Spawned:      ≈$SPAWN_COUNT processes"
echo "  Stalls:       $STALL_COUNT"
echo "  Restarts:     $RESTART_COUNT"
echo ""

if [ $STALL_COUNT -gt 0 ]; then
    echo "  ★ STALL REPRODUCED!"
    echo "  See $LOGDIR/ for details"
    echo ""
    echo "  === Stall log ==="
    cat "$LOGDIR/stalls.log" 2>/dev/null || true
else
    echo "  No stalls detected."
fi

echo ""
echo "=== Final dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  No relevant messages"
