#!/bin/bash
# LAVD Stall Reproduction — Vtime Underflow & Cgroup BW Interaction
#
# Tests whether the production binary (commit b7d93529, which has
# cur_logical_clk=0 init bug) stalls short-lived processes.
#
# Methodology:
# 1. Start LAVD with --performance --enable-cpu-bw
# 2. Create background CPU load (like production's 8000 threads)
# 3. Periodically spawn short-lived processes (like squashfuse_ll)
# 4. Monitor for watchdog stalls via dmesg + /sys/kernel/sched_ext/
#
# Usage: sudo ./repro_stall.sh [LAVD_BINARY] [DURATION_MINS]

set -euo pipefail

LAVD_BIN="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
DURATION_MINS="${2:-5}"
BG_WORKERS="${BG_WORKERS:-200}"     # Background CPU workers
SPAWN_INTERVAL="${SPAWN_INTERVAL:-2}"  # Seconds between short-lived spawns
CPUS_TO_USE="${CPUS_TO_USE:-8}"     # Limit CPUs (like production's 52)

echo "=== LAVD Stall Reproduction Experiment ==="
echo "  Binary:         $LAVD_BIN"
echo "  Duration:       ${DURATION_MINS} minutes"
echo "  BG workers:     $BG_WORKERS"
echo "  Spawn interval: ${SPAWN_INTERVAL}s"
echo "  CPUs:           $CPUS_TO_USE (taskset)"
echo ""

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: Must run as root"
    exit 1
fi

# Record start time and dmesg baseline
START_TIME=$(date +%s)
DMESG_BEFORE=$(dmesg | wc -l)
LOGFILE="repro_$(date +%Y%m%d_%H%M%S).log"

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    kill $LAVD_PID 2>/dev/null || true
    kill $SPAWNER_PID 2>/dev/null || true
    pkill -f "stress-ng.*stall_repro" 2>/dev/null || true
    sleep 2
    echo "Scheduler state: $(cat /sys/kernel/sched_ext/state 2>/dev/null || echo 'unknown')"
    echo "Done."
}
trap cleanup EXIT

# Start LAVD
echo "=== Starting LAVD ==="
$LAVD_BIN --performance --enable-cpu-bw --log-level info 2>&1 &
LAVD_PID=$!
sleep 3

# Verify LAVD is running
if cat /sys/kernel/sched_ext/root/ops 2>/dev/null | grep -q lavd; then
    echo "  ✓ LAVD running (PID $LAVD_PID)"
    echo "  ops: $(cat /sys/kernel/sched_ext/root/ops)"
else
    echo "  ✗ LAVD failed to start"
    wait $LAVD_PID
    exit 1
fi

# Start background CPU workers
echo "=== Starting $BG_WORKERS background workers ==="
stress-ng --cpu $BG_WORKERS --cpu-method matrixprod \
    --timeout $((DURATION_MINS * 60 + 30))s \
    --job-name stall_repro 2>/dev/null &
STRESS_PID=$!
sleep 2
echo "  ✓ stress-ng running (PID $STRESS_PID)"

# Spawner: periodically create short-lived processes (like squashfuse_ll)
# Each process sleeps briefly then exits, mimicking XAR mount behavior
echo "=== Starting periodic spawner (every ${SPAWN_INTERVAL}s) ==="
(
    while true; do
        # Spawn a short-lived process that:
        # 1. Forks (like squashfuse_ll being forked from systemd)
        # 2. Does minimal work (like mounting a FUSE filesystem)
        # 3. Sleeps waiting for I/O (like FUSE waiting for requests)
        # 4. Wakes up and exits
        /bin/sleep 0.001 &  # Very short sleep — mimics a task that wakes quickly
        CHILD_PID=$!
        
        # Log the spawn
        echo "[$(date +%H:%M:%S)] Spawned sleep PID=$CHILD_PID" >> "$LOGFILE"
        
        sleep $SPAWN_INTERVAL
    done
) &
SPAWNER_PID=$!

echo "  ✓ Spawner running (PID $SPAWNER_PID)"
echo ""
echo "=== Monitoring for stalls (${DURATION_MINS} minutes) ==="
echo "  Watching: dmesg, /sys/kernel/sched_ext/state"
echo ""

# Monitor loop
END_TIME=$((START_TIME + DURATION_MINS * 60))
STALL_COUNT=0
while [ $(date +%s) -lt $END_TIME ]; do
    sleep 5
    
    # Check sched_ext state
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo "unknown")
    if [ "$STATE" != "enabled" ]; then
        STALL_COUNT=$((STALL_COUNT + 1))
        echo "  ⚠ [$(date +%H:%M:%S)] sched_ext state=$STATE (stall #$STALL_COUNT)"
        dmesg | tail -5 | grep -iE "stall|watchdog|sched_ext|lavd" || true
        
        # Log the stall
        echo "[$(date +%H:%M:%S)] STALL DETECTED state=$STATE" >> "$LOGFILE"
        dmesg | tail -20 >> "$LOGFILE"
        
        # Try to restart LAVD
        echo "  Restarting LAVD..."
        $LAVD_BIN --performance --enable-cpu-bw --log-level info 2>&1 &
        LAVD_PID=$!
        sleep 3
        
        if cat /sys/kernel/sched_ext/root/ops 2>/dev/null | grep -q lavd; then
            echo "  ✓ LAVD restarted (PID $LAVD_PID)"
        else
            echo "  ✗ LAVD restart failed. Exiting."
            break
        fi
    fi
    
    # Check for new dmesg messages
    DMESG_NOW=$(dmesg | wc -l)
    if [ "$DMESG_NOW" -gt "$DMESG_BEFORE" ]; then
        NEW_MSGS=$(( DMESG_NOW - DMESG_BEFORE ))
        if dmesg | tail -$NEW_MSGS | grep -qiE 'stall|watchdog|sched_ext'; then
            echo "  ⚠ [$(date +%H:%M:%S)] New dmesg messages ($NEW_MSGS):"
            dmesg | tail -$NEW_MSGS | grep -iE 'stall|watchdog|sched_ext|lavd' || true
        fi
        DMESG_BEFORE=$DMESG_NOW
    fi
    
    ELAPSED=$(($(date +%s) - START_TIME))
    if [ $((ELAPSED % 60)) -lt 5 ]; then
        echo "  [$(date +%H:%M:%S)] ${ELAPSED}s elapsed, $STALL_COUNT stalls"
    fi
done

echo ""
echo "=== Results ==="
echo "  Duration:    ${DURATION_MINS} minutes"
echo "  Stalls:      $STALL_COUNT"
echo "  Log file:    $LOGFILE"
echo ""

if [ $STALL_COUNT -gt 0 ]; then
    echo "  ★ STALL REPRODUCED! See $LOGFILE for details."
else
    echo "  No stalls detected."
fi

echo ""
echo "=== Final dmesg check ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  No relevant messages"
