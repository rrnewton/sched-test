#!/usr/bin/env python3
"""Two numbers that keep getting conflated: how many scenarios have a capture
from both backends, and how many AGREE.

They are not the same and the gap between them is the interesting part. Nothing
computed either, so reports drifted into quoting one as the other -- this
script exists so the numbers come from the tree instead of from memory.

    python3 scripts/backend_scoreboard.py [--check] [--ktstr CHECKOUT]

`--check` exits non-zero when the tree and this script disagree, so CI catches
a scoreboard that has gone stale rather than one that has gone wrong.

`--ktstr` points at a ktstr checkout and adds THE CEILING: how many of the 45
work types the exporter emits at all. Optional, because this repository vendors
no ktstr, so the default mode stays runnable in CI with no network. The exporter
lives only on `feat/ktstr-scenario-dsl-v2` (ktstr PR #47, public fork
https://github.com/rrnewton/ktstr); against `main` there is no exporter and the
ceiling is undefined rather than zero.

REPORTED BY BUCKET, NOT ONLY BY COUNT. Five of the seven scenarios with
two-backend captures use the same work type, so a scenario count reads as far
more maturity than exists.
The 45 work types are clustered into ten buckets by WHAT THEY STRESS (see
`BUCKETS`), and the per-bucket table is the headline: it makes the shape of the
evidence sayable in one sentence -- pure-CPU work has an agreeing comparison,
blocking IO's three ordinary ktstr types are explicitly refused while calibrated
modelling uses a separate simulator-only input, and six buckets have nothing.

A FOURTH CATEGORY, WORSE THAN THE THIRD, AND NOT DETECTABLE BY COUNTING RUNS.
A work type can be mapped, accepted, run to completion and satisfy its oracle
while the engine models something that is not the workload. Nothing refuses it
and nothing goes red: `compile_source` carries `ir.fidelity` and no caller
in-tree gates on it. `FIDELITY` records what has been AUDITED -- currently 11
ordinary ktstr replay types emitted by the exporter -- and 5 of those 11 are in
this state. Every number this script prints is therefore an UPPER BOUND on
trustworthy coverage, including the agreeing one.

THE THIRD CATEGORY IS THE POINT. A scenario can have a capture from both
backends and still be uncheckable, and in a two-number summary that state is
invisible -- it reads as neither agreement nor failure. Both conversions done
on 2026-08-13 landed there, for unrelated reasons, so it is not a corner case.

WHY WORK TYPES AND NOT JUST COUNTS. The scenarios are a proxy. The question
behind them is whether the simulator and a VM will agree on a ktstr test
somebody writes NEXT, and a count of 4 hides that all four exercise ONE work
type out of 45. A future test is far likelier to use something never validated
than to be another spin_wait, so the work-type line predicts the real variable
better than the scenario line does.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from dataclasses import dataclass

CRATE = pathlib.Path(__file__).resolve().parent.parent / "crates" / "ktstr-scenario-replay"
SOURCE_RS = (
    pathlib.Path(__file__).resolve().parent.parent
    / "crates" / "scxsim-workload-ir" / "src" / "source.rs"
)

@dataclass(frozen=True)
class NotCheckable:
    """A scenario measured on both backends whose oracle cannot be compared."""

    name: str
    issue: str
    work_types: tuple[str, ...]
    cgroups: int
    why: str
    measured: str


# Scenarios measured on BOTH backends that are deliberately not in `CASES`.
#
# Not a to-do list and not a backlog of failures: each entry is a scenario whose
# oracle CANNOT be evaluated across backends, for a reason that a bound cannot
# fix. Recorded here because the alternative is that they vanish from the
# scoreboard entirely and "4 agree" reads as the whole story.
#
# Adding a record + baseline + CASES entry for any of these promotes it out of
# this list; `--check` fails if that happens and the entry is left behind.
NOT_CHECKABLE: tuple[NotCheckable, ...] = (
    NotCheckable(
        name="cover_cgroup_io_compute_imbalance",
        issue="sim-1zqbl",
        work_types=("io_sync_write", "spin_wait"),
        cgroups=2,
        why="this committed capture used the now-removed fabricated IoSyncWrite "
            "50% duty-cycle fallback; current ordinary replay refuses the "
            "fieldless source. Its historical share result is not an evaluation "
            "of the separate, explicitly calibrated IoModelV1 input.",
        measured="VM cg_0 19.72% vs SIM 1.52% (12.96x), reproduced at 19.48%",
    ),
    NotCheckable(
        name="cross_affinity_churn_runs_in_vm",
        issue="sim-zb2e1",
        work_types=("futex_ping_pong", "cross_affinity_churn"),
        cgroups=1,
        why="one cgroup, so the share bound is UNDEFINED rather than failing -- "
            "every share is 1.0 by construction. Separately both work types "
            "declare spin_iters: 0, so every phase lowers to a zero duration.",
        measured="VM 23,991,065,904 ns mean of 4 vs SIM 19,537,778,632 ns",
    ),
)


# ---------------------------------------------------------------------------
# THE BUCKETS
#
# A flat list of 45 identifiers is not something anyone reasons about. These are
# the same 45, clustered by WHAT THEY STRESS ABOUT THE SCHEDULER, so that
# coverage can be stated in a sentence a person retains: "we have evidence for
# pure-CPU work and none at all for blocking IO."
#
# This is the one hardcoded table in this file, because the clustering is a
# judgement and cannot be derived. `--check` guarantees it stays in sync: the
# buckets plus `SIMULATOR_ONLY_VARIANTS` must partition `SourceWorkType`
# exactly, so a new variant nobody classified fails CI instead of quietly
# vanishing from the denominator.
#
# THE AXIS IS WHAT THE SCHEDULER SEES, which is not always what the variant is
# named after. Two that catch people, both worth knowing before trusting a
# bucket:
#   * `SmtSiblingSpin`'s work loop is byte-for-byte `SpinWait`. Every bit of its
#     SMT character comes from the affinity layer, so the scheduler sees pure
#     CPU and that is where it is bucketed.
#   * `IpcVariance` is INSTRUCTIONS PER CYCLE, not inter-process communication.
#     It never blocks and talks to nobody. Sitting near `PipeIo` in an
#     alphabetical list, it is the likeliest misread in the whole enum.
#
# BLOCKS is carried per bucket because it is the property that most predicts
# whether the engine can reproduce a workload at all: a task that never leaves
# the CPU is one the engine has a fair chance at; a task that goes off-CPU
# waiting for something requires the engine to model the something.
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class Bucket:
    """One cluster of work types, named by what it stresses."""

    key: str
    title: str
    rep: str
    blocks: str
    stresses: str
    members: tuple[str, ...]


BUCKETS: tuple[Bucket, ...] = (
    Bucket(
        key="cpu", title="Pure CPU burn", rep="SpinWait", blocks="never",
        stresses="Time-slicing and load balance among tasks that are ALWAYS "
                 "runnable; the task never leaves the CPU on its own.",
        members=("SpinWait", "AluHot", "IpcVariance", "SmtSiblingSpin"),
    ),
    Bucket(
        key="memory", title="Memory, cache & NUMA", rep="CachePressure",
        blocks="mostly never (NUMA sweeps sleep between phases)",
        stresses="Still CPU-bound, but the cost of a unit of work depends on "
                 "WHERE it runs: cache warmth, migration cost, NUMA locality.",
        members=("CachePressure", "CacheYield", "CachePipe", "PageFaultChurn",
                 "NumaWorkingSetSweep", "NumaMigrationChurn"),
    ),
    Bucket(
        key="yield", title="Voluntary yield", rep="YieldHeavy",
        blocks="never (stays runnable)",
        stresses="Hands the CPU back while STAYING RUNNABLE. Requeue and "
                 "re-placement on yield is a different decision from a block.",
        members=("YieldHeavy", "Mixed"),
    ),
    Bucket(
        key="storage_io", title="Blocking storage IO", rep="IoSyncWrite",
        blocks="always",
        stresses="Goes OFF-CPU in D-state waiting for a block device, then wakes "
                 "on completion. Stresses off-CPU accounting and the "
                 "runnable-versus-blocked distinction itself.",
        members=("IoSyncWrite", "IoRandRead", "IoConvoy"),
    ),
    Bucket(
        key="wakeup", title="Wakeups, synchronisation & event delivery",
        rep="FutexPingPong",
        blocks="usually -- but SPLIT BY ROLE: in 8 of these the waker never "
               "blocks while the wakee does, so one label per variant is wrong "
               "for half its workers",
        stresses="One task's progress depends on ANOTHER waking it. Wake "
                 "placement (select_task_rq), wake-to-run latency, WF_SYNC. "
                 "Three sub-shapes: paired/chained (PipeIo, FutexPingPong, "
                 "WakeChain, AsymmetricWaker); fan-out, herds and queues "
                 "(FutexFanOut, FanOutCompute, ThunderingHerd, MutexContention, "
                 "ProducerConsumerImbalance, EpollStorm); and delivery from "
                 "kernel context (SignalStorm, NetTraffic, IrqWake).",
        members=("PipeIo", "FutexPingPong", "FutexFanOut", "FanOutCompute",
                 "MutexContention", "ThunderingHerd", "AsymmetricWaker",
                 "WakeChain", "ProducerConsumerImbalance", "EpollStorm",
                 "SignalStorm", "NetTraffic", "IrqWake"),
    ),
    Bucket(
        key="timer", title="Timer & sleep driven", rep="Bursty", blocks="always",
        stresses="Sleeps on a timer and wakes on expiry. Idle/wake transitions "
                 "and timer wakeup latency, with a duty cycle nobody contends for.",
        members=("Bursty", "IdleChurn", "TimerLatency"),
    ),
    Bucket(
        key="priority", title="Priority, policy & preemption", rep="NiceSweep",
        blocks="varies (RT spinners never; the timer-driven preemptor does)",
        stresses="The task's weight or scheduling CLASS changes, or something "
                 "above it preempts it. reweight_task, class transitions, "
                 "starvation.",
        members=("NiceSweep", "PolicyChurn", "PriorityInversion", "RtStarvation",
                 "PreemptStorm"),
    ),
    Bucket(
        key="affinity", title="Affinity & migration", rep="AffinityChurn",
        blocks="never (yields after each change)",
        stresses="The set of CPUs a task MAY run on changes underneath the "
                 "scheduler. set_cpumask, affine_move_task, migration_cpu_stop.",
        members=("AffinityChurn", "CrossAffinityChurn"),
    ),
    Bucket(
        key="lifecycle", title="Process & cgroup lifecycle", rep="ForkExit",
        blocks="varies (waitpid blocks; self-rotation sleeps)",
        stresses="Tasks and cgroup memberships appear and disappear. "
                 "wake_up_new_task, exit paths, sched_move_task.",
        members=("ForkExit", "CgroupChurn", "CgroupAttachStorm"),
    ),
    Bucket(
        key="composite", title="Composite / not a primitive", rep="Schbench",
        blocks="whatever it was configured with",
        stresses="No intrinsic character -- these WRAP other work. Sequence "
                 "composes WorkPhases; Custom runs arbitrary user code; Schbench "
                 "and Taobench are whole benchmarks with internal thread "
                 "topologies. A coverage claim here is a claim about whatever "
                 "they were configured with.",
        members=("Sequence", "Custom", "Schbench", "Taobench"),
    ),
)

# SourceWorkType also carries typed inputs that are deliberately not ktstr
# WorkType variants. Keep them explicit and outside ktstr coverage denominators;
# source.rs documents why each one exists.
SIMULATOR_ONLY_VARIANTS: frozenset[str] = frozenset({"IoModelV1"})


def snake(camel: str) -> str:
    """`IoSyncWrite` -> `io_sync_write`, matching serde's rename_all."""
    return re.sub(r"(?<!^)(?=[A-Z])", "_", camel).lower()


