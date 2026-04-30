#!/bin/bash
# LAVD Stall Reproduction — FUSE-Based (squashfuse_ll)
#
# Mimics production behavior: periodically mount squashfs images
# via squashfuse_ll, access files, unmount. This creates the exact
# kernel wake path (VFS → FUSE → wake daemon) that production uses.
#
# Usage (inside vng VM):
#   bash vm_fuse_stall_test.sh /path/to/scx_lavd [duration_mins] [bg_workers]

set -euo pipefail

LAVD_BIN="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
DURATION_MINS="${2:-10}"
BG_WORKERS="${3:-600}"
SQSH="${4:-/home/newton/work/multi_sched-test/experiments/lavd_stall_vtime_20260430/test.sqsh}"
LOGDIR="/tmp/lavd_fuse_stall_$(date +%Y%m%d_%H%M%S)"

mkdir -p "$LOGDIR"

echo "============================================="
echo "  LAVD FUSE Stall Test (squashfuse_ll)"
echo "============================================="
echo "  CPUs:       $(nproc)"
echo "  Binary:     $LAVD_BIN"
echo "  Workers:    $BG_WORKERS"
echo "  Duration:   ${DURATION_MINS} min"
echo "  Squashfs:   $SQSH"
echo "  Log dir:    $LOGDIR"
echo ""

# Check prerequisites
if ! which squashfuse_ll >/dev/null 2>&1; then
    echo "ERROR: squashfuse_ll not found"
    exit 1
fi
if [ ! -f "$SQSH" ]; then
    echo "ERROR: squashfs image not found: $SQSH"
    exit 1
fi

# Load fuse module
modprobe fuse 2>/dev/null || true

# Enable cgroup cpu controller
CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
if [ -n "$CG_ROOT" ]; then
    echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true
fi

DMESG_BEFORE=$(dmesg | wc -l)
STALL_COUNT=0
MOUNT_COUNT=0
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
    # Unmount any leftover FUSE mounts
    for mp in /tmp/fuse_mount_*; do
        fusermount3 -u "$mp" 2>/dev/null || true
        rmdir "$mp" 2>/dev/null || true
    done
    kill $LAVD_PID 2>/dev/null || true
    pkill stress-ng 2>/dev/null || true
    pkill squashfuse_ll 2>/dev/null || true
    sleep 1
}
trap cleanup EXIT

# Start LAVD
echo "Starting LAVD..."
if ! start_lavd; then
    echo "LAVD failed to start."
    exit 1
fi
echo "  LAVD enabled (PID $LAVD_PID)"

# Start background CPU load
echo "Starting $BG_WORKERS background workers..."
stress-ng --cpu $BG_WORKERS --cpu-method matrixprod \
    --timeout $((DURATION_MINS * 60 + 60))s --quiet 2>/dev/null &
sleep 2
echo "  Background load running"

echo ""
echo "=== FUSE Mount/Access/Unmount Loop ==="
echo ""

END_TIME=$((START_TIME + DURATION_MINS * 60))
LAST_REPORT=0

while [ $(date +%s) -lt $END_TIME ]; do
    MOUNT_COUNT=$((MOUNT_COUNT + 1))
    MP="/tmp/fuse_mount_${MOUNT_COUNT}"
    mkdir -p "$MP"

    # Mount squashfs via squashfuse_ll (creates the FUSE daemon)
    squashfuse_ll "$SQSH" "$MP" 2>/dev/null &
    FUSE_PID=$!
    sleep 0.1  # Let it initialize

    # Access files to trigger FUSE requests (wakes the daemon)
    for f in "$MP"/file_*; do
        [ -f "$f" ] && cat "$f" > /dev/null 2>&1
        break  # Just read one file per mount cycle
    done

    # Brief pause then unmount (triggers cleanup)
    sleep 0.05
    fusermount3 -u "$MP" 2>/dev/null || true
    wait $FUSE_PID 2>/dev/null || true
    rmdir "$MP" 2>/dev/null || true

    # Check for stall
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
    if [ "$STATE" != "enabled" ]; then
        ELAPSED=$(($(date +%s) - START_TIME))
        STALL_COUNT=$((STALL_COUNT + 1))

        echo ""
        echo "  ★★★ STALL #$STALL_COUNT at ${ELAPSED}s! (mount #$MOUNT_COUNT) ★★★"

        DMESG_NOW=$(dmesg | wc -l)
        if [ "$DMESG_NOW" -gt "$DMESG_BEFORE" ]; then
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) | grep -iE 'stall|watchdog|sched_ext|lavd|fuse|ERROR' | tail -20
            dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) > "$LOGDIR/dmesg_stall_${STALL_COUNT}.log"
            DMESG_BEFORE=$DMESG_NOW
        fi

        echo "[${ELAPSED}s] STALL #$STALL_COUNT mount=$MOUNT_COUNT" >> "$LOGDIR/stalls.log"

        # Clean up any orphan mounts
        pkill squashfuse_ll 2>/dev/null || true
        for mp in /tmp/fuse_mount_*; do
            fusermount3 -u "$mp" 2>/dev/null || true
            rmdir "$mp" 2>/dev/null || true
        done

        RESTART_COUNT=$((RESTART_COUNT + 1))
        echo "  Restarting LAVD..."
        start_lavd || break
        echo "  LAVD restarted"
    fi

    ELAPSED=$(($(date +%s) - START_TIME))
    if [ $((ELAPSED - LAST_REPORT)) -ge 30 ]; then
        LAST_REPORT=$ELAPSED
        echo "  [${ELAPSED}s] mounts=$MOUNT_COUNT stalls=$STALL_COUNT"
    fi

    # Rate: ~5 mount/unmount cycles per second
    sleep 0.15
done

echo ""
echo "============================================="
echo "  RESULTS"
echo "============================================="
echo "  Duration:   ${DURATION_MINS} minutes"
echo "  CPUs:       $(nproc)"
echo "  Workers:    $BG_WORKERS"
echo "  Mounts:     $MOUNT_COUNT squashfuse_ll cycles"
echo "  Stalls:     $STALL_COUNT"
echo "  Restarts:   $RESTART_COUNT"
echo ""

if [ $STALL_COUNT -gt 0 ]; then
    echo "  ★ STALL REPRODUCED WITH FUSE WORKLOAD!"
    cat "$LOGDIR/stalls.log" 2>/dev/null || true
else
    echo "  No stalls detected."
fi

echo ""
echo "=== Final dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext|fuse' | tail -10 || echo "  No relevant messages"
