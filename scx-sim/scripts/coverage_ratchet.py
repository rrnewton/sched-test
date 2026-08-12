#!/usr/bin/env python3
"""
Rust library-coverage ratchet for scx_simulator's validate.sh.

Measures per-crate line coverage of the embeddable library crates from the
profile data produced by `cargo llvm-cov nextest --workspace --no-report`
(which validate.sh runs immediately before this), and GATES on regression
against a committed per-crate baseline (data/rust_coverage_baseline.csv).

There is NO arbitrary target percentage: the floor is the last achieved value
minus a small epsilon for cross-toolchain line-attribution jitter. The baseline
moves only via `--update-baseline`, which is raise-only unless --allow-lower.

C scheduler-.so coverage is a separate concern owned by coverage.sh; this gate
instruments Rust only (cargo-llvm-cov leaves the dlopen'd .so non-instrumented).

Usage:
    python3 scripts/coverage_ratchet.py                       # gate (default; validate.sh)
    python3 scripts/coverage_ratchet.py --update-baseline     # re-measure + raise the baseline
    python3 scripts/coverage_ratchet.py --update-baseline --allow-lower
"""

import argparse
import csv
import json
import subprocess
import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
BASELINE_CSV = PROJECT_ROOT / "data" / "rust_coverage_baseline.csv"

# Embeddable library crates the gate protects. NOT scx_cgroup_tree (slated for
# removal) and NOT embed_harness (the link-contract harness, not a guarded
# library surface). Keep this list in sync with the workspace's library members.
LIBRARY_CRATES = ("scx_simulator", "scxsim-build", "scx_perf")

# Files excluded from each crate's coverage %: test/bench/example code, build
# scripts, the vendored C substrate (coverage.sh's domain), bin entrypoints, and
# deps. /examples/ is listed explicitly so the scoping does not depend on
# cargo-llvm-cov's default ignore set (examples are not run by the tests, so
# counting them would drag a crate down spuriously).
IGNORE_REGEX = r"(/tests?/|/benches/|/examples/|build\.rs|/csrc/|/scxtest/|/bin/|main\.rs|/registry/|/\.cargo/)"

# Slack for cross-toolchain line-attribution jitter. NOT a coverage target.
EPSILON_PCT = 0.5

BASELINE_HEADER = ("crate", "coverage_pct")


class BaselineError(Exception):
    """The committed baseline CSV is missing a column or has a non-numeric value."""


def parse_total_lines(report_json: str) -> tuple[int, int]:
    """(count, covered) total lines from a `cargo llvm-cov report --json --summary-only` payload."""
    doc = json.loads(report_json)
    lines = doc["data"][0]["totals"]["lines"]
    return int(lines["count"]), int(lines["covered"])


def line_pct(count: int, covered: int) -> float:
    """Covered-line percentage; 0.0 when there are no instrumentable lines."""
    return (covered * 100.0 / count) if count > 0 else 0.0


def read_baseline(path: Path) -> dict[str, float]:
    """crate -> committed baseline percentage."""
    out: dict[str, float] = {}
    with path.open(newline="") as handle:
        # start=2: DictReader consumes line 1 as the header.
        for lineno, row in enumerate(csv.DictReader(handle), start=2):
            try:
                out[row["crate"]] = float(row["coverage_pct"])
            except (KeyError, ValueError) as exc:
                raise BaselineError(
                    f"malformed baseline {path} (line {lineno}): {exc}; regenerate "
                    "with `python3 scripts/coverage_ratchet.py --update-baseline`"
                ) from exc
    return out


def gate_failures(
    measured: dict[str, tuple[int, int]],
    baseline: dict[str, float],
    epsilon: float,
) -> list[str]:
    """One message per gate violation; empty list means the gate passes."""
    failures: list[str] = []
    for crate in LIBRARY_CRATES:
        if crate not in measured:
            failures.append(f"{crate}: not measured (absent from the coverage run)")
            continue
        count, covered = measured[crate]
        if count == 0:
            failures.append(
                f"{crate}: 0 instrumentable lines — scoping (-p name / ignore-regex) is wrong, not a real pass"
            )
            continue
        if crate not in baseline:
            failures.append(
                f"{crate}: no committed baseline — run "
                "`python3 scripts/coverage_ratchet.py --update-baseline` and commit "
                "data/rust_coverage_baseline.csv"
            )
            continue
        pct = line_pct(count, covered)
        floor = baseline[crate] - epsilon
        if pct < floor:
            failures.append(
                f"{crate}: coverage {pct:.1f}% < baseline {baseline[crate]:.1f}% "
                f"(floor {floor:.1f}%). Add tests, or if intentional run "
                "--update-baseline and commit the new baseline."
            )
    return failures