# ---------------------------------------------------------------------------
# FIDELITY: the category that passes every gate and is still wrong.
#
# A work type can be mapped by the exporter, accepted by the lowering, accepted
# by ingest, run to completion by the engine, and satisfy its oracle -- while
# what the engine simulated is not the workload. Nothing refuses it. Nothing
# goes red. `compile_source` carries `ir.fidelity` and NO caller in-tree gates
# on it, so the disclosure exists and changes nothing.
#
# The storage types are the proof that this audit must stay live: their old
# fabricated fallback produced 19.72% in the VM against 1.52% in the simulator;
# ordinary replay now refuses all three rather than reporting a false result.
#
# Entries are keyed by work type and derived by reading `plan_work` in
# `scxsim-workload-ir/src/lower.rs` against the ktstr doc comments. Symbols, not
# line numbers -- line numbers rot across every rebase.
#
# NOT AN EXHAUSTIVE LIST OF DEFECTS. It records what has been AUDITED. An
# ordinary ktstr work type absent from here is unexamined, not clean. The 11 types
# emitted by the exporter have been audited; the other 34 never arrive through
# ordinary replay. Simulator-only typed inputs are accounted for separately.
# ---------------------------------------------------------------------------
# Each entry is (VERDICT, one-line, detail). The one-line is what the summary
# prints, so it is written once here rather than sliced out of the prose -- a
# summary derived by splitting the detail on "." produced ragged fragments and
# is exactly the kind of cleverness that reads fine until the prose changes.
FIDELITY: dict[str, tuple[str, str, str]] = {
    "SpinWait": (
        "FAITHFUL", "the one arm that lets the scheduler end the slice",
        "`continuous_run(ctx)` emits Run(scenario_duration), so the SCHEDULER's "
        "slice ends the slice rather than an invented phase boundary. The only "
        "arm in lower.rs that does this."),
    "YieldHeavy": (
        "REFUSED", "loud refusal at ingest, on a premise that has expired",
        "IngestError::YieldNotRepresentable. But the refusal is STALE: "
        "scx_simulator::Phase has since gained a Yield variant, so the premise "
        "no longer holds."),
    "Mixed": (
        "REFUSED", "same stale YieldNotRepresentable path",
        "Refused at ingest via the same arm as YieldHeavy."),
    "IoSyncWrite": (
        "REFUSED", "fieldless source cannot select a calibrated volume profile",
        "LoweringError::UnmodelledIoSource. The fabricated Run/Sleep fallback "
        "was removed; an explicit typed IoModelV1 source and profile are required."),
    "IoRandRead": (
        "REFUSED", "no calibrated random-read model is accepted",
        "LoweringError::UnmodelledIoSource. The v1 profile is specific to "
        "synchronous sequential writes and cannot be relabelled as a read model."),
    "IoConvoy": (
        "REFUSED", "no calibrated multi-worker convoy model is accepted",
        "LoweringError::UnmodelledIoSource. The one-worker v1 profile cannot "
        "represent device contention and is rejected rather than reused."),
    "ForkExit": (
        "NOT MODELLED", "tasks EXIT after 500us, leaving the cgroup empty",
        "Repeat::Once means a 12s scenario gets ~1ms of CPU and then an empty "
        "cgroup for 11.999s. The unbounded fork/exit/waitpid loop and its "
        "blocking waitpid are both gone."),
    "NiceSweep": (
        "NOT MODELLED", "the sweep is gone; the yield is dropped unrecorded",
        "Emits a single fixed nice; the -20..19 cycling is disclosed as lost, "
        "but the per-iteration sched_yield is dropped with NO record. Also "
        "invents a 500us quantum where ktstr's burst is a KNOWN 512 iterations "
        "-- a ~10x error it need not have made."),
    "SmtSiblingSpin": (
        "NOT MODELLED", "re-creates the 68x defect continuous_run exists to stop",
        "Calls invented_slice rather than continuous_run, fabricating a "
        "voluntary yield every 500us in a workload that has none. "
        "continuous_run's own doc records the measured 68x divergence from that "
        "mistake. The SmtSiblingPair pinning is dropped too."),
    "FutexPingPong": (
        "NOT MODELLED", "workers(8) silently becomes 2 tasks; handoff is inert",
        "The arm hardcodes tasks:2 and records this NOWHERE. The Sleep is a "
        "TIMED block of the same duration as the work, so cadence comes from a "
        "constant rather than from the partner, and Wake edges fire against "
        "already-running peers."),
    "CrossAffinityChurn": (
        "NOT MODELLED", "merged with AffinityChurn: identical IR, opposite target",
        "Shares an arm with AffinityChurn and produces identical IR, though one "
        "churns its OWN affinity and the other rewrites every SIBLING's. The "
        "cross-task dimension and the per-iteration yield are dropped with NO "
        "record."),
}

