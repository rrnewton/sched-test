#!/bin/bash
# Extreme FUSE + throttled cgroup + cgroup churn
# 4 CPUs, 1500 workers, squashfuse_ll in transient cgroups
set -euo pipefail

LAVD="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
SQSH="${2:-/home/newton/work/multi_sched-test/experiments/lavd_stall_vtime_20260430/test.sqsh}"
DURATION="${3:-300}"

modprobe fuse 2>/dev/null || true
CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true

echo "=== EXTREME FUSE+THROTTLE: $(nproc) CPUs ==="

$LAVD --performance --enable-cpu-bw --log-level warn 2>/dev/null &
LAVD_PID=$!
sleep 10
echo "LAVD: $(cat /sys/kernel/sched_ext/state 2>/dev/null)"

# Throttled cgroup with workers
mkdir -p "$CG_ROOT/throttled"
echo "200000 100000" > "$CG_ROOT/throttled/cpu.max"
stress-ng --cpu 300 --cpu-method matrixprod --timeout $((DURATION+30))s --quiet 2>/dev/null &
S1=$!; sleep 1
for pid in $(pgrep -P $S1 2>/dev/null); do
    echo $pid > "$CG_ROOT/throttled/cgroup.procs" 2>/dev/null || true
done

# Background workers
stress-ng --cpu 1200 --cpu-method matrixprod --timeout $((DURATION+30))s --quiet 2>/dev/null &
echo "Workers: 1500 (300 throttled + 1200 bg)"

STALLS=0
MOUNTS=0
START=$(date +%s)

echo "Starting aggressive FUSE + cgroup churn..."
while [ $(($(date +%s) - START)) -lt $DURATION ]; do
    MOUNTS=$((MOUNTS + 1))
    MP="/tmp/fmnt_$MOUNTS"
    CG="$CG_ROOT/fuse_cg_$MOUNTS"
    mkdir -p "$MP" "$CG" 2>/dev/null || continue

    # squashfuse_ll in transient cgroup (like systemd scope)
    (
        echo $$ > "$CG/cgroup.procs" 2>/dev/null || true
        exec squashfuse_ll "$SQSH" "$MP" 2>/dev/null
    ) &
    FPID=$!
    sleep 0.05

    # Access files
    cat "$MP/file_1" > /dev/null 2>&1 || true
    sleep 0.02

    # Unmount + destroy
    fusermount3 -u "$MP" 2>/dev/null || true
    wait $FPID 2>/dev/null || true
    rmdir "$MP" 2>/dev/null || true
    rmdir "$CG" 2>/dev/null || true

    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        ELAPSED=$(($(date +%s) - START))
        STALLS=$((STALLS + 1))
        echo "STALL #$STALLS at ${ELAPSED}s mount=$MOUNTS"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
        # Clean up orphan mounts
        pkill squashfuse_ll 2>/dev/null || true
        for mp in /tmp/fmnt_*; do fusermount3 -u "$mp" 2>/dev/null; rmdir "$mp" 2>/dev/null; done
        $LAVD --performance --enable-cpu-bw --log-level warn 2>/dev/null &
        LAVD_PID=$!
        sleep 10
    fi

    [ $((MOUNTS % 50)) -eq 0 ] && echo "  [$(($(date +%s) - START))s] mounts=$MOUNTS stalls=$STALLS"
    sleep 0.9
done

echo ""
echo "=== RESULTS ==="
echo "  CPUs: $(nproc), Workers: 1500, Mounts: $MOUNTS, Stalls: $STALLS"

pkill stress-ng 2>/dev/null || true
pkill squashfuse_ll 2>/dev/null || true
kill $LAVD_PID 2>/dev/null || true
sleep 1
rmdir "$CG_ROOT/throttled" 2>/dev/null || true

echo "=== dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  none"
