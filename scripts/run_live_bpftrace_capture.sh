#!/bin/bash
# run_live_bpftrace_capture.sh — drive a Bug-1 reproducer on devbig176
# baremetal with structops_full.bt + helpers_full.bt capturing JSONL through
# the stall transition.
#
# Usage:
#   bash scripts/run_live_bpftrace_capture.sh OUTDIR
#
# Output (in OUTDIR):
#   - structops.jsonl  (live structop calls + returns, one per line)
#   - helpers.jsonl    (live BPF-helper calls + returns)
#   - scx_lavd.{stdout,stderr,pid}
#   - reproducer.{stdout,stderr}
#   - dmesg.during, dmesg.tail-2000
#   - run_metadata.txt
#   - STALL_DETECTED (sentinel) if the bug fired
#   - watchdog_message.txt (extracted "failed to run for X.Ys" line)
set -u

OUTDIR=${1:?"usage: $0 OUTDIR"}
mkdir -p "$OUTDIR"

ROOT=/home/newton/working_copies/multi_sched-test
WORKTREE=$ROOT/worktrees/explore-1-live-bpftrace-capture/sched-test

SCX_LAVD=${SCX_LAVD:-$ROOT/experiments/bin_cache/a08c9e272b/scx_lavd}
WORKERS=${WORKERS:-32}
DURATION=${DURATION:-180}
PERIOD_MS=${PERIOD_MS:-150}
CPU_MAX=${CPU_MAX:-"10000 100000"}
SYM_BT=$WORKTREE/scripts/probes/scxsim_symmetric.bt
CG=/sys/fs/cgroup/test_bw_stop
TS=$(date -u +%Y%m%d-%H%M%SZ)
MARK="LIVE-CAPTURE-$TS"
LOG=$OUTDIR/runner.log

exec > >(tee -a "$LOG") 2>&1

echo "=== [run_live_bpftrace_capture.sh] start $(date -u +%FT%TZ) ==="
echo "OUTDIR=$OUTDIR"
echo "SCX_LAVD=$SCX_LAVD"
echo "  sha256=$(sha256sum "$SCX_LAVD" | awk '{print $1}')"
echo "WORKERS=$WORKERS DURATION=$DURATION PERIOD_MS=$PERIOD_MS CPU_MAX=\"$CPU_MAX\""

# ---- pre-flight ----
state=$(cat /sys/kernel/sched_ext/state)
if [ "$state" != "disabled" ]; then
    echo "ABORT pre-flight: sched_ext state '$state' != disabled"
    exit 2
fi
if pgrep -f "$SCX_LAVD" >/dev/null; then
    echo "ABORT pre-flight: scx_lavd already running"
    pgrep -af "$SCX_LAVD"
    exit 2
fi
sudo -n rmdir "$CG" 2>/dev/null || true
if [ ! -f "$SYM_BT" ]; then
    echo "ABORT pre-flight: probe not found at $SYM_BT"
    exit 2
fi

# ---- record metadata ----
{
    echo "ts=$TS"
    echo "host=$(hostname)"
    echo "kernel=$(uname -r)"
    echo "scx_lavd_path=$SCX_LAVD"
    echo "scx_lavd_sha256=$(sha256sum "$SCX_LAVD" | awk '{print $1}')"
    echo "scx_commit_built_from=a08c9e272b (per bin_cache metadata.json)"
    echo "workers=$WORKERS duration=$DURATION period_ms=$PERIOD_MS cpu_max=\"$CPU_MAX\""
    echo "scxsim_symmetric_bt=$SYM_BT"
    echo "scxsim_symmetric_bt_sha256=$(sha256sum "$SYM_BT" | awk '{print $1}')"
    echo "scx_timeout_ms=5000"
} > "$OUTDIR/run_metadata.txt"
cat "$OUTDIR/run_metadata.txt"

# ---- launch scx_lavd ----
echo "[$(date +%T)] launching scx_lavd"
sudo -n env SCX_TIMEOUT_MS=5000 "$SCX_LAVD" --performance --enable-cpu-bw \
    > "$OUTDIR/scx_lavd.stdout" 2> "$OUTDIR/scx_lavd.stderr" &
SUDO_PID=$!

LAVD_PID=""
for i in $(seq 1 30); do
    s=$(cat /sys/kernel/sched_ext/state 2>/dev/null)
    if [ "$s" = "enabled" ]; then
        LAVD_PID=$(pgrep -f "$SCX_LAVD --performance --enable-cpu-bw" | head -1)
        echo "[$(date +%T)] sched_ext enabled in ${i}s, scx_lavd pid=$LAVD_PID"
        break
    fi
    sleep 1
done
if [ "$(cat /sys/kernel/sched_ext/state)" != "enabled" ]; then
    echo "ABORT: scx_lavd didn't enable in 30s"
    cat "$OUTDIR/scx_lavd.stderr" | tail -30
    exit 3
fi
echo "$LAVD_PID" > "$OUTDIR/scx_lavd.pid"

# ---- arm bpftrace probe ----
echo "[$(date +%T)] arming scxsim_symmetric probe -> $OUTDIR/live.jsonl"
sudo -n env BPFTRACE_PERF_RB_PAGES=512 bpftrace "$SYM_BT" \
    > "$OUTDIR/live.jsonl" 2> "$OUTDIR/live.bt.err" &
