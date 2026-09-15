#!/usr/bin/env python3
"""Alarm: is the vendored `scx` submodule pin stale or unreachable from upstream?

WHY THIS EXISTS, and why it is deliberately standalone
------------------------------------------------------
Between 2026-06-26 and 2026-08-12 the `Sync Upstream SCX` job ran nightly, failed
48 times, and succeeded zero times. Nobody noticed for seven weeks. The pin fell
41 upstream commits and 19 days behind, spanning 22 scheduler-behaviour changes
in the two schedulers scx-sim models most closely.

The reason it went unnoticed is the point of this file:

    A MONITOR MUST NOT DEPEND ON THE THING IT MONITORS.

The only signal that syncing had stopped was the sync job's own red X — emitted
by the broken component, onto a mirror repository nobody watches. A check whose
sole failure signal comes from the subsystem it watches is not a check.

So this script:
  * imports nothing from, and calls nothing in, `.github/workflows/sync-upstream.yml`;
  * shares no state, no branch, and no credentials with it;
  * runs on a schedule of its own, so it fires whether or not the sync job runs,
    is disabled, is deleted, or is silently delivering to a dead branch.

If the sync job is healthy this script is redundant. That is fine. It is
insurance, and insurance that depends on the insured event not happening is not
insurance.

WHAT IT CHECKS
--------------
1. REACHABILITY (the real P0). The pin must share history with upstream `main`.
   If `git merge-base` finds no common ancestor, the pin points at history that
   has been rebased away or dropped upstream, and no amount of "sync later" fixes
   it. This is the condition worth waking someone for.

2. STALENESS. How far the pin's *base* is behind upstream `main`, in both commits
   and days. Both are reported; either can trip the alarm.

3. LOCAL DIVERGENCE. The pin is legitimately NOT an ancestor of upstream main —
   it carries local patches (today: `lib/cgroup_bw: add scxsim targeted yield
   hooks`). That is expected and is NOT an error. The precise invariant is:

       the pin's BASE must be an ancestor of upstream main

   which is what this asserts. Stating it as "the pin must be an ancestor" — the
   obvious phrasing — is wrong: it is false every single day, so it would be
   ignored within a week, or "fixed" by deleting the local patch. The sync
   workflow's bare `git checkout origin/main` does exactly that deletion,
   silently. An unexpectedly LARGE number of local commits is flagged, because
   that suggests real divergence rather than a carried patch.

EXIT CODES
----------
    0  healthy (or within thresholds)
    1  alarm: stale beyond threshold, unexpected divergence, or unreachable
    2  could not run (missing submodule, no upstream remote, git failure)
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from datetime import datetime, timezone

# Thresholds chosen against the outage this exists to prevent: it ran 19 days /
# 41 commits behind without anyone noticing, so the alarm must trip well inside
# that. Overridable so the numbers can be tuned without editing code.
DEFAULT_MAX_DAYS = int(os.environ.get("SCX_PIN_MAX_DAYS", "10"))
DEFAULT_MAX_COMMITS = int(os.environ.get("SCX_PIN_MAX_COMMITS", "25"))
# More than a couple of carried patches means something other than "we hold one
# local fix" is going on.
DEFAULT_MAX_LOCAL = int(os.environ.get("SCX_PIN_MAX_LOCAL_COMMITS", "5"))

UPSTREAM_REF = os.environ.get("SCX_PIN_UPSTREAM_REF", "origin/main")


class CheckError(RuntimeError):
    """Cannot perform the check (as distinct from: the check failed)."""


def git(*args: str, cwd: str, check: bool = True) -> str:
    proc = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )
    if check and proc.returncode != 0:
        raise CheckError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout.strip()


def annotate(level: str, message: str) -> None:
    """Emit a GitHub annotation when running in Actions; plain text otherwise.

    Annotations surface on the run page and on the commit, which is somewhere a
    human already looks. The old signal was a red X on a mirror nobody watches;
    the fix is not a louder alarm, it is an alarm in a place people already read.
    """
    if os.environ.get("GITHUB_ACTIONS"):
        print(f"::{level}::{message}")
    else:
        print(f"{level.upper()}: {message}")


def summary(lines: list[str]) -> None:
    """Write a human-readable block to the Actions run summary, if available."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=".", help="sched-test checkout root")
    parser.add_argument("--submodule", default="scx", help="submodule path")
    parser.add_argument("--max-days", type=int, default=DEFAULT_MAX_DAYS)
    parser.add_argument("--max-commits", type=int, default=DEFAULT_MAX_COMMITS)
    parser.add_argument("--max-local-commits", type=int, default=DEFAULT_MAX_LOCAL)
    parser.add_argument(
        "--pin",
        default=None,
        help="override the pin SHA (used by the self-test to simulate staleness)",
    )
    parser.add_argument(
        "--fetch",
        action="store_true",
        help="fetch upstream before checking (CI passes this)",
    )
    args = parser.parse_args()

    sub = os.path.join(args.repo, args.submodule)
    if not os.path.isdir(os.path.join(sub, ".git")) and not os.path.exists(
        os.path.join(sub, ".git")
    ):
        annotate("error", f"{args.submodule} submodule is not checked out at {sub}")
        return 2

    try:
        if args.fetch:
            remote = UPSTREAM_REF.split("/", 1)[0]
            git("fetch", "--quiet", remote, cwd=sub, check=False)

        pin = args.pin or git("rev-parse", f"HEAD:{args.submodule}", cwd=args.repo)
        if not git("cat-file", "-e", pin, cwd=sub, check=False) and (
            subprocess.run(
                ["git", "cat-file", "-e", pin], cwd=sub, capture_output=True
            ).returncode
            != 0
        ):
            annotate(
                "error",
                f"pin {pin[:12]} does not exist in the submodule object store — "
                "it may have been garbage-collected or never fetched",
            )
            return 1

        upstream = git("rev-parse", UPSTREAM_REF, cwd=sub)

        # --- 1. reachability: the real P0 -------------------------------------
        base = git("merge-base", pin, UPSTREAM_REF, cwd=sub, check=False)
        if not base:
            annotate(
                "error",
                f"P0: pin {pin[:12]} shares NO history with {UPSTREAM_REF}. Upstream "
                "has rebased or dropped the commit the pin descends from; this "
                "cannot be resolved by syncing later.",
            )
            return 1

        # --- 2. local divergence (expected, but bounded) ----------------------
        local = [
            line
            for line in git(
                "log", "--oneline", f"{UPSTREAM_REF}..{pin}", cwd=sub
            ).splitlines()
            if line.strip()
        ]

        # --- 3. staleness of the BASE against upstream ------------------------
        behind = int(git("rev-list", "--count", f"{base}..{UPSTREAM_REF}", cwd=sub))
        base_date = git("log", "-1", "--format=%cI", base, cwd=sub)
        up_date = git("log", "-1", "--format=%cI", upstream, cwd=sub)
        days = (
            datetime.fromisoformat(up_date) - datetime.fromisoformat(base_date)
        ).days

    except CheckError as exc:
        annotate("error", f"cannot run the staleness check: {exc}")
        return 2

    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%MZ")
    report = [
        "## scx pin staleness alarm",
        "",
        f"checked {now} against `{UPSTREAM_REF}`",
        "",
        "| | |",
        "|---|---|",
        f"| pin | `{pin[:12]}` |",
        f"| pin base (shared with upstream) | `{base[:12]}` ({base_date[:10]}) |",
        f"| upstream head | `{upstream[:12]}` ({up_date[:10]}) |",
        f"| commits behind | **{behind}** (threshold {args.max_commits}) |",
        f"| days behind | **{days}** (threshold {args.max_days}) |",
        f"| local commits carried on the pin | {len(local)} "
        f"(threshold {args.max_local_commits}) |",
    ]
    for line in local:
        report.append(f"| ↳ carried | `{line}` |")
    print("\n".join(report))

    failures: list[str] = []
    if behind > args.max_commits:
        failures.append(
            f"pin base is {behind} commits behind {UPSTREAM_REF} "
            f"(threshold {args.max_commits})"
        )
    if days > args.max_days:
        failures.append(
            f"pin base is {days} days behind {UPSTREAM_REF} "
            f"(threshold {args.max_days})"
        )
    if len(local) > args.max_local_commits:
        failures.append(
            f"pin carries {len(local)} local commits (threshold "
            f"{args.max_local_commits}) — expected a small number of held patches, "
            "this suggests real divergence"
        )

    if failures:
        for msg in failures:
            annotate("error", f"scx pin staleness: {msg}")
        annotate(
            "error",
            "The scx submodule pin is stale. This alarm is INDEPENDENT of the "
            "sync-upstream-scx job by design — if that job looks green, it is "
            "not delivering where you think it is.",
        )
        report.append("")
        report.append("**ALARM: " + "; ".join(failures) + "**")
        summary(report)
        return 1

    report.append("")
    report.append("Pin is within thresholds and reachable from upstream.")
    summary(report)
    print("scx pin staleness: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
