#!/bin/bash
# LAVD Stall Test — Cgroup BW Throttle + Churn
# Run inside vng VM. Combines:
# 1. A cgroup with tight cpu.max (like fbpkg.proxy.http.service)
# 2. High CPU contention
# 3. Rapid cgroup create/destroy (like systemd scopes for XAR mounts)

set -euo pipefail

LAVD_BIN="${1:-/home/newton/work/multi_sched-test/sched-test1/scx/target/release/scx_lavd}"
DURATION="${2:-300}"

CG_ROOT=$(mount | grep cgroup2 | head -1 | awk '{print $3}')
echo "+cpu" > "$CG_ROOT/cgroup.subtree_control" 2>/dev/null || true

echo "=== LAVD Throttle+Churn Test ==="
echo "CPUs: $(nproc), Duration: ${DURATION}s"

# Start LAVD
$LAVD_BIN --performance --enable-cpu-bw --log-level warn 2>/dev/null &
LAVD_PID=$!
sleep 10
echo "LAVD: $(cat /sys/kernel/sched_ext/state 2>/dev/null)"

# Create tight cgroup
mkdir -p "$CG_ROOT/tight_cg"
echo "200000 100000" > "$CG_ROOT/tight_cg/cpu.max"

# Run workers in tight cgroup
stress-ng --cpu 200 --cpu-method matrixprod --timeout $((DURATION+30))s --quiet 2>/dev/null &
S1=$!
sleep 1
for pid in $(pgrep -P $S1 2>/dev/null); do
    echo $pid > "$CG_ROOT/tight_cg/cgroup.procs" 2>/dev/null || true
done
echo "Tight workers: $(wc -l < "$CG_ROOT/tight_cg/cgroup.procs" 2>/dev/null || echo 0)"

# Background workers in root
stress-ng --cpu 400 --cpu-method matrixprod --timeout $((DURATION+30))s --quiet 2>/dev/null &
S2=$!
echo "Background workers started"

# Churn loop
STALLS=0
START=$(date +%s)
DMESG_BEFORE=$(dmesg | wc -l)
SEQ=0

echo "Starting churn..."
while [ $(($(date +%s) - START)) -lt $DURATION ]; do
    SEQ=$((SEQ + 1))
    CG="$CG_ROOT/churn_$SEQ"

    mkdir -p "$CG" 2>/dev/null || continue
    (echo $$ > "$CG/cgroup.procs" 2>/dev/null; exec /bin/sleep 0.01) &
    CHILD=$!
    sleep 0.05
    wait $CHILD 2>/dev/null || true
    rmdir "$CG" 2>/dev/null || true

    STATE=$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo unknown)
    if [ "$STATE" != "enabled" ]; then
        ELAPSED=$(($(date +%s) - START))
        STALLS=$((STALLS + 1))
        echo "STALL #$STALLS at ${ELAPSED}s (churn=$SEQ)!"
        dmesg | tail -5 | grep -iE 'stall|watchdog|sched_ext' || true
        $LAVD_BIN --performance --enable-cpu-bw --log-level warn 2>/dev/null &
        LAVD_PID=$!
        sleep 10
    fi

    if [ $((SEQ % 100)) -eq 0 ]; then
        ELAPSED=$(($(date +%s) - START))
        echo "  [${ELAPSED}s] churns=$SEQ stalls=$STALLS"
    fi
done

echo ""
echo "=== RESULTS ==="
echo "  Duration: ${DURATION}s"
echo "  CPUs: $(nproc)"
echo "  Churns: $SEQ"
echo "  Stalls: $STALLS"

pkill stress-ng 2>/dev/null || true
kill $LAVD_PID 2>/dev/null || true
sleep 1
rmdir "$CG_ROOT/tight_cg" 2>/dev/null || true

echo "=== dmesg ==="
dmesg | grep -iE 'stall|watchdog|sched_ext' | tail -10 || echo "  none"