BT_PID=$!
echo "$BT_PID" > "$OUTDIR/bpftrace.pid"
# bpftrace needs ~2-3s to compile + attach
sleep 4
if ! sudo -n grep -q "Attached" "$OUTDIR/live.bt.err" 2>/dev/null; then
    echo "WARN: probe may not have attached. stderr:"
    sudo -n cat "$OUTDIR/live.bt.err" 2>/dev/null
fi

# ---- start dmesg marker + run reproducer ----
sudo -n bash -c "echo '$MARK-START' > /dev/kmsg"
echo "[$(date +%T)] running R3 reproducer (workers=$WORKERS, duration=$DURATION, period=$PERIOD_MS ms)"

sudo -n bash <<EOF > "$OUTDIR/reproducer.stdout" 2> "$OUTDIR/reproducer.stderr"
set -u
mkdir -p "$CG"
echo "$CPU_MAX" > "$CG/cpu.max"
declare -a YES_PIDS
for i in \$(seq 1 $WORKERS); do
    yes >/dev/null &
    YES_PIDS[\$i]=\$!
done
sleep 1
COUNT=0
for pid in "\${YES_PIDS[@]}"; do
    if echo \$pid > "$CG/cgroup.procs" 2>/dev/null; then
        COUNT=\$((COUNT + 1))
    fi
done
echo "placed \$COUNT workers in $CG (cpu.max=\$(cat $CG/cpu.max))"

PERIOD=\$(printf "0.%03d" $PERIOD_MS)
END=\$(( \$(date +%s) + $DURATION ))
CYCLES=0
STALLED=0
while [ \$(date +%s) -lt \$END ]; do
    pkill -STOP -P \$\$ yes 2>/dev/null || true
    sleep \$PERIOD
    pkill -CONT -P \$\$ yes 2>/dev/null || true
    sleep \$PERIOD
    CYCLES=\$((CYCLES + 1))
    if (( CYCLES % 10 == 0 )); then
        # Quick check: has scheduler exited (watchdog fire)?
        if [ "\$(cat /sys/kernel/sched_ext/state 2>/dev/null)" != "enabled" ]; then
            echo "[\$(date +%T)] sched_ext state went non-enabled at cycle \$CYCLES — stall detected"
            STALLED=1
            break
        fi
    fi
done

# CONT all workers, then kill
pkill -CONT -P \$\$ yes 2>/dev/null || true
sleep 0.2
pkill -9 -P \$\$ yes 2>/dev/null || true
sleep 1
rmdir "$CG" 2>/dev/null || true
echo "cycles=\$CYCLES stalled=\$STALLED"
exit \$STALLED
EOF
REPRO_RC=$?
sudo -n bash -c "echo '$MARK-END' > /dev/kmsg"
echo "[$(date +%T)] reproducer rc=$REPRO_RC ($([ "$REPRO_RC" = "1" ] && echo 'STALL' || echo 'no stall'))"

# ---- stop bpftrace cleanly so SIGINT drains the perf ringbuffer ----
echo "[$(date +%T)] stopping bpftrace probe (gentle SIGINT first)"
sudo -n kill -INT $BT_PID 2>/dev/null || true
for i in $(seq 1 10); do
    if ! kill -0 $BT_PID 2>/dev/null; then
        break
    fi
    sleep 1
done
sudo -n kill -KILL $BT_PID 2>/dev/null || true

# ---- capture dmesg ----
sudo -n dmesg | awk "/$MARK-START/,/$MARK-END/" > "$OUTDIR/dmesg.during"
sudo -n dmesg | tail -2000 > "$OUTDIR/dmesg.tail-2000"
LINES=$(wc -l < "$OUTDIR/dmesg.during")
echo "[$(date +%T)] dmesg.during lines=$LINES"

# ---- detect stall in dmesg ----
WATCHDOG_LINE=$(sudo -n dmesg | grep "failed to run for" | tail -1 || true)
if [ -n "$WATCHDOG_LINE" ]; then
    echo "STALL_DETECTED: $WATCHDOG_LINE"
    echo "$WATCHDOG_LINE" > "$OUTDIR/watchdog_message.txt"
    touch "$OUTDIR/STALL_DETECTED"
fi

# ---- tear down scheduler ----
state=$(cat /sys/kernel/sched_ext/state)
echo "[$(date +%T)] post-run sched_ext state=$state"
if [ "$state" = "enabled" ]; then
    sudo -n pkill -INT -f "$SCX_LAVD" 2>/dev/null || true
    sleep 3
fi
if pgrep -f "$SCX_LAVD" >/dev/null; then
    sudo -n pkill -9 -f "$SCX_LAVD" 2>/dev/null || true
    sleep 1
fi

# ---- summary ----
LIVE_BYTES=$(stat -c %s "$OUTDIR/live.jsonl" 2>/dev/null || echo 0)
LIVE_LINES=$(wc -l < "$OUTDIR/live.jsonl" 2>/dev/null || echo 0)
LOST_LINES=$(grep -c "Lost" "$OUTDIR/live.bt.err" 2>/dev/null || echo 0)
echo
echo "=== summary ==="
echo "  live.jsonl:     $LIVE_LINES lines / $LIVE_BYTES bytes"
echo "  ringbuf losses: $LOST_LINES lines in stderr (zero is good)"
echo "  reproducer rc:  $REPRO_RC ($([ "$REPRO_RC" = "1" ] && echo 'STALL DETECTED' || echo 'NO STALL'))"
echo "  watchdog line:   $WATCHDOG_LINE"
echo "=== [run_live_bpftrace_capture.sh] end $(date -u +%FT%TZ) ==="
exit 0