# The asymmetry worth naming: YieldHeavy is hard-refused for containing a yield,
# while NiceSweep's and CrossAffinityChurn's yields are deleted for free --
# because they never become a Phase::Yield, so the ingest gate never sees them.


def source_work_type_variants() -> list[str]:
    """Every SourceWorkType variant, including simulator-only typed inputs.

    Returns names rather than a count so the ktstr buckets plus the explicit
    simulator-only set can be checked against the enum. A count alone cannot
    catch one variant being renamed while another is added.
    """
    s = SOURCE_RS.read_text()
    i = s.index("pub enum SourceWorkType")
    j = s.index("{", i)
    depth, k = 0, j
    while k < len(s):
        if s[k] == "{":
            depth += 1
        elif s[k] == "}":
            depth -= 1
            if depth == 0:
                break
        k += 1
    depth, names = 0, []
    for line in s[j + 1:k].split("\n"):
        st = line.split("//")[0].strip()
        if depth == 0:
            m = re.match(r"^([A-Z]\w*)\s*(,|\{|$)", st)
            if m:
                names.append(m.group(1))
        depth += st.count("{") - st.count("}")
    return names


def exporter_mapped(ktstr: pathlib.Path) -> set[str]:
    """Which work types ktstr's exporter actually emits -- the CEILING.

    Optional, because it needs a ktstr checkout and this repository does not
    vendor one. Without it the scoreboard still reports everything else; with
    it, it reports the hard limit on how far any amount of test-writing can get.

    DERIVED, NEVER HARDCODED, and that is a direct lesson from a live bug: the
    harness's `scripts/ktstr_gate_chain.py` hardcodes this set as the 9 fieldless
    variants, correct when written at ktstr@3db68f27 and stale ever since --
    `FutexPingPong` and `CrossAffinityChurn` were added to the exporter
    afterwards. It therefore reports both as "exporter does not map it" when it
    does, which understated its own reachable-body count by one. A hardcoded
    mirror of somebody else's source drifts silently and then reports
    confidently.

    Scans only `fn work_type`'s body, and only after stripping comments: the
    module docs in that file NAME variants they explicitly do not map (citing
    `PriorityInversion::pi_mode` as an example of what would be dropped), so a
    whole-file scan overcounts.
    """
    p = ktstr / "src" / "scenario" / "export.rs"
    if not p.exists():
        sys.exit(
            f"ERROR: {p} not found.\n"
            "  If that checkout is on ktstr `main`, this is the expected failure:\n"
            "  the exporter exists only on feat/ktstr-scenario-dsl-v2 (ktstr PR #47),\n"
            "  hosted on the public fork https://github.com/rrnewton/ktstr ."
        )
    s = p.read_text(errors="replace")
    m = re.search(r"fn work_type\s*\(", s)
    if not m:
        sys.exit("ERROR: `fn work_type` not found in ktstr export.rs; exporter moved?")
    i = s.index("{", m.end())
    depth = 0
    for k in range(i, len(s)):
        if s[k] == "{":
            depth += 1
        elif s[k] == "}":
            depth -= 1
            if depth == 0:
                break
    body = re.sub(r"//[^\n]*", "", s[i + 1:k])
    return set(re.findall(r"WorkType::([A-Z]\w*)", body))


