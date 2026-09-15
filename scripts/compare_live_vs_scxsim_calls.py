#!/usr/bin/env python3
"""
compare_live_vs_scxsim_calls.py — side-by-side diff of structops + helper
JSONL traces from a live bpftrace capture vs an scxsim run.

Inputs:
  --live    PATH    JSONL produced by `bpftrace scripts/probes/structops_full.bt`
                    (or helpers_full.bt). May be passed multiple times to
                    merge structops + helpers files.
  --sim     PATH    JSONL produced by `scxsim ... --structops-jsonl PATH`.
  --out     PATH    Path to write the comparison report (markdown).
  --window  USEC    Coalesce consecutive same-name events within this
                    timestamp window into one record (default: 0 = none).
                    Useful for matching live traces (per-call records) to
                    scxsim traces (per-event records).

The diff is two-tiered:

1. **Aggregate counts**: per (kind, name) tuple, count of records in each
   side. Reveals high-level coverage gaps (e.g. live emits dispatch 12K
   times but scxsim emits 0).

2. **Sequence diff**: walks both streams in timestamp order, pairs records
   on (kind, name). Where a name has no counterpart on the other side,
   marks it `LIVE-ONLY` or `SIM-ONLY`. Where both sides have counts,
   reports the count delta.

The harness deliberately does NOT try to align timestamps (live time is
wall-clock CLOCK_MONOTONIC ns; sim time is logical sim-ns). Timestamp
alignment is a separate problem; the goal here is to catch structural
divergence — which calls happen, in roughly what proportion, in roughly
what order.

Tg: bpftrace-structops-helpers-trace-for-scxsim-vs-live-comparison
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Record:
    ts_ns: int
    cpu: int
    pid: int
    kind: str
    name: str
    phase: str
    args: dict
    ret: object  # int | None | bool


def load_jsonl(paths: list[Path]) -> list[Record]:
    out: list[Record] = []
    for p in paths:
        with p.open() as f:
            for line_no, line in enumerate(f, start=1):
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError as e:
                    print(
                        f"WARN {p}:{line_no}: not valid JSON: {e}",
                        file=sys.stderr,
                    )
                    continue
                out.append(
                    Record(
                        ts_ns=int(obj["ts_ns"]),
                        cpu=int(obj["cpu"]),
                        pid=int(obj["pid"]),
                        kind=str(obj["kind"]),
                        name=str(obj["name"]),
                        phase=str(obj["phase"]),
                        args=dict(obj.get("args", {})),
                        ret=obj.get("ret"),
                    )
                )
    out.sort(key=lambda r: r.ts_ns)
    return out


def aggregate_counts(records: list[Record]) -> Counter:
    c: Counter = Counter()
    for r in records:
        c[(r.kind, r.name, r.phase)] += 1
    return c


def render_aggregate(
    live: Counter,
    sim: Counter,
) -> str:
    keys = sorted(set(live) | set(sim), key=lambda k: (k[0], k[1], k[2]))
    rows: list[str] = []
    rows.append("| kind | name | phase | live | sim | delta | classification |")
    rows.append("|---|---|---|---:|---:|---:|---|")
    for k in keys:
        l = live.get(k, 0)
        s = sim.get(k, 0)
        delta = l - s
        kind, name, phase = k
        if l > 0 and s == 0:
            cls = "LIVE-ONLY"
        elif s > 0 and l == 0:
            cls = "SIM-ONLY"
        elif l == s:
            cls = "match"
        elif l > 0 and s > 0:
            ratio = max(l, s) / max(min(l, s), 1)
            cls = (
                "near-match" if ratio < 2 else
                f"diverges (live/sim ratio = {l/max(s,1):.1f}x)"
            )
        else:
            cls = "?"
        rows.append(
            f"| {kind} | {name} | {phase} | {l} | {s} | {delta:+d} | {cls} |"
        )
    return "\n".join(rows)


def render_first_divergence(
    live: list[Record],
    sim: list[Record],
    show_n: int = 50,
) -> str:
    """Walk both streams in arrival order and report the first 50 records on
    each side that have no counterpart on the other (by (kind, name) only).
    """
    live_names = Counter((r.kind, r.name) for r in live)
    sim_names = Counter((r.kind, r.name) for r in sim)

    live_only_names = {k for k in live_names if sim_names.get(k, 0) == 0}
    sim_only_names = {k for k in sim_names if live_names.get(k, 0) == 0}

    out_lines: list[str] = []
    out_lines.append(f"### LIVE-ONLY records (first {show_n} of "
                     f"{sum(live_names[k] for k in live_only_names)})")
    out_lines.append("")
    out_lines.append("```")
    n = 0
    for r in live:
        if (r.kind, r.name) in live_only_names:
            out_lines.append(
                f"  ts={r.ts_ns:>16}  cpu={r.cpu:<3}  {r.kind:8}  "
                f"{r.name:24}  {r.phase:5}  args={r.args}"
            )
            n += 1
            if n >= show_n:
                break
    out_lines.append("```")
    out_lines.append("")
    out_lines.append(f"### SIM-ONLY records (first {show_n} of "
                     f"{sum(sim_names[k] for k in sim_only_names)})")
    out_lines.append("")
    out_lines.append("```")
    n = 0
    for r in sim:
        if (r.kind, r.name) in sim_only_names:
            out_lines.append(
                f"  ts={r.ts_ns:>16}  cpu={r.cpu:<3}  {r.kind:8}  "
                f"{r.name:24}  {r.phase:5}  args={r.args}"
            )
            n += 1
            if n >= show_n:
                break
    out_lines.append("```")
    return "\n".join(out_lines)


def render_report(
    live: list[Record],
    sim: list[Record],
    live_paths: list[Path],
    sim_path: Path,
) -> str:
    live_count = aggregate_counts(live)
    sim_count = aggregate_counts(sim)

    # high-level totals
    live_total = len(live)
    sim_total = len(sim)
    live_kinds = Counter(r.kind for r in live)
    sim_kinds = Counter(r.kind for r in sim)
    live_names = {(r.kind, r.name) for r in live}
    sim_names = {(r.kind, r.name) for r in sim}
    common_names = live_names & sim_names
    live_only = live_names - sim_names
    sim_only = sim_names - live_names

    out: list[str] = []
    out.append("# Live vs scxsim structops/helpers comparison")
    out.append("")
    out.append(f"- **live JSONL inputs** ({len(live_paths)}): " +
               ", ".join(f"`{p}`" for p in live_paths))
    out.append(f"- **scxsim JSONL input**: `{sim_path}`")
    out.append("")
    out.append("## Headline metrics")
    out.append("")
    out.append(f"- live records:  **{live_total}**  (structops "
               f"{live_kinds.get('structop',0)} + helpers "
               f"{live_kinds.get('helper',0)})")
    out.append(f"- sim records:   **{sim_total}**  (structops "
               f"{sim_kinds.get('structop',0)} + helpers "
               f"{sim_kinds.get('helper',0)})")
    out.append(f"- distinct (kind,name) in **both**: {len(common_names)}")
    out.append(f"- distinct (kind,name) **live-only**: {len(live_only)}")
    out.append(f"- distinct (kind,name) **sim-only**: {len(sim_only)}")
    out.append("")
    out.append("## Per-(kind,name,phase) counts")
    out.append("")
    out.append(render_aggregate(live_count, sim_count))
    out.append("")
    out.append("## Sample divergent records")
    out.append("")
    out.append(render_first_divergence(live, sim))
    out.append("")
    out.append("## Interpretation guide")
    out.append("")
    out.append("- **LIVE-ONLY** with high count (e.g. `helper / select_cpu_dfl`) ")
    out.append("  is usually a scxsim coverage gap — scxsim doesn't model that ")
    out.append("  helper or doesn't emit a TraceKind for it. File as a tg ")
    out.append("  follow-up to extend `safe::structops_jsonl::emit_event` *only ")
    out.append("  if* the underlying event is something scxsim actually causes ")
    out.append("  to happen (per the No-Stub / No-Fake-Approximation rule).")
    out.append("- **SIM-ONLY** records mean scxsim emits an event the kernel ")
    out.append("  side never fires. Investigate: either scxsim is over-emitting, ")
    out.append("  or the kernel side dropped that callback in this version.")
    out.append("- **near-match** (within 2x) is usually fine for a 30s capture ")
    out.append("  + a sim run of nominally-equivalent duration; exact-match is ")
    out.append("  rare without seed alignment.")
    out.append("")
    return "\n".join(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--live", action="append", default=[], required=True,
        help="JSONL from bpftrace (may be repeated)"
    )
    ap.add_argument("--sim", required=True, help="JSONL from scxsim --structops-jsonl")
    ap.add_argument("--out", required=True, help="Output report path (markdown)")
    args = ap.parse_args(argv)

    live_paths = [Path(p) for p in args.live]
    sim_path = Path(args.sim)
    out_path = Path(args.out)

    for p in live_paths + [sim_path]:
        if not p.exists():
            print(f"ERROR: input not found: {p}", file=sys.stderr)
            return 2

    live = load_jsonl(live_paths)
    sim = load_jsonl([sim_path])

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(render_report(live, sim, live_paths, sim_path))
    print(f"wrote report: {out_path}")
    print(f"  live: {len(live)} records   sim: {len(sim)} records")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
