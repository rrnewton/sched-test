#!/usr/bin/env python3
"""Account on-CPU time in a Perfetto protobuf trace, by task class.

Both halves of a live-vs-simulated comparison are Perfetto protobuf: a ktstr
guest run writes one via wprof, and `scxsim --trace-format perfetto` (or
`scxsim-calibration`'s `dump_sim_perfetto` example) writes the other against a
deliberately wprof-compatible vocabulary. This turns either one into per-task
on-CPU totals, classified so the two can be compared like with like — and so
the part that does NOT compare like with like can be quantified rather than
discarded.

Why that second half matters: guest wprof is SYSTEM-WIDE. It sees kernel
threads, workqueues, softirqs and the idle task; the simulator models none of
them. Filtering them out to get a fair comparison throws away exactly the
unmodelled overhead worth characterising, so this reports both — the workload
totals, and what the filter removed, broken down by class and CPU.

Wire format only: no `.proto` files and no `trace_processor`. That is inherited
from `experiments/wprof_live_vs_scxsim_perfetto_20260513/scripts/decode_perfetto_pb.py`,
which established the approach when a missing `trusted_packet_sequence_id`
caused `trace_processor` to silently drop every scxsim event. Being
schema-less means a trace that some other tool refuses can still be read.

Usage:

    wprof_trace_compare.py <trace.pb> [--json]
    wprof_trace_compare.py --compare <live.pb> <sim.pb> [--json]

`--compare` prints the two side by side. It deliberately does NOT emit a
pass/fail verdict: tolerances and the four-outcome verdict live in the
`scxsim-calibration` crate, which is where a judgement belongs. This produces
the quantities that judgement would be made from.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Dict, Iterator, List, Optional, Set, Tuple

# --- protobuf wire format -------------------------------------------------

WIRE_VARINT = 0
WIRE_64BIT = 1
WIRE_LENGTH_DELIMITED = 2
WIRE_32BIT = 5

# TrackEvent.type
TYPE_SLICE_BEGIN = 1
TYPE_SLICE_END = 2
TYPE_INSTANT = 3


def read_varint(buf: bytes, pos: int) -> Tuple[int, int]:
    result = 0
    shift = 0
    while pos < len(buf):
        b = buf[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not b & 0x80:
            return result, pos
        shift += 7
    raise ValueError("truncated varint")


def read_tag(buf: bytes, pos: int) -> Tuple[int, int, int]:
    tag, pos = read_varint(buf, pos)
    return tag >> 3, tag & 0x07, pos


def skip_field(buf: bytes, pos: int, wire: int) -> int:
    if wire == WIRE_VARINT:
        _, pos = read_varint(buf, pos)
        return pos
    if wire == WIRE_64BIT:
        return pos + 8
    if wire == WIRE_LENGTH_DELIMITED:
        n, pos = read_varint(buf, pos)
        return pos + n
    if wire == WIRE_32BIT:
        return pos + 4
    raise ValueError(f"unknown wire type {wire}")


def iter_packets(blob: bytes) -> Iterator[bytes]:
    """Yield each `Trace.packets` submessage (field 1, length-delimited)."""
    pos = 0
    while pos < len(blob):
        field_no, wire, pos = read_tag(blob, pos)
        if field_no == 1 and wire == WIRE_LENGTH_DELIMITED:
            n, pos = read_varint(blob, pos)
            yield blob[pos : pos + n]
            pos += n
        else:
            pos = skip_field(blob, pos, wire)


# --- trace model ----------------------------------------------------------


class TaskClass(Enum):
    """What kind of thing accumulated the time.

    The split exists because the simulator models exactly one of these.
    """

    WORKLOAD = "workload"
    """The tasks under test. The only class with a simulator counterpart."""
    KTHREAD = "kthread"
    """Kernel threads: rcu_*, kworker/*, ksoftirqd/*, kcompactd*, and friends."""
    IRQ = "irq"
    """HARDIRQ / SOFTIRQ:* slices — interrupt servicing, not a task."""
    WORKQUEUE = "workqueue"
    """WQ:* slices."""
    IDLE = "idle"
    """swapper/N — the CPU doing nothing."""
    TRACER = "tracer"
    """wprof's own threads. Observer effect; excluded from overhead totals."""
    HARNESS = "harness"
    """ktstr's in-guest plumbing (stderr forwarder, console poller)."""
    OTHER = "other"


KTHREAD_PREFIXES = (
    "rcu_",
    "kworker/",
    "ksoftirqd/",
    "kcompactd",
    "kthread",
    "migration/",
    "cpuhp/",
    "irq/",
    "kdevtmpfs",
    "khugepaged",
    "kswapd",
    "writeback",
    "kblockd",
)
TRACER_NAMES = ("wprof", "wprof-capture")
HARNESS_NAMES = ("ktstr-stderr-fw", "trace-pipe", "hvc0-poll", "scheduler")


def classify(name: str, workload_names: Tuple[str, ...]) -> TaskClass:
    """Bucket a track by its name.

    `workload_names` is supplied by the caller rather than guessed: in a ktstr
    guest the workload threads inherit the comm of the test binary running as
    PID 1, so they are indistinguishable from `init` by name alone. Getting
    this wrong in either direction is the difference between measuring the
    workload and measuring the guest, so it is a parameter, not a heuristic.
    """
    if name in workload_names:
        return TaskClass.WORKLOAD
    if name.startswith("swapper/"):
        return TaskClass.IDLE
    if name.startswith("WQ:"):
        return TaskClass.WORKQUEUE
    if name.startswith(("HARDIRQ", "SOFTIRQ")):
        return TaskClass.IRQ
    if name.startswith(TRACER_NAMES) or name.startswith("wprof_rb"):
        return TaskClass.TRACER
    if name in HARNESS_NAMES:
        return TaskClass.HARNESS
    if name.startswith(KTHREAD_PREFIXES):
        return TaskClass.KTHREAD
    return TaskClass.OTHER


@dataclass
class OpenSlice:
    """A slice that has begun and not yet ended, on one track's stack."""

    start_ns: int
    label: str
    child_ns: int = 0


@dataclass
class TrackInfo:
    name: Optional[str] = None
    pid: Optional[int] = None
    tid: Optional[int] = None


@dataclass
class SliceTotals:
    """Accumulated on-CPU time for one named entity.

    Two totals, because Perfetto slices NEST and the difference matters here.
    wprof puts an IRQ slice INSIDE the slice of whatever task it interrupted,
    so a task's inclusive time already contains the interrupt time stolen from
    it. Reporting only inclusive would double-count when summing classes;
    reporting only self would hide that the interrupt landed on that task.
    """

    name: str
    klass: TaskClass
    total_ns: int = 0
    """Self time: this slice minus its direct children. Classes SUM correctly."""
    inclusive_ns: int = 0
    """Wall span of the slice including anything nested inside it."""
    count: int = 0
    unclosed: int = 0
    max_depth: int = 0
    tids: Set[int] = field(default_factory=set)

    @property
    def mean_ns(self) -> float:
        return self.total_ns / self.count if self.count else 0.0


@dataclass
class TraceAccounting:
    path: str
    packets: int
    events: int
    span_ns: int
    by_name: Dict[str, SliceTotals]
    unpaired_ends: int
    max_nesting: int = 0

    def by_class(self) -> Dict[TaskClass, int]:
        out: Dict[TaskClass, int] = {k: 0 for k in TaskClass}
        for t in self.by_name.values():
            out[t.klass] += t.total_ns
        return out

    def total_ns(self) -> int:
        return sum(t.total_ns for t in self.by_name.values())


def parse_track_descriptor(buf: bytes) -> Tuple[Optional[int], TrackInfo]:
    uuid: Optional[int] = None
    info = TrackInfo()
    pos = 0
    while pos < len(buf):
        f, wire, pos = read_tag(buf, pos)
        if f == 1 and wire == WIRE_VARINT:
            uuid, pos = read_varint(buf, pos)
        elif f == 2 and wire == WIRE_LENGTH_DELIMITED:  # name
            n, pos = read_varint(buf, pos)
            info.name = buf[pos : pos + n].decode("utf-8", "replace")
            pos += n
        elif f == 4 and wire == WIRE_LENGTH_DELIMITED:  # ThreadDescriptor
            n, pos = read_varint(buf, pos)
            sub = buf[pos : pos + n]
            pos += n
            sp = 0
            while sp < len(sub):
                sf, sw, sp = read_tag(sub, sp)
                if sf == 1 and sw == WIRE_VARINT:
                    info.pid, sp = read_varint(sub, sp)
                elif sf == 2 and sw == WIRE_VARINT:
                    info.tid, sp = read_varint(sub, sp)
                elif sf == 5 and sw == WIRE_LENGTH_DELIMITED:
                    n2, sp = read_varint(sub, sp)
                    if info.name is None:
                        info.name = sub[sp : sp + n2].decode("utf-8", "replace")
                    sp += n2
                else:
                    sp = skip_field(sub, sp, sw)
        else:
            pos = skip_field(buf, pos, wire)
    return uuid, info


def parse_interned(buf: bytes) -> Iterator[Tuple[int, int, str]]:
    """Yield (field_no, iid, name) for interned event names/categories."""
    pos = 0
    while pos < len(buf):
        f, wire, pos = read_tag(buf, pos)
        if wire != WIRE_LENGTH_DELIMITED:
            pos = skip_field(buf, pos, wire)
            continue
        n, pos = read_varint(buf, pos)
        sub = buf[pos : pos + n]
        pos += n
        iid: Optional[int] = None
        nm: Optional[str] = None
        sp = 0
        while sp < len(sub):
            sf, sw, sp = read_tag(sub, sp)
            if sf == 1 and sw == WIRE_VARINT:
                iid, sp = read_varint(sub, sp)
            elif sf == 2 and sw == WIRE_LENGTH_DELIMITED:
                n2, sp = read_varint(sub, sp)
                nm = sub[sp : sp + n2].decode("utf-8", "replace")
                sp += n2
            else:
                sp = skip_field(sub, sp, sw)
        if iid is not None and nm is not None:
            yield f, iid, nm


def parse_track_event(
    buf: bytes,
) -> Tuple[Optional[str], Optional[int], Optional[int], Optional[int]]:
    """Return (name, name_iid, type, track_uuid)."""
    name: Optional[str] = None
    name_iid: Optional[int] = None
    typ: Optional[int] = None
    track_uuid: Optional[int] = None
    pos = 0
    while pos < len(buf):
        f, wire, pos = read_tag(buf, pos)
        if f == 23 and wire == WIRE_LENGTH_DELIMITED:
            n, pos = read_varint(buf, pos)
            name = buf[pos : pos + n].decode("utf-8", "replace")
            pos += n
        elif f == 10 and wire == WIRE_VARINT:
            name_iid, pos = read_varint(buf, pos)
        elif f == 9 and wire == WIRE_VARINT:
            typ, pos = read_varint(buf, pos)
        elif f == 11 and wire == WIRE_VARINT:
            track_uuid, pos = read_varint(buf, pos)
        else:
            pos = skip_field(buf, pos, wire)
    return name, name_iid, typ, track_uuid


def account(path: Path, workload_names: Tuple[str, ...]) -> TraceAccounting:
    """Pair slice begin/end per track and total the durations.

    Perfetto slices on a track form a STACK, not a single slot: wprof nests an
    interrupt slice inside the slice of the task it interrupted. A one-slot
    implementation silently drops every enclosing slice — measured on a real
    guest trace it lost 731 of 3819 ends and under-reported workload CPU time
    by more than 10x, which looked like a spectacular fidelity gap and was a
    bug in this file. Hence an explicit stack, and `child_ns` bookkeeping so
    self time is exact.
    """
    blob = path.read_bytes()
    names_by_iid: Dict[int, str] = {}
    tracks: Dict[int, TrackInfo] = {}
    by_name: Dict[str, SliceTotals] = {}
    # track_uuid -> stack of open slices (innermost last)
    stacks: Dict[int, List[OpenSlice]] = {}

    packets = 0
    events = 0
    unpaired_ends = 0
    max_nesting = 0
    ts_min: Optional[int] = None
    ts_max: Optional[int] = None

    def entry_for(label: str) -> SliceTotals:
        e = by_name.get(label)
        if e is None:
            e = SliceTotals(name=label, klass=classify(label, workload_names))
            by_name[label] = e
        return e

    for pkt in iter_packets(blob):
        packets += 1
        ts: Optional[int] = None
        te: Optional[bytes] = None
        pos = 0
        while pos < len(pkt):
            f, wire, pos = read_tag(pkt, pos)
            if f == 8 and wire == WIRE_VARINT:  # timestamp
                ts, pos = read_varint(pkt, pos)
            elif f == 11 and wire == WIRE_LENGTH_DELIMITED:  # TrackEvent
                n, pos = read_varint(pkt, pos)
                te = pkt[pos : pos + n]
                pos += n
            elif f == 12 and wire == WIRE_LENGTH_DELIMITED:  # InternedData
                n, pos = read_varint(pkt, pos)
                for _f, iid, nm in parse_interned(pkt[pos : pos + n]):
                    names_by_iid.setdefault(iid, nm)
                pos += n
            elif f == 60 and wire == WIRE_LENGTH_DELIMITED:  # TrackDescriptor
                n, pos = read_varint(pkt, pos)
                uuid, info = parse_track_descriptor(pkt[pos : pos + n])
                if uuid is not None:
                    tracks[uuid] = info
                pos += n
            else:
                pos = skip_field(pkt, pos, wire)

        if te is None or ts is None:
            continue
        events += 1
        ts_min = ts if ts_min is None else min(ts_min, ts)
        ts_max = ts if ts_max is None else max(ts_max, ts)

        name, name_iid, typ, track_uuid = parse_track_event(te)
        if name is None and name_iid is not None:
            name = names_by_iid.get(name_iid)
        if track_uuid is None:
            continue

        if typ == TYPE_SLICE_BEGIN:
            label = name or (tracks.get(track_uuid, TrackInfo()).name) or "<unnamed>"
            stack = stacks.setdefault(track_uuid, [])
            stack.append(OpenSlice(start_ns=ts, label=label))
            max_nesting = max(max_nesting, len(stack))
        elif typ == TYPE_SLICE_END:
            stack = stacks.get(track_uuid) or []
            if not stack:
                unpaired_ends += 1
                continue
            done = stack.pop()
            dur = max(0, ts - done.start_ns)
            e = entry_for(done.label)
            e.inclusive_ns += dur
            e.total_ns += max(0, dur - done.child_ns)
            e.count += 1
            e.max_depth = max(e.max_depth, len(stack) + 1)
            tinfo = tracks.get(track_uuid)
            if tinfo is not None and tinfo.tid is not None:
                e.tids.add(tinfo.tid)
            if stack:
                stack[-1].child_ns += dur

    # Slices still open when the capture stopped are counted as unclosed, not
    # extrapolated: guessing an end time would invent CPU time.
    for _uuid, stack in stacks.items():
        for pending in stack:
            entry_for(pending.label).unclosed += 1

    span = (ts_max - ts_min) if (ts_min is not None and ts_max is not None) else 0
    return TraceAccounting(
        path=str(path),
        packets=packets,
        events=events,
        span_ns=span,
        by_name=by_name,
        unpaired_ends=unpaired_ends,
        max_nesting=max_nesting,
    )


# --- reporting ------------------------------------------------------------


def ms(ns: int) -> float:
    return ns / 1e6


def render(acc: TraceAccounting, cpus: int) -> str:
    lines: List[str] = []
    span_s = acc.span_ns / 1e9
    capacity_ns = acc.span_ns * cpus
    lines.append(f"=== {acc.path}")
    lines.append(
        f"    packets {acc.packets}  track-events {acc.events}  "
        f"span {span_s:.3f}s  assumed cpus {cpus}"
    )
    lines.append(f"    max slice nesting {acc.max_nesting}; unpaired ends {acc.unpaired_ends}")
    lines.append("    (class totals are SELF time, so they sum without double-counting)")

    lines.append("")
    lines.append("    on-CPU time by class:")
    by_class = acc.by_class()
    for klass in TaskClass:
        ns = by_class[klass]
        if ns == 0:
            continue
        pct = (ns / capacity_ns * 100.0) if capacity_ns else 0.0
        lines.append(f"      {klass.value:<12} {ms(ns):12.3f} ms  {pct:7.4f}% of capacity")

    lines.append("")
    lines.append("    top entities:")
    ranked = sorted(acc.by_name.values(), key=lambda t: -t.total_ns)
    for t in ranked[:18]:
        pct = (t.total_ns / capacity_ns * 100.0) if capacity_ns else 0.0
        unclosed = f" (+{t.unclosed} unclosed)" if t.unclosed else ""
        lines.append(
            f"      {t.name:<24} {t.klass.value:<10} self {ms(t.total_ns):11.3f} ms "
            f"{pct:7.4f}%  incl {ms(t.inclusive_ns):11.3f} ms  n={t.count}{unclosed}"
        )
    return "\n".join(lines)


def to_json(acc: TraceAccounting, cpus: int) -> Dict[str, object]:
    return {
        "path": acc.path,
        "packets": acc.packets,
        "track_events": acc.events,
        "span_ns": acc.span_ns,
        "cpus": cpus,
        "unpaired_ends": acc.unpaired_ends,
        "max_nesting": acc.max_nesting,
        "by_class_self_ns": {k.value: v for k, v in acc.by_class().items() if v},
        "by_name": [
            {
                "name": t.name,
                "class": t.klass.value,
                "self_ns": t.total_ns,
                "inclusive_ns": t.inclusive_ns,
                "count": t.count,
                "unclosed": t.unclosed,
                "tids": sorted(t.tids),
            }
            for t in sorted(acc.by_name.values(), key=lambda x: -x.total_ns)
        ],
    }


def compare(live: TraceAccounting, sim: TraceAccounting, cpus: int) -> str:
    """Side-by-side, split into what compares and what does not.

    No verdict. Tolerances and the four-outcome verdict live in the
    `scxsim-calibration` crate; duplicating them here would be a second,
    diverging opinion about what agreement means.
    """
    lines: List[str] = []
    lines.append("=== live vs simulated ===")
    lines.append(f"  live {live.path}")
    lines.append(f"  sim  {sim.path}")
    lines.append("")
    lines.append(f"  {'quantity':<26}{'live':>16}{'sim':>16}   note")

    def row(label: str, a: str, b: str, note: str = "") -> None:
        lines.append(f"  {label:<26}{a:>16}{b:>16}   {note}")

    row("trace span", f"{live.span_ns/1e9:.3f}s", f"{sim.span_ns/1e9:.3f}s",
        "live spans boot+hold+teardown; sim is the hold only")

    lc, sc = live.by_class(), sim.by_class()
    lw, sw = lc[TaskClass.WORKLOAD], sc[TaskClass.WORKLOAD]
    row("workload on-CPU", f"{ms(lw):.1f}ms", f"{ms(sw):.1f}ms",
        f"COMPARABLE — differ {abs(lw-sw)/lw*100:.2f}%" if lw else "")

    lslices = sum(t.count for t in live.by_name.values() if t.klass is TaskClass.WORKLOAD)
    sslices = sum(t.count for t in sim.by_name.values() if t.klass is TaskClass.WORKLOAD)
    row("workload slices", str(lslices), str(sslices),
        f"COMPARABLE — sim has {sslices/lslices:.0f}x more" if lslices else "")
    if lslices and sslices:
        row("mean slice", f"{ms(lw)/lslices:.3f}ms", f"{ms(sw)/sslices:.3f}ms",
            "COMPARABLE — the headline divergence")

    lines.append("")
    lines.append("  what the workload filter REMOVED from the live side")
    lines.append("  (the simulator has no counterpart for any of it: NotMeasured, not zero)")
    capacity = live.span_ns * cpus
    overhead = 0
    for klass in (TaskClass.KTHREAD, TaskClass.IRQ, TaskClass.WORKQUEUE,
                  TaskClass.HARNESS, TaskClass.TRACER, TaskClass.OTHER):
        ns = lc[klass]
        if not ns:
            continue
        if klass is not TaskClass.TRACER:
            overhead += ns
        pct = ns / capacity * 100.0 if capacity else 0.0
        tag = " (observer effect, excluded from the total)" if klass is TaskClass.TRACER else ""
        row(f"  {klass.value}", f"{ms(ns):.3f}ms", "n/a", f"{pct:.4f}% of capacity{tag}")
    row("  TOTAL unmodelled", f"{ms(overhead):.3f}ms", "n/a",
        f"{overhead/capacity*100:.4f}% of capacity" if capacity else "")
    row("  idle", f"{ms(lc[TaskClass.IDLE]):.1f}ms", f"{ms(sc[TaskClass.IDLE]):.1f}ms",
        "live idles during boot/teardown; sim has no idle at all")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("traces", nargs="+", type=Path)
    ap.add_argument(
        "--workload-names",
        default="init",
        help="comma-separated comms that ARE the workload. In a ktstr guest the "
        "worker threads inherit the test binary's comm (it runs as PID 1), so "
        "this defaults to 'init'. For a scxsim trace, pass the task comms.",
    )
    ap.add_argument("--cpus", type=int, default=2, help="CPUs, for the capacity denominator")
    ap.add_argument("--json", action="store_true")
    ap.add_argument(
        "--compare",
        action="store_true",
        help="two traces, live first then simulated: print them side by side",
    )
    ap.add_argument(
        "--sim-workload-names",
        default="",
        help="workload comms for the SECOND trace under --compare, when the "
        "simulator names its tasks differently from the guest (it does)",
    )
    args = ap.parse_args()

    workload = tuple(n for n in args.workload_names.split(",") if n)
    sim_workload = tuple(n for n in args.sim_workload_names.split(",") if n) or workload
    if args.compare:
        if len(args.traces) != 2:
            print("--compare takes exactly two traces: live then sim", file=sys.stderr)
            return 2
        results = [account(args.traces[0], workload), account(args.traces[1], sim_workload)]
    else:
        results = [account(p, workload) for p in args.traces]

    if args.json:
        json.dump([to_json(a, args.cpus) for a in results], sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0

    for acc in results:
        print(render(acc, args.cpus))
        print()
    if args.compare:
        print(compare(results[0], results[1], args.cpus))
    return 0


if __name__ == "__main__":
    sys.exit(main())