@dataclass(frozen=True)
class Facts:
    """What a record declares, as far as the scoreboard needs to know."""

    cgroups: frozenset[str]
    work_types: frozenset[str]


def record_facts(path: pathlib.Path) -> Facts:
    """Cgroups and work types a record actually declares."""
    d = json.loads(path.read_text())
    cgroups: set[str] = set()
    work_types: set[str] = set()
    for step in d.get("steps", []):
        for cg in step.get("setup", []):
            cgroups.add(cg["name"])
            for w in cg.get("works") or []:
                wt = w["work_type"]
                work_types.add(wt if isinstance(wt, str) else next(iter(wt)))
    # A CgroupDef with no explicit WorkSpec runs the default.
    return Facts(frozenset(cgroups), frozenset(work_types or {"spin_wait"}))


def cases() -> dict[str, str]:
    """Parse the CASES table: scenario -> Agree | KnownDivergence.

    Parsed rather than duplicated. A second copy of this list would be one more
    thing to keep in sync, and the whole complaint here is numbers drifting from
    the tree.
    """
    src = (CRATE / "tests" / "cross_backend.rs").read_text()
    i = src.index("const CASES:")
    end = src.index("\n];", i)
    out: dict[str, str] = {}
    for m in re.finditer(r'name:\s*"([^"]+)"\s*,\s*expect:\s*Expect::(\w+)', src[i:end]):
        out[m.group(1)] = m.group(2)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true",
                    help="exit non-zero if the tree and this script disagree")
    ap.add_argument("--ktstr", metavar="CHECKOUT",
                    help="a ktstr checkout on feat/ktstr-scenario-dsl-v2; adds the "
                         "exporter ceiling. Optional: this repo vendors no ktstr, "
                         "so the default mode stays runnable in CI with no network.")
    a = ap.parse_args()

    expect = cases()
    records = {p.stem: record_facts(p) for p in sorted((CRATE / "records").glob("*.json"))}
    baselines = {p.name.removesuffix(".vm.json") for p in (CRATE / "baselines").glob("*.vm.json")}

    committed = sorted(set(records) & baselines)
    agree = [n for n in committed if expect.get(n) == "Agree"]
    diverge = [n for n in committed if expect.get(n) == "KnownDivergence"]
    # With one cgroup every share is 1.0 on both sides, so `share_delta` is 0.0
    # by construction and only the (deliberately loose, 2.0x) gross-total ratio
    # can fire. That is a real check but a much weaker one, and counting it
    # beside a genuine share agreement overstates what has been established.
    discriminating = [n for n in agree if len(records[n].cgroups) >= 2]
    gross_only = [n for n in agree if len(records[n].cgroups) < 2]

    captures = len(committed) + len(NOT_CHECKABLE)
    print("SCENARIOS")
    print(f"  captured on both backends ....... {captures}")
    print(f"    with a committed comparison ... {len(committed)}")
    print(f"    measured, not comparable ...... {len(NOT_CHECKABLE)}")
    print()
    print(f"  AGREE ........................... {len(agree)} / {captures}")
    print(f"    share-discriminating .......... {len(discriminating)}   "
          f"(>=2 cgroups, so the share bound can fire)")
    print(f"    gross-total only .............. {len(gross_only)}   "
          f"(1 cgroup: share is 1.0 both sides by construction)")
    print(f"  KNOWN DIVERGENCE ................ {len(diverge)}")
    for n in diverge:
        print(f"      {n}")
    print(f"  CAPTURED BUT NOT CHECKABLE ...... {len(NOT_CHECKABLE)}")
    for e in NOT_CHECKABLE:
        print(f"      {e.name}  ({e.issue})")

    validated = sorted({wt for n in agree for wt in records[n].work_types})
    captured_work_types = sorted(
        {wt for f in records.values() for wt in f.work_types}
        | {wt for e in NOT_CHECKABLE for wt in e.work_types}
    )
    source_variants = set(source_work_type_variants())
    ktstr_names = source_variants - SIMULATOR_ONLY_VARIANTS
    mapped = exporter_mapped(pathlib.Path(a.ktstr)) if a.ktstr else None

    print()
    print("WORK TYPES  (the better predictor: a future test is likelier to use")
    print("             one we have never validated than another spin_wait)")
    print(f"  ktstr can express ............... {len(ktstr_names)}")
    print(f"  simulator-only model inputs ..... {len(SIMULATOR_ONLY_VARIANTS)}   "
          f"{sorted(SIMULATOR_ONLY_VARIANTS)} (excluded from ktstr coverage)")
    if mapped is not None:
        print(f"  the EXPORTER maps ............... {len(mapped)}   <- THE CEILING. "
              f"the other {len(ktstr_names) - len(mapped)} cannot reach the simulator at all,")
        print( "                                       no matter how many tests are written")
    print(f"  in two-backend captures ......... {len(captured_work_types)}   "
          f"{captured_work_types}")
    print(f"  in an AGREEING comparison ....... {len(validated)}   {validated}")

    # ---- by bucket -------------------------------------------------------
    # The headline. Five of the seven captured scenarios use the same work
    # type, so a per-scenario count reads as far more maturity than exists;
    # per bucket, the shape of what is and is not established is visible at a
    # glance.
    print()
    print("BY BUCKET  (n = ktstr work types in the bucket; columns narrow left to")
    print("            right, and every one of them is an UPPER bound)")
    print()
    print(f"  {'bucket':<42} {'n':>2} {'map':>4} {'cap':>4} {'agr':>4}  {'audited fidelity':<24}")
    print(f"  {'-'*42} {'--':>2} {'----':>4} {'----':>4} {'----':>4}  {'-'*24:<24}")
    for b in BUCKETS:
        mem = b.members
        sn = {snake(m) for m in mem}
        n_map = len([m for m in mem if m in mapped]) if mapped is not None else None
        n_captured = len(sn & set(captured_work_types))
        n_agr = len(sn & set(validated))
        audited = [m for m in mem if m in FIDELITY]
        faithful = [m for m in audited if FIDELITY[m][0] == "FAITHFUL"]
        refused = [m for m in audited if FIDELITY[m][0] == "REFUSED"]
        broken = [m for m in audited if FIDELITY[m][0] == "NOT MODELLED"]
        if audited:
            fid = f"{len(faithful)} ok, {len(refused)} refused, {len(broken)} WRONG"
        else:
            fid = "none audited"
        mapcol = "  -" if n_map is None else f"{n_map:>4}"
        print(f"  {b.title:<42} {len(mem):>2} {mapcol} {n_captured:>4} "
              f"{n_agr:>4}  {fid:<24}")
    print()
    print("  map = exporter emits it (blank without --ktstr)   "
          "cap = appears in captured two-backend evidence")
    print("  agr = appeared in an agreeing comparison")
    print("  WRONG = mapped, accepted, ran green, and does not model the workload")

    # ---- the dangerous category -----------------------------------------
    broken_all = sorted(k for k, v in FIDELITY.items() if v[0] == "NOT MODELLED")
    audited_all = sorted(FIDELITY)
    print()
    print("MAPPED, ACCEPTED, AND NOT FAITHFULLY MODELLED")
    print("  This is the category that reports success. Nothing refuses these;")
    print("  the engine accepts them, runs them, and produces a number that is")
    print("  not the workload. compile_source carries ir.fidelity and NO caller")
    print("  in-tree gates on it, so disclosure changes nothing.")
    print()
    print(f"  audited ......................... {len(audited_all)} "
          f"(ordinary ktstr replay types emitted by the exporter)")
    print(f"  NOT FAITHFULLY MODELLED ......... {len(broken_all)}")
    for k in broken_all:
        print(f"      {k:<20} {FIDELITY[k][1]}")
    print()
    print(f"  The remaining {len(ktstr_names) - len(audited_all)} ktstr work types "
          "are UNEXAMINED, not clean:")
    print("  the exporter does not emit them, so ordinary replay has not audited them.")

    problems = []
    # THE ANTI-STALENESS GATE. The ktstr buckets and simulator-only set must be
    # an exact, disjoint partition of SourceWorkType. Without this the table
    # decays the way ktstr_gate_chain.py's hardcoded exporter set did: correct
    # when written, silently wrong later, and still printing confident numbers.
    bucketed: dict[str, str] = {}
    for b in BUCKETS:
        for m in b.members:
            if m in bucketed:
                problems.append(
                    f"work type in two buckets: {m} ({bucketed[m]} and {b.key})")
            bucketed[m] = b.key
    bucketed_names = set(bucketed)
    for m in sorted(bucketed_names - source_variants):
        problems.append(f"bucketed work type no longer in SourceWorkType: {m}")
    for m in sorted(SIMULATOR_ONLY_VARIANTS - source_variants):
        problems.append(f"simulator-only variant no longer in SourceWorkType: {m}")
    for m in sorted(bucketed_names & SIMULATOR_ONLY_VARIANTS):
        problems.append(f"source variant classified as both ktstr and simulator-only: {m}")
    for m in sorted(source_variants - bucketed_names - SIMULATOR_ONLY_VARIANTS):
        problems.append(
            f"NEW SourceWorkType variant is unclassified: {m} -- add ordinary "
            "ktstr types to BUCKETS or typed model inputs to "
            "SIMULATOR_ONLY_VARIANTS")
    for b in BUCKETS:
        if b.rep not in b.members:
            problems.append(
                f"bucket {b.key}: representative {b.rep} is not a member")
    for m in sorted(set(FIDELITY) - ktstr_names):
        problems.append(f"FIDELITY names a non-ktstr SourceWorkType: {m}")
    if mapped is not None:
        for m in sorted(mapped - ktstr_names):
            problems.append(f"exporter maps a non-ktstr or unknown work type: {m}")
        # The audit covers exactly the mapped set; anything else is unexamined.
        for m in sorted(mapped - set(FIDELITY)):
            problems.append(
                f"exporter maps {m} but FIDELITY has no entry -- it reaches the "
                f"lowering and nobody has audited what it becomes")

    for n in sorted(set(records) - baselines):
        problems.append(f"record without a baseline: {n}")
    for n in sorted(baselines - set(records)):
        problems.append(f"baseline without a record: {n}")
    for n in committed:
        if n not in expect:
            problems.append(f"comparable scenario with no CASES entry: {n}")
    for e in NOT_CHECKABLE:
        if e.name in records or e.name in baselines or e.name in expect:
            problems.append(
                f"{e.name} is listed NOT_CHECKABLE but now has a record, "
                f"baseline or CASES entry -- it was promoted; drop it from the list")
    if problems:
        print("\nSTALE:")
        for p in problems:
            print(f"  {p}")
        if a.check:
            return 1
    elif a.check:
        print("\nscoreboard reconciles with the tree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