def raise_only_baseline(
    measured: dict[str, tuple[int, int]],
    prior: dict[str, float],
    allow_lower: bool,
) -> tuple[dict[str, float], list[str]]:
    """New baseline + crates refused for lowering. Refused crates keep their prior value."""
    new: dict[str, float] = {}
    refused: list[str] = []
    for crate in LIBRARY_CRATES:
        pct = round(line_pct(*measured[crate]), 1)
        if not allow_lower and crate in prior and pct < prior[crate]:
            refused.append(f"{crate}: {pct:.1f}% < current baseline {prior[crate]:.1f}%")
            new[crate] = prior[crate]
        else:
            new[crate] = pct
    return new, refused


def write_baseline(path: Path, values: dict[str, float]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(BASELINE_HEADER)
        for crate in LIBRARY_CRATES:
            writer.writerow([crate, f"{values[crate]:.1f}"])


def measure_crate(crate: str) -> tuple[int, int]:
    """Per-crate (count, covered) from on-disk profile data — no test re-run."""
    proc = subprocess.run(
        [
            "cargo", "llvm-cov", "report", "--json", "--summary-only",
            "-p", crate, "--ignore-filename-regex", IGNORE_REGEX,
        ],
        cwd=PROJECT_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return parse_total_lines(proc.stdout)


def measure_all() -> dict[str, tuple[int, int]]:
    return {crate: measure_crate(crate) for crate in LIBRARY_CRATES}


def run_instrumented_suite() -> None:
    subprocess.run(
        ["cargo", "llvm-cov", "nextest", "--workspace", "--no-fail-fast", "--no-report"],
        cwd=PROJECT_ROOT,
        check=True,
    )


def update_baseline(allow_lower: bool) -> int:
    run_instrumented_suite()
    measured = measure_all()
    prior = read_baseline(BASELINE_CSV) if BASELINE_CSV.exists() else {}
    values, refused = raise_only_baseline(measured, prior, allow_lower)
    if refused:
        print("Refusing to lower the baseline (use --allow-lower to override):", file=sys.stderr)
        for msg in refused:
            print(f"  {msg}", file=sys.stderr)
        return 1
    write_baseline(BASELINE_CSV, values)
    for crate in LIBRARY_CRATES:
        print(f"  {crate}: {line_pct(*measured[crate]):.1f}%")
    print(f"  Wrote baseline {BASELINE_CSV.relative_to(PROJECT_ROOT)}")
    return 0


def gate() -> int:
    measured = measure_all()
    baseline = read_baseline(BASELINE_CSV) if BASELINE_CSV.exists() else {}
    for crate in LIBRARY_CRATES:
        base = baseline.get(crate)
        suffix = f"baseline {base:.1f}%" if base is not None else "no baseline"
        print(f"  {crate}: {line_pct(*measured[crate]):.1f}% ({suffix})")
    failures = gate_failures(measured, baseline, EPSILON_PCT)
    if failures:
        print("FAIL: Rust library coverage ratchet:", file=sys.stderr)
        for msg in failures:
            print(f"  {msg}", file=sys.stderr)
        return 1
    print("  Rust library coverage ratchet passed.")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Rust library-coverage ratchet")
    parser.add_argument(
        "--update-baseline",
        action="store_true",
        help="re-measure (fresh instrumented run) and raise the committed baseline",
    )
    parser.add_argument(
        "--allow-lower",
        action="store_true",
        help="with --update-baseline, permit lowering a crate's baseline",
    )
    args = parser.parse_args(argv)
    try:
        if bool(args.update_baseline):
            return update_baseline(bool(args.allow_lower))
        return gate()
    except subprocess.CalledProcessError as exc:
        print(
            "ERROR: a cargo-llvm-cov invocation failed "
            f"(exit {exc.returncode}). In gate mode this script must run AFTER "
            "`cargo llvm-cov nextest --workspace --no-report` (validate.sh does this).",
            file=sys.stderr,
        )
        return 1
    except BaselineError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
