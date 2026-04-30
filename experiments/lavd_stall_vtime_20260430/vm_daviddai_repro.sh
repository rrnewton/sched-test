#!/bin/bash
# David Dai's stall reproducer (P2295308593) adapted for vng VM
# Key ingredient: massive concurrent cgroup migration with tight cpu.max
set -e

LAVD="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
WORKERS="${2:-300}"
DURATION="${3:-30}"

CG1=/sys/fs/cgroup/test_bw
CG2=/sys/fs/cgroup/test_bw2

echo "=== David Dai Stall Reproducer ==="
echo "  CPUs:    $(nproc)"
echo "  Workers: $WORKERS per side ($((WORKERS * 2)) total)"
echo "  CG1:     full machine quota"
echo "  CG2:     2 CPUs (tight)"
echo ""

# Enable cpu controller
CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    pkill -9 stress-ng 2>/dev/null || true
    sleep 1
    rmdir $CG1 2>/dev/null || true
    rmdir $CG2 2>/dev/null || true
    kill $LAVD_PID 2>/dev/null || true
    echo "Done."
}
trap cleanup EXIT

# Start LAVD
echo "Starting LAVD..."
$LAVD --performance --enable-cpu-bw --log-level warn 2>/dev/null &
LAVD_PID=$!
sleep 10
STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
echo "  LAVD: $STATE (PID $LAVD_PID)"
if [ "$STATE" != "enabled" ]; then
    echo "  LAVD failed to enable!"
    exit 1
fi

# Create cgroups — scale quota to match our CPU count
NCPU=$(nproc)
FULL_QUOTA=$((NCPU * 100000))  # Full machine
TIGHT_QUOTA=200000              # 2 CPUs
echo "Creating cgroups..."
mkdir -p $CG1 $CG2
echo "$FULL_QUOTA 100000" > $CG1/cpu.max
echo "$TIGHT_QUOTA 100000" > $CG2/cpu.max
echo "  CG1: cpu.max = $(cat $CG1/cpu.max)"
echo "  CG2: cpu.max = $(cat $CG2/cpu.max)"

# Start workers
echo ""
echo "Starting $((WORKERS * 2)) workers..."
stress-ng --cpu $WORKERS --cpu-method matrixprod --timeout $((DURATION * 3 + 60))s --quiet 2>/dev/null &
STRESS_PID1=$!
stress-ng --cpu $WORKERS --cpu-method matrixprod --timeout $((DURATION * 3 + 60))s --quiet 2>/dev/null &
STRESS_PID2=$!
sleep 3

PIDS1=$(pgrep -P $STRESS_PID1 2>/dev/null || true)
PIDS2=$(pgrep -P $STRESS_PID2 2>/dev/null || true)
echo "  Group 1: $(echo "$PIDS1" | wc -w) workers"
echo "  Group 2: $(echo "$PIDS2" | wc -w) workers"

# Place group 1 → CG1 (full quota), group 2 → CG2 (tight)
echo ""
echo "=== Phase 1: Group1→CG1(full), Group2→CG2(tight) ==="
for pid in $PIDS1; do echo $pid > $CG1/cgroup.procs 2>/dev/null || true; done
for pid in $PIDS2; do echo $pid > $CG2/cgroup.procs 2>/dev/null || true; done
echo "  CG1: $(wc -l < $CG1/cgroup.procs 2>/dev/null || echo 0) procs"
echo "  CG2: $(wc -l < $CG2/cgroup.procs 2>/dev/null || echo 0) procs"

DMESG_BEFORE=$(dmesg | wc -l)
echo "Running Phase 1 for ${DURATION}s..."
for i in $(seq 1 $DURATION); do
    sleep 1
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        echo "  ★★★ STALL at Phase 1, ${i}s! ★★★"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
        break
    fi
    [ $((i % 10)) -eq 0 ] && echo "  [${i}s] state=$STATE"
done

# SWAP: group 1 → CG2 (tight!), group 2 → CG1 (full)
echo ""
echo "=== SWAP: Group1→CG2(tight!), Group2→CG1(full) ==="
echo "  Moving $WORKERS tasks from full→tight quota simultaneously..."
for pid in $PIDS1; do echo $pid > $CG2/cgroup.procs 2>/dev/null || true; done
for pid in $PIDS2; do echo $pid > $CG1/cgroup.procs 2>/dev/null || true; done
echo "  CG1: $(wc -l < $CG1/cgroup.procs 2>/dev/null || echo 0) procs"
echo "  CG2: $(wc -l < $CG2/cgroup.procs 2>/dev/null || echo 0) procs"

echo "Running Phase 2 for ${DURATION}s..."
for i in $(seq 1 $DURATION); do
    sleep 1
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        echo "  ★★★ STALL at Phase 2, ${i}s! ★★★"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
        break
    fi
    [ $((i % 10)) -eq 0 ] && echo "  [${i}s] state=$STATE"
done

# Wait for watchdog (5 minutes like David's script)
echo ""
echo "=== Waiting 120s for potential delayed stall ==="
for i in $(seq 1 120); do
    sleep 1
    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        echo "  ★★★ STALL at wait phase, ${i}s! ★★★"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
        break
    fi
    [ $((i % 30)) -eq 0 ] && echo "  [${i}s] state=$STATE"
done

echo ""
echo "=== RESULTS ==="
DMESG_NOW=$(dmesg | wc -l)
if [ "$DMESG_NOW" -gt "$DMESG_BEFORE" ]; then
    STALL_MSGS=$(dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) | grep -ciE 'stall|watchdog' || echo 0)
    echo "  Stall-related dmesg messages: $STALL_MSGS"
    dmesg | tail -$((DMESG_NOW - DMESG_BEFORE)) | grep -iE 'stall|watchdog|sched_ext' | tail -10 || true
else
    echo "  No new dmesg messages"
fi

STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
echo "  Final state: $STATE"
if [ "$STATE" != "enabled" ]; then
    echo "  ★ STALL REPRODUCED!"
else
    echo "  No stalls detected."
fi
