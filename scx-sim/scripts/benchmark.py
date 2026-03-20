#!/usr/bin/env python3
"""
Throughput benchmarks for scx_simulator.

Measures speedup factor (simulated time / wall-clock time) across a matrix
of modes, schedulers, and workloads. Generates Plotly HTML dashboards and
optionally appends results to a historical CSV for tracking over time.

Usage:
    python3 scripts/benchmark.py run                    # Full benchmark suite
    python3 scripts/benchmark.py run --no-build         # Skip cargo build
    python3 scripts/benchmark.py run --csv out.csv --append --git-metadata
    python3 scripts/benchmark.py run --perf --filter-mode sequential --no-build
    python3 scripts/benchmark.py plot-history data/benchmarks/CPU/perf_history.csv
"""

import argparse
import csv
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Project layout
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).resolve().parent.parent
SCXSIM = PROJECT_ROOT / "target" / "release" / "scxsim"
WORKLOADS_DIR = PROJECT_ROOT / "crates" / "scx_simulator" / "workloads"

# ---------------------------------------------------------------------------
# Test matrix
# ---------------------------------------------------------------------------

MODES = [
    ("sequential",         []),
    ("interleave",         ["--interleave"]),
    ("preemptive-pmu",     ["--preemptive", "--preempt-mode", "pmu"]),
    ("preemptive-e9patch", ["--preemptive", "--preempt-mode", "e9patch"]),
]

SCHEDULERS = ["simple", "lavd", "mitosis"]

WORKLOAD_NAMES = ["two_runners.json", "dsq_contention.json"]

# CSV column order for current-run output
CSV_COLUMNS = [
    "workload", "scheduler", "mode",
    "simulated_ns", "wall_clock_ms", "speedup_factor",
]

# Extra columns prepended when --git-metadata is used
GIT_META_COLUMNS = [
    "timestamp", "git_commit", "git_depth", "git_branch", "git_dirty",
]

# Patterns that indicate mutex/lock overhead in perf reports.
# These are split into exact-substring patterns and word-boundary patterns.
# Word-boundary patterns use \b to avoid false positives like "clock" matching "lock".
_MUTEX_EXACT_PATTERNS = [
    "mutex", "pthread_mutex", "futex",
    "Mutex::lock", "MutexGuard", "try_lock",
    "with_sim", "clone_sim_arc", "sim_rbc_pause", "sim_rbc_resume",
    "spin_lock", "rwlock",
]

