#!/usr/bin/env python3
"""Record the VM backend's per-cgroup CPU time as a committed baseline.

The cross-backend check (`crates/ktstr-scenario-replay/tests/cross_backend.rs`)
compares a simulator run against a baseline produced here. Without this script
the comparison is a human reading two sets of numbers side by side, which is
exactly the gap the check exists to close.

USAGE

    scripts/record_vm_baseline.py <ktstr-run-dir> [--out <dir>] [--kernel 6.14.11]

`<ktstr-run-dir>` is the directory ktstr writes its stats sidecars to, one
`<scenario>-<hash>.ktstr.json` per test, e.g.

    <ktstr-target>/ktstr/<kernel>-<ktstr-commit>/

WHY AN EXTRACT AND NOT THE WHOLE SIDECAR

A sidecar is ~25 KB of which the check reads two fields, and it carries
incidental strings from the recording host that have no business in a committed
baseline. The extract is small enough to review in a diff, so a baseline that
moves is visible rather than buried.

The cost is that the extract can drift from the sidecar schema. That is
deliberate and bounded: this script reads `stats.cgroups[].total_cpu_time_ns`
and fails loudly if it is absent, so a ktstr schema change breaks the recording
step rather than silently producing an empty baseline.

WHAT IS NOT RECORDED

Only per-cgroup CPU time. The simulator has no counterpart for ktstr's
iterations, migrations or wake latency in the sense ktstr measures them, and
inventing correspondences would manufacture agreement rather than test for it.
"""

from __future__ import annotations

import argparse
import datetime
import glob
import json
import os
import sys
from typing import TypedDict


class Provenance(TypedDict):
    """Enough about a recording to decide whether to trust it.

    Deliberately a closed shape rather than a copy of the sidecar: everything
    not named here is dropped, including strings incidental to the machine that
    did the recording. Mirrors `BaselineProvenance` in
    `crates/ktstr-scenario-replay/src/compare.rs`; a field added on one side
    must be added on the other, and the Rust side fails loudly if it is not.
    """

    test_name: str
    topology: str | None
    scheduler: str | None
    project_commit: str | None
    passed: bool
    kernel: str
    recorded_utc: str | None
    sidecar: str


class Baseline(TypedDict):
    """One scenario's VM-side reading, as committed under `baselines/`."""

    scenario: str
    per_cgroup_cpu_time_ns: dict[str, int]
    provenance: Provenance


def extract(sidecar_path: str, kernel: str, run_epoch_ns: int | None) -> Baseline:
    with open(sidecar_path) as fh:
        doc = json.load(fh)

    scenario = doc["test_name"]

    if not doc.get("passed"):
        raise SystemExit(
            f"{sidecar_path}: this run did not pass (passed={doc.get('passed')}, "
            f"skipped={doc.get('skipped')}, inconclusive={doc.get('inconclusive')}). "
            "A baseline recorded from a failing VM run would make the simulator "
            "agree with a broken reference. Fix the VM run first."
        )

    cgroups = doc.get("stats", {}).get("cgroups")
    if not cgroups:
        raise SystemExit(
            f"{sidecar_path}: no stats.cgroups. Either ktstr's schema moved or "
            "this scenario reported no cgroups; both need a human, not a default."
        )

    per_cgroup: dict[str, int] = {}
    for entry in cgroups:
        name = entry["cgroup_name"]
        if "total_cpu_time_ns" not in entry:
            raise SystemExit(
                f"{sidecar_path}: cgroup {name} has no total_cpu_time_ns. This is "
                "the one field the cross-backend check reads; a ktstr schema "
                "change has broken the seam."
            )
        if name in per_cgroup:
            raise SystemExit(f"{sidecar_path}: duplicate cgroup {name}")
        per_cgroup[name] = int(entry["total_cpu_time_ns"])

    recorded = None
    if run_epoch_ns is not None:
        recorded = (
            datetime.datetime.fromtimestamp(
                run_epoch_ns / 1e9, datetime.timezone.utc
            )
            .replace(microsecond=0)
            .isoformat()
            .replace("+00:00", "Z")
        )

    provenance: Provenance = {
        "test_name": scenario,
        "topology": doc.get("topology"),
        "scheduler": doc.get("scheduler"),
        "project_commit": doc.get("project_commit"),
        "passed": bool(doc["passed"]),
        "kernel": kernel,
        "recorded_utc": recorded,
        "sidecar": os.path.basename(sidecar_path),
    }

    return {
        "scenario": scenario,
        "per_cgroup_cpu_time_ns": per_cgroup,
        "provenance": provenance,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("run_dir", help="directory of <scenario>-<hash>.ktstr.json files")
    ap.add_argument(
        "--out",
        default=os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "crates",
            "ktstr-scenario-replay",
            "baselines",
        ),
        help="where to write <scenario>.vm.json",
    )
    ap.add_argument(
        "--kernel",
        default=None,
        help="kernel the VM ran; defaults to the leading component of the run "
        "directory name, which is how ktstr lays it out (<kernel>-<commit>)",
    )
    args = ap.parse_args()

    run_dir = os.path.abspath(args.run_dir)
    kernel = args.kernel or os.path.basename(run_dir).rsplit("-", 1)[0]

    epoch_path = os.path.join(run_dir, ".ktstr_run_epoch")
    run_epoch_ns = None
    if os.path.exists(epoch_path):
        with open(epoch_path) as fh:
            run_epoch_ns = int(fh.read().strip())

    sidecars = sorted(glob.glob(os.path.join(run_dir, "*.ktstr.json")))
    if not sidecars:
        raise SystemExit(f"{run_dir}: no *.ktstr.json sidecars")

    os.makedirs(args.out, exist_ok=True)
    for path in sidecars:
        baseline = extract(path, kernel, run_epoch_ns)
        dest = os.path.join(args.out, f"{baseline['scenario']}.vm.json")
        with open(dest, "w") as fh:
            json.dump(baseline, fh, indent=2, sort_keys=True)
            fh.write("\n")
        total = sum(baseline["per_cgroup_cpu_time_ns"].values())
        print(f"{baseline['scenario']:<32} {len(baseline['per_cgroup_cpu_time_ns'])} cgroup(s)  total {total / 1e9:.3f}s  -> {dest}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
