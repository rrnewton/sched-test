#!/usr/bin/env python3
"""Two numbers that keep getting conflated: how many scenarios RUN on both
backends, and how many AGREE.

They are not the same and the gap between them is the interesting part. Nothing
computed either, so reports drifted into quoting one as the other -- this
script exists so the numbers come from the tree instead of from memory.

    python3 scripts/backend_scoreboard.py [--check]

`--check` exits non-zero when the tree and this script disagree, so CI catches
a scoreboard that has gone stale rather than one that has gone wrong.

THE THIRD CATEGORY IS THE POINT. A scenario can run on both backends and still
be uncheckable, and in a two-number summary that state is invisible -- it reads
as neither agreement nor failure. Both of the conversions done on 2026-08-13
landed there, for two unrelated reasons, so it is not a corner case.

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
        why="the io/compute split is a fabricated constant. IoSyncWrite lowers to "
            "a 50% duty cycle; the VM measures the worker off-CPU 21.4%. A share "
            "bound here is red for a workload-modelling reason and would be "
            "misread as a scheduler divergence.",
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


def source_work_types() -> int:
    """How many work types ktstr can express, as the IR enumerates them."""
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
    depth, n = 0, 0
    for line in s[j + 1:k].split("\n"):
        st = line.split("//")[0].strip()
        if depth == 0 and re.match(r"^[A-Z]\w*\s*(,|\{|$)", st):
            n += 1
        depth += st.count("{") - st.count("}")
    return n


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

    runs = len(committed) + len(NOT_CHECKABLE)
    print("SCENARIOS")
    print(f"  run on both backends ............ {runs}")
    print(f"    with a committed comparison ... {len(committed)}")
    print(f"    measured, not comparable ...... {len(NOT_CHECKABLE)}")
    print()
    print(f"  AGREE ........................... {len(agree)} / {runs}")
    print(f"    share-discriminating .......... {len(discriminating)}   "
          f"(>=2 cgroups, so the share bound can fire)")
    print(f"    gross-total only .............. {len(gross_only)}   "
          f"(1 cgroup: share is 1.0 both sides by construction)")
    print(f"  KNOWN DIVERGENCE ................ {len(diverge)}")
    for n in diverge:
        print(f"      {n}")
    print(f"  RUN BUT NOT CHECKABLE ........... {len(NOT_CHECKABLE)}")
    for e in NOT_CHECKABLE:
        print(f"      {e.name}  ({e.issue})")

    validated = sorted({wt for n in agree for wt in records[n].work_types})
    reached = sorted({wt for f in records.values() for wt in f.work_types}
                     | {wt for e in NOT_CHECKABLE for wt in e.work_types})
    total = source_work_types()
    print()
    print("WORK TYPES  (the better predictor: a future test is likelier to use")
    print("             one we have never validated than another spin_wait)")
    print(f"  ktstr can express ............... {total}")
    print(f"  reached either backend .......... {len(reached)}   {reached}")
    print(f"  in an AGREEING comparison ....... {len(validated)}   {validated}")

    problems = []
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