# Compiled regex: combine exact substrings with case-insensitive matching
_MUTEX_RE = re.compile(
    "|".join(re.escape(p) for p in _MUTEX_EXACT_PATTERNS),
    re.IGNORECASE,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def find_python() -> str:
    """Return the venv python if it exists, else system python3."""
    venv_python = PROJECT_ROOT / ".venv" / "bin" / "python3"
    if venv_python.is_file():
        return str(venv_python)
    return "python3"


def e9_schedulers_available() -> bool:
    """Check if _e9.so scheduler variants exist (built via `make e9`)."""
    sched_dirs = list(
        PROJECT_ROOT.glob("target/release/build/scx_simulator-*/out/schedulers")
    )
    if not sched_dirs:
        return False
    return any(sched_dirs[0].glob("*_e9.so"))


def read_workload_duration_ns(workload_path: Path) -> int:
    """Read global.duration from a workload JSON and convert to nanoseconds."""
    with open(workload_path) as f:
        data = json.load(f)
    dur_secs = data.get("global", {}).get("duration", 1)
    if dur_secs <= 0:
        dur_secs = 10  # default fallback, matches rtapp.rs
    return int(dur_secs * 1_000_000_000)


def get_git_metadata() -> dict[str, str]:
    """Collect git commit, depth, branch, dirty status."""
    def _git(*args: str) -> str:
        result = subprocess.run(
            ["git", "-C", str(PROJECT_ROOT)] + list(args),
            capture_output=True, text=True, check=True,
        )
        return result.stdout.strip()

    commit = _git("rev-parse", "--short", "HEAD")
    branch = _git("rev-parse", "--abbrev-ref", "HEAD")
    depth_script = PROJECT_ROOT / "scripts" / "gitdepth.sh"
    if depth_script.is_file():
        result = subprocess.run(
            [str(depth_script)],
            capture_output=True, text=True, cwd=str(PROJECT_ROOT),
        )
        depth = result.stdout.strip() if result.returncode == 0 else "0"
    else:
        depth = _git("rev-list", "--count", "HEAD")
    # Check dirty status
    dirty_result = subprocess.run(
        ["git", "-C", str(PROJECT_ROOT), "diff-index", "--quiet", "HEAD", "--"],
        capture_output=True,
    )
    dirty = "dirty" if dirty_result.returncode != 0 else ""
    return {
        "git_commit": commit,
        "git_depth": depth,
        "git_branch": branch,
        "git_dirty": dirty,
    }


# ---------------------------------------------------------------------------
# Benchmark data
# ---------------------------------------------------------------------------

@dataclass
class BenchmarkResult:
    """Result of a single benchmark run."""
    workload: str
    scheduler: str
    mode: str
    simulated_ns: int
    wall_clock_ms: float
    speedup_factor: float
    perf_report: Optional[str] = field(default=None, repr=False)


def _run_perf_report(perf_data_path: str) -> str:
    """Extract top functions from a perf.data file via perf report.

    Uses --call-graph none to produce a compact flat list of hot symbols
    rather than expanded call trees that would consume too many lines.
    """
    report = subprocess.run(
        [
            "perf", "report",
            "-i", perf_data_path,
            "--stdio", "--no-children", "-n",
            "--percent-limit", "0.5",
            "--call-graph", "none",
        ],
        capture_output=True, text=True,
    )
    lines = report.stdout.splitlines()[:60]
    return "\n".join(lines)


def run_single_benchmark(
    workload_path: Path,
    scheduler: str,
    mode_name: str,
    mode_flags: list[str],
    *,
    perf: bool = False,
) -> BenchmarkResult:
    """Run one scxsim invocation and measure wall-clock time."""
    simulated_ns = read_workload_duration_ns(workload_path)

    scxsim_cmd = [
        str(SCXSIM), "run", str(workload_path),
        "-s", scheduler,
    ] + mode_flags

    label = f"{workload_path.stem}_{scheduler}_{mode_name}"
    perf_data = f"/tmp/perf_bench_{label}.data"

    if perf:
        cmd = [
            "perf", "record", "-g", "--call-graph", "dwarf",
            "-o", perf_data, "--",
        ] + scxsim_cmd
    else:
        cmd = scxsim_cmd

    env = os.environ.copy()
    env["RUST_LOG"] = "warn"

    start = time.monotonic()
    result = subprocess.run(
        cmd, capture_output=True, text=True, env=env,
    )
    elapsed_s = time.monotonic() - start

    if result.returncode != 0:
        print(f"  FAILED (exit {result.returncode}): {' '.join(cmd)}")
        stderr_preview = result.stderr.strip()[:200]
        if stderr_preview:
            print(f"    stderr: {stderr_preview}")
        return BenchmarkResult(
            workload=workload_path.stem,
            scheduler=scheduler,
            mode=mode_name,
            simulated_ns=simulated_ns,
            wall_clock_ms=elapsed_s * 1000,
            speedup_factor=0.0,
        )

    wall_clock_ms = elapsed_s * 1000
    speedup = simulated_ns / (elapsed_s * 1e9) if elapsed_s > 0 else 0.0

    perf_report = None
    if perf and Path(perf_data).is_file():
        perf_report = _run_perf_report(perf_data)

    return BenchmarkResult(
        workload=workload_path.stem,
        scheduler=scheduler,
        mode=mode_name,
        simulated_ns=simulated_ns,
        wall_clock_ms=wall_clock_ms,
        speedup_factor=speedup,
        perf_report=perf_report,
    )


def should_skip_mode(mode_name: str) -> bool:
    """Check if a mode should be skipped (e.g. e9patch without _e9.so)."""
    if mode_name == "preemptive-e9patch" and not e9_schedulers_available():
        return True
    return False


def run_benchmark_matrix(
    *,
    perf: bool = False,
    filter_mode: Optional[str] = None,
) -> list[BenchmarkResult]:
    """Run the benchmark matrix and return results.

    Args:
        perf: Wrap each run with ``perf record`` and collect profiles.
        filter_mode: If set, only run modes whose name matches this string.
    """
    results: list[BenchmarkResult] = []
    workloads = [WORKLOADS_DIR / name for name in WORKLOAD_NAMES]

    # Validate all workloads exist
    for wl in workloads:
        if not wl.is_file():
            print(f"ERROR: workload not found: {wl}", file=sys.stderr)
            sys.exit(1)

    active_modes = [
        (name, flags) for name, flags in MODES
        if filter_mode is None or name == filter_mode
    ]
    if filter_mode and not active_modes:
        valid = ", ".join(name for name, _ in MODES)
        print(f"ERROR: --filter-mode '{filter_mode}' matches no modes. "
              f"Valid: {valid}", file=sys.stderr)
        sys.exit(1)

    total = len(workloads) * len(SCHEDULERS) * len(active_modes)
    done = 0

    for wl in workloads:
        for scheduler in SCHEDULERS:
            for mode_name, mode_flags in active_modes:
                done += 1
                if should_skip_mode(mode_name):
                    print(f"  [{done}/{total}] SKIP {wl.stem}/{scheduler}/{mode_name}"
                          " (e9patch not available)")
                    continue

                label = f"{wl.stem}/{scheduler}/{mode_name}"
                print(f"  [{done}/{total}] {label} ...", end="", flush=True)

                result = run_single_benchmark(
                    wl, scheduler, mode_name, mode_flags, perf=perf,
                )

                if result.speedup_factor > 0:
                    print(f" {result.speedup_factor:.2f}x"
                          f" ({result.wall_clock_ms:.0f}ms)")
                    if result.perf_report:
                        print(f"\n    --- perf report: {label} ---")
                        for line in result.perf_report.splitlines():
                            print(f"    {line}")
                        print()
                results.append(result)

    return results


# ---------------------------------------------------------------------------
# CSV I/O
# ---------------------------------------------------------------------------

def write_csv(
    results: list[BenchmarkResult],
    csv_path: Path,
    append: bool,
    git_metadata: Optional[dict[str, str]],
) -> None:
    """Write or append benchmark results to a CSV file."""
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    if git_metadata:
        columns = GIT_META_COLUMNS + CSV_COLUMNS
    else:
        columns = CSV_COLUMNS

    file_exists = csv_path.is_file()
    mode = "a" if (append and file_exists) else "w"

    csv_path.parent.mkdir(parents=True, exist_ok=True)

    with open(csv_path, mode, newline="") as f:
        writer = csv.DictWriter(f, fieldnames=columns)
        if mode == "w" or not file_exists:
            writer.writeheader()
        for r in results:
            row = {
                "workload": r.workload,
                "scheduler": r.scheduler,
                "mode": r.mode,
                "simulated_ns": r.simulated_ns,
                "wall_clock_ms": f"{r.wall_clock_ms:.1f}",
                "speedup_factor": f"{r.speedup_factor:.4f}",
            }
            if git_metadata:
                row.update({
                    "timestamp": timestamp,
                    "git_commit": git_metadata["git_commit"],
                    "git_depth": git_metadata["git_depth"],
                    "git_branch": git_metadata["git_branch"],
                    "git_dirty": git_metadata["git_dirty"],
                })
            writer.writerow(row)

    print(f"\nCSV written to: {csv_path}")


# ---------------------------------------------------------------------------
# Plotly visualization: bar chart (current run)
# ---------------------------------------------------------------------------

def generate_bar_chart_html(
    results: list[BenchmarkResult],
    html_path: Path,
) -> None:
    """Generate a grouped bar chart HTML file using Plotly."""
    try:
        import plotly.graph_objects as go  # type: ignore[import-untyped]
        from plotly.subplots import make_subplots  # type: ignore[import-untyped]
    except ImportError:
        print("WARNING: plotly not installed; skipping HTML generation.")
        print("  Install with: .venv/bin/pip install plotly pandas")
        return

    # Filter out failed runs
    valid = [r for r in results if r.speedup_factor > 0]
    if not valid:
        print("WARNING: no successful benchmark results to plot.")
        return

    workloads = sorted(set(r.workload for r in valid))
    # +1 subplot for aggregate
    n_plots = len(workloads) + 1

    subplot_titles = [f"Workload: {wl}" for wl in workloads] + ["Aggregate (avg)"]
    fig = make_subplots(
        rows=n_plots, cols=1,
        subplot_titles=subplot_titles,
        vertical_spacing=0.08,
    )

    schedulers = sorted(set(r.scheduler for r in valid))
    modes = sorted(set(r.mode for r in valid))

    colors = {
        "simple": "#636EFA",
        "lavd": "#EF553B",
        "mitosis": "#00CC96",
        "cosmos": "#AB63FA",
        "tickless": "#FFA15A",
    }

    # Per-workload plots
    for plot_idx, wl in enumerate(workloads):
        wl_results = [r for r in valid if r.workload == wl]
        for sched in schedulers:
            x_vals = []
            y_vals = []
            for mode in modes:
                matching = [r for r in wl_results
                            if r.scheduler == sched and r.mode == mode]
                if matching:
                    x_vals.append(mode)
                    y_vals.append(matching[0].speedup_factor)
            fig.add_trace(
                go.Bar(
                    name=sched,
                    x=x_vals,
                    y=y_vals,
                    marker_color=colors.get(sched, "#999"),
                    legendgroup=sched,
                    showlegend=(plot_idx == 0),
                ),
                row=plot_idx + 1, col=1,
            )
        fig.update_yaxes(title_text="Speedup factor", row=plot_idx + 1, col=1)

    # Aggregate plot: average across workloads
    for sched in schedulers:
        x_vals = []
        y_vals = []
        for mode in modes:
            matching = [r for r in valid
                        if r.scheduler == sched and r.mode == mode]
            if matching:
                avg = sum(r.speedup_factor for r in matching) / len(matching)
                x_vals.append(mode)
                y_vals.append(avg)
        fig.add_trace(
            go.Bar(
                name=sched,
                x=x_vals,
                y=y_vals,
                marker_color=colors.get(sched, "#999"),
                legendgroup=sched,
                showlegend=False,
            ),
            row=n_plots, col=1,
        )
    fig.update_yaxes(title_text="Speedup factor", row=n_plots, col=1)

    fig.update_layout(
        title_text="scx_simulator Throughput Benchmarks",
        barmode="group",
        template="plotly_dark",
        height=400 * n_plots,
        width=1000,
    )

    html_path.parent.mkdir(parents=True, exist_ok=True)
    fig.write_html(str(html_path), include_plotlyjs="cdn")
    print(f"Bar chart written to: {html_path}")


# ---------------------------------------------------------------------------
# Plotly visualization: time-series (history)
# ---------------------------------------------------------------------------

def generate_history_html(csv_path: Path, html_path: Path) -> None:
    """Generate a time-series dashboard from historical benchmark CSV."""
    try:
        import pandas as pd  # type: ignore[import-untyped]
        import plotly.graph_objects as go
        from plotly.subplots import make_subplots
    except ImportError:
        print("ERROR: plotly/pandas not installed.", file=sys.stderr)
        print("  Install with: .venv/bin/pip install plotly pandas", file=sys.stderr)
        sys.exit(1)

    if not csv_path.is_file():
        print(f"ERROR: CSV file not found: {csv_path}", file=sys.stderr)
        sys.exit(1)

    df = pd.read_csv(csv_path)

    if "git_depth" not in df.columns:
        print("ERROR: CSV missing git_depth column (need --git-metadata).",
              file=sys.stderr)
        sys.exit(1)

    # Create a composite key for each benchmark configuration
    df["config"] = df["scheduler"] + "/" + df["mode"]
    workloads = sorted(df["workload"].unique())
    configs = sorted(df["config"].unique())

    n_plots = len(workloads) + 1  # +1 for aggregate
    subplot_titles = [f"Workload: {wl}" for wl in workloads] + ["Aggregate (avg)"]

    fig = make_subplots(
        rows=n_plots, cols=1,
        subplot_titles=subplot_titles,
        vertical_spacing=0.06,
    )

    for plot_idx, wl in enumerate(workloads):
        wl_df = df[df["workload"] == wl]
        for config in configs:
            cfg_df = wl_df[wl_df["config"] == config].sort_values("git_depth")
            if cfg_df.empty:
                continue
            hover = [
                f"commit: {c}<br>depth: {d}<br>speedup: {s:.2f}x"
                for c, d, s in zip(
                    cfg_df["git_commit"],
                    cfg_df["git_depth"],
                    cfg_df["speedup_factor"],
                )
            ]
            fig.add_trace(
                go.Scatter(
                    x=cfg_df["git_depth"],
                    y=cfg_df["speedup_factor"],
                    mode="lines+markers",
                    name=config,
                    legendgroup=config,
                    showlegend=(plot_idx == 0),
                    hovertext=hover,
                    hoverinfo="text",
                ),
                row=plot_idx + 1, col=1,
            )
        fig.update_xaxes(title_text="Git depth", row=plot_idx + 1, col=1)
        fig.update_yaxes(title_text="Speedup factor", row=plot_idx + 1, col=1)

    # Aggregate
    agg = df.groupby(["git_depth", "git_commit", "config"])["speedup_factor"].mean()
    agg = agg.reset_index()
    for config in configs:
        cfg_agg = agg[agg["config"] == config].sort_values("git_depth")
        if cfg_agg.empty:
            continue
        fig.add_trace(
            go.Scatter(
                x=cfg_agg["git_depth"],
                y=cfg_agg["speedup_factor"],
                mode="lines+markers",
                name=config,
                legendgroup=config,
                showlegend=False,
            ),
            row=n_plots, col=1,
        )
    fig.update_xaxes(title_text="Git depth", row=n_plots, col=1)
    fig.update_yaxes(title_text="Speedup factor", row=n_plots, col=1)

    fig.update_layout(
        title_text="scx_simulator Throughput Over Time",
        template="plotly_dark",
        height=350 * n_plots,
        width=1100,
    )

    html_path.parent.mkdir(parents=True, exist_ok=True)
    fig.write_html(str(html_path), include_plotlyjs="cdn")
    print(f"History dashboard written to: {html_path}")


# ---------------------------------------------------------------------------
# Perf: mutex overhead summary
# ---------------------------------------------------------------------------

def _extract_mutex_overhead(report: str) -> list[tuple[float, str]]:
    """Parse perf report lines and return (percent, symbol) for mutex hits.

    Each interesting line looks like:
        4.20%   12345  scxsim  libfoo.so  [.] some::function
    We extract the percent and symbol for lines matching MUTEX_PATTERNS.
    """
    pct_re = re.compile(r"^\s*(\d+\.\d+)%\s+.+\]\s+(.+)$")
    hits: list[tuple[float, str]] = []
    for line in report.splitlines():
        m = pct_re.match(line)
        if not m:
            continue
        pct, symbol = float(m.group(1)), m.group(2).strip()
        if _MUTEX_RE.search(symbol):
            hits.append((pct, symbol))
    return hits


def print_mutex_overhead_summary(results: list[BenchmarkResult]) -> None:
    """Print a table of mutex/lock overhead per benchmark."""
    profiled = [(r, r.perf_report) for r in results if r.perf_report]
    if not profiled:
        return

    print("\n=== Mutex / Lock Overhead Summary ===\n")
    print(f"{'Benchmark':<45} {'Mutex %':>8}  Top mutex symbols")
    print("-" * 100)

    for result, report in profiled:
        label = f"{result.workload}/{result.scheduler}/{result.mode}"
        hits = _extract_mutex_overhead(report)
        total_pct = sum(pct for pct, _ in hits)
        top_syms = ", ".join(
            f"{sym} ({pct:.1f}%)" for pct, sym in sorted(hits, reverse=True)[:3]
        )
        print(f"{label:<45} {total_pct:>7.2f}%  {top_syms or '(none)'}")

    overall_pcts = []
    for _, report in profiled:
        hits = _extract_mutex_overhead(report)
        overall_pcts.append(sum(pct for pct, _ in hits))
    if overall_pcts:
        avg = sum(overall_pcts) / len(overall_pcts)
        print("-" * 100)
        print(f"{'Average across benchmarks':<45} {avg:>7.2f}%")
    print()


# ---------------------------------------------------------------------------
# Subcommand: run
# ---------------------------------------------------------------------------

def cmd_run(args: argparse.Namespace) -> int:
    """Execute the benchmark suite."""
    if not args.no_build:
        print("=== Building scxsim (release) ===")
        result = subprocess.run(
            ["cargo", "build", "--release"],
            cwd=str(PROJECT_ROOT),
        )
        if result.returncode != 0:
            print("ERROR: cargo build failed", file=sys.stderr)
            return 1
        print()

    if not SCXSIM.is_file():
        print(f"ERROR: {SCXSIM} not found", file=sys.stderr)
        return 1

    use_perf = getattr(args, "perf", False)
    filter_mode = getattr(args, "filter_mode", None)

    print("=== Running benchmark matrix ===")
    results = run_benchmark_matrix(perf=use_perf, filter_mode=filter_mode)

    # Filter to successful results for summary
    successful = [r for r in results if r.speedup_factor > 0]
    failed = [r for r in results if r.speedup_factor == 0]

    print(f"\n=== Results: {len(successful)} passed, {len(failed)} failed ===")

    if use_perf:
        print_mutex_overhead_summary(results)

    # Write CSV
    csv_path = Path(args.csv) if args.csv else PROJECT_ROOT / "benchmark_results.csv"
    git_meta = get_git_metadata() if args.git_metadata else None
    write_csv(results, csv_path, append=args.append, git_metadata=git_meta)

    # Generate bar chart HTML
    html_path = Path(args.html) if args.html else csv_path.with_suffix(".html")
    generate_bar_chart_html(results, html_path)

    return 0


# ---------------------------------------------------------------------------
# Subcommand: plot-history
# ---------------------------------------------------------------------------

def cmd_plot_history(args: argparse.Namespace) -> int:
    """Generate time-series dashboard from historical CSV."""
    csv_path = Path(args.csv_file)
    html_path = Path(args.output) if args.output else csv_path.with_suffix(".html")
    generate_history_html(csv_path, html_path)
    return 0


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(
        description="scx_simulator throughput benchmarks",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # --- run ---
    run_parser = subparsers.add_parser("run", help="Run benchmark suite")
    run_parser.add_argument(
        "--no-build", action="store_true",
        help="Skip cargo build --release",
    )
    run_parser.add_argument(
        "--csv", type=str, default=None,
        help="Output CSV path (default: benchmark_results.csv)",
    )
    run_parser.add_argument(
        "--html", type=str, default=None,
        help="Output HTML path (default: same as CSV with .html)",
    )
    run_parser.add_argument(
        "--append", action="store_true",
        help="Append to existing CSV instead of overwriting",
    )
    run_parser.add_argument(
        "--git-metadata", action="store_true",
        help="Include git commit/depth/branch/dirty columns in CSV",
    )
    run_parser.add_argument(
        "--perf", action="store_true",
        help="Profile each benchmark with perf record and show hottest functions",
    )
    run_parser.add_argument(
        "--filter-mode", type=str, default=None, dest="filter_mode",
        help="Only run benchmarks for this mode (e.g. sequential, interleave)",
    )

    # --- plot-history ---
    history_parser = subparsers.add_parser(
        "plot-history", help="Generate time-series dashboard from CSV",
    )
    history_parser.add_argument(
        "csv_file", type=str,
        help="Path to historical perf_history.csv",
    )
    history_parser.add_argument(
        "--output", "-o", type=str, default=None,
        help="Output HTML path (default: same as CSV with .html)",
    )

    args = parser.parse_args()

    if args.command == "run":
        return cmd_run(args)
    elif args.command == "plot-history":
        return cmd_plot_history(args)
    else:
        parser.print_help()
        return 1


if __name__ == "__main__":
    sys.exit(main())
