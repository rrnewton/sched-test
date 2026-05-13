#!/usr/bin/env python3
"""
canary_oncpu_metric.py — derive per-window canary-task ONCPU-switch
trajectory from a scxsim Chrome JSON trace.

This is the scxsim-side counterpart of the wprof "NEGATIVE SPACE"
metric (per-window count of canary `yes` task switch_to events) used
to characterize the cpu-bw-stall-bug signature in `experiments/
wprof_cpu_bw_stall_capture_20260513/`. See tg
`explore-7-negative-space-partial-reproduction-in-scxsim` notes for
the full metric definition + skeptic verification (R3 SIGSTOP/SIGCONT
cadence makes 100 ms windows too noisy in healthy state; recommended
primary bucket is 1 s, with 300 ms as the empirical floor).

Source events: scxsim Chrome JSON trace (default `--perfetto FILE`
output, or `--perfetto FILE --trace-format json` explicitly). Per
`crates/scx_simulator/src/safe/perfetto.rs`, `TraceKind::TaskScheduled`
maps to a `cat:"sched"`, `ph:"B"` event with `name` = task name
(matches scenario `TaskDef::name`) and `args.pid` = PID. We bucket
those events whose name matches the configured canary regex.

Output: a 3-column CSV (bucket_start_us, count, rate_per_sec) plus
a one-line summary printed to stderr.

Typical use:

    scxsim run tests/fixtures/h6/bug1_canonical.json \
        --config tests/fixtures/h6/bug1_canonical.toml \
        -s lavd --cpus 4 --watchdog 5s --duration 5s \
        --perfetto out.json
    python3 scripts/canary_oncpu_metric.py \
        --trace out.json --bucket-ms 100 \
        --canary-regex '^yes_[0-9]+$' --out trajectory.csv
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
from collections import Counter
from pathlib import Path


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument(
        "--trace",
        required=True,
        type=Path,
        help="Path to a scxsim Chrome JSON trace file (default --perfetto output).",
    )
    p.add_argument(
        "--bucket-ms",
        type=float,
        default=1000.0,
        help="Bucket width in milliseconds. Default 1000 ms (matches wprof R1 measurement).",
    )
    p.add_argument(
        "--canary-regex",
        type=str,
        default=r"^yes_\d+$",
        help="Regex matching canary task names (TaskDef.name). Default `^yes_\\d+$` for "
        "the bug1_canonical fixture (yes_0..yes_15).",
    )
    p.add_argument(
        "--out",
        type=Path,
        required=True,
        help="Output CSV path (3 columns: bucket_start_us, count, rate_per_sec).",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()
    canary_re = re.compile(args.canary_regex)
    bucket_us = int(round(args.bucket_ms * 1000.0))
    if bucket_us <= 0:
        print(f"ERROR: bucket-ms must be > 0 (got {args.bucket_ms})", file=sys.stderr)
        return 2

    with args.trace.open() as f:
        data = json.load(f)
    events = data.get("traceEvents") if isinstance(data, dict) else data
    if not isinstance(events, list):
        print(
            f"ERROR: {args.trace} does not look like Chrome JSON (no traceEvents list)",
            file=sys.stderr,
        )
        return 2

    # Filter: TaskScheduled events for canary tasks.
    # Per perfetto.rs: cat="sched", ph="B", name=<TaskDef.name>, args.pid=<pid>.
    canary_events = []
    for e in events:
        if e.get("cat") != "sched" or e.get("ph") != "B":
            continue
        name = e.get("name", "")
        if not canary_re.match(name):
            continue
        canary_events.append(e)

    # Determine trace bounds: cover 0..last_ts so trailing zero-buckets
    # (= negative space) are visible in the output.
    if events:
        max_ts = max(int(e.get("ts", 0)) for e in events)
    else:
        max_ts = 0

    buckets: Counter[int] = Counter()
    for e in canary_events:
        ts_us = int(e["ts"])
        b = (ts_us // bucket_us) * bucket_us
        buckets[b] += 1

    # Emit dense series (include zero buckets through trace end).
    n_buckets = (max_ts // bucket_us) + 1
    with args.out.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["bucket_start_us", "count", "rate_per_sec"])
        rate_scale = 1_000_000.0 / bucket_us  # events/bucket → events/sec
        for i in range(n_buckets):
            b_start = i * bucket_us
            cnt = buckets.get(b_start, 0)
            w.writerow([b_start, cnt, f"{cnt * rate_scale:.2f}"])

    nonzero = sum(1 for v in buckets.values() if v > 0)
    print(
        f"canary_oncpu_metric: trace={args.trace.name} bucket_ms={args.bucket_ms:g} "
        f"canary_events={len(canary_events)} nonzero_buckets={nonzero}/{n_buckets} "
        f"(={100.0 * nonzero / max(n_buckets, 1):.1f}%) "
        f"max_ts_us={max_ts} -> {args.out}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
