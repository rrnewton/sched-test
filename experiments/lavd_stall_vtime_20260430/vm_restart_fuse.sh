#!/bin/bash
# LAVD Restart Cycling + FUSE workload
# Rapidly restart LAVD while squashfuse_ll is running and files are being accessed
# This tests the transition window hypothesis
set -euo pipefail

LAVD="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
SQSH="${2:-/home/newton/work/multi_sched-test/experiments/lavd_stall_vtime_20260430/test.sqsh}"
RESTARTS="${3:-20}"

modprobe fuse 2>/dev/null || true

echo "=== LAVD Restart Cycling + FUSE ==="
echo "CPUs: $(nproc), Restarts: $RESTARTS"

# Start background workers
stress-ng --cpu 400 --cpu-method matrixprod --timeout 600s --quiet 2>/dev/null &

# Keep squashfuse_ll mounted during the entire test
MP="/tmp/persistent_fuse"
mkdir -p "$MP"
squashfuse_ll "$SQSH" "$MP" 2>/dev/null &
FUSE_PID=$!
sleep 1
echo "squashfuse_ll mounted at $MP (PID $FUSE_PID)"

# File access background loop
(
    while true; do
        cat "$MP/file_1" > /dev/null 2>&1 || true
        cat "$MP/file_50" > /dev/null 2>&1 || true
        sleep 0.01
    done
) &
ACCESS_PID=$!

STALLS=0
DMESG_BEFORE=$(dmesg | wc -l)

echo "Starting restart cycling..."
for i in $(seq 1 $RESTARTS); do
    # Start LAVD
    $LAVD --performance --enable-cpu-bw --log-level warn 2>/dev/null &
    LAVD_PID=$!

    # Wait for enable
    for j in $(seq 1 15); do
        STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
        [ "$STATE" = "enabled" ] && break
        sleep 0.5
    done

    if [ "$STATE" != "enabled" ]; then
        echo "  [$i] Failed to enable"
        continue
    fi

    # Run for a random duration (2-10 seconds) while accessing FUSE files
    RUNTIME=$((2 + RANDOM % 9))
    sleep $RUNTIME

    # Check for stall before killing
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        STALLS=$((STALLS + 1))
        echo "  [$i] ★ STALL during run (after ${RUNTIME}s)!"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
    fi

    # Kill LAVD (like production restart)
    kill $LAVD_PID 2>/dev/null || true
    wait $LAVD_PID 2>/dev/null || true

    # Brief pause between restarts
    sleep 1

    # Check state during the gap (no scheduler)
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)

    echo "  [$i/$RESTARTS] runtime=${RUNTIME}s gap_state=$STATE stalls=$STALLS"
done

# Final check with LAVD running
$LAVD --performance --enable-cpu-bw --log-level warn 2>/dev/null &
LAVD_PID=$!
sleep 10

STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
if [ "$STATE" != "enabled" ]; then
    STALLS=$((STALLS + 1))
    echo "★ STALL after final restart!"
fi

echo ""
echo "=== RESULTS ==="
echo "  Restarts: $RESTARTS"
echo "  Stalls:   $STALLS"

kill $ACCESS_PID 2>/dev/null || true
fusermount3 -u "$MP" 2>/dev/null || true
wait $FUSE_PID 2>/dev/null || true
rmdir "$MP" 2>/dev/null || true
pkill stress-ng 2>/dev/null || true
kill $LAVD_PID 2>/dev/null || true

echo "=== dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  none"
