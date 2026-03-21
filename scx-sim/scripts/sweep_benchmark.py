#!/usr/bin/env python3
"""
2D parameter sweep benchmark: varies timeslice AND concurrency level.

Runs a matrix of (timeslice, thread_count) combinations, measures per-simulation
wall-clock time, and generates Plotly visualizations.

Usage:
    python3 scripts/sweep_benchmark.py \
      --scheduler lavd \
      --workload dsq_contention \
      --timeslices 1,50,100,200,500,1000,2000 \
      --threads 1,2,4,8,16 \
      --end-time 200ms \
      --reps 3 \
      --csv debug/sweep_results.csv \
      --html debug/sweep_plots.html
"""

import argparse
import csv
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Project layout
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).resolve().parent.parent
SCXSIM = PROJECT_ROOT / "target" / "release" / "scxsim"
WORKLOADS_DIR = PROJECT_ROOT / "crates" / "scx_simulator" / "workloads"

# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------

SWEEP_CSV_COLUMNS = [
    "timeslice", "threads", "rep",
    "wall_ms", "seed",
]

SUMMARY_CSV_COLUMNS = [
    "timeslice", "threads",
    "avg_wall_ms", "median_wall_ms", "p95_wall_ms",
    "min_wall_ms", "max_wall_ms", "total_wall_ms",
]


@dataclass(frozen=True)
class SweepConfig:
    """Immutable configuration for a single simulation run."""
    workload_path: str
    scheduler: str
    cpus: int
    timeslice: int
    seed: int
    end_time: str


@dataclass
class RunResult:
    """Result of a single simulation run."""
    timeslice: int
    threads: int
    rep: int
    seed: int
    wall_ms: float
    success: bool


# ---------------------------------------------------------------------------
# Single-run execution (must be top-level for ProcessPoolExecutor)
# ---------------------------------------------------------------------------

def _run_single_sim(config: SweepConfig) -> float:
    """Run one scxsim simulation, return wall-clock milliseconds.

    Raises subprocess.CalledProcessError on failure.
    """
    cmd = [
        str(SCXSIM), "run", config.workload_path,
        "-s", config.scheduler,
        "-c", str(config.cpus),
        "--seed", str(config.seed),
        "--end-time", config.end_time,
        "--preemptive",
        "--timeslice-min", str(config.timeslice),
        "--timeslice-max", str(config.timeslice),
    ]
    env = os.environ.copy()
    env["RUST_LOG"] = "warn"

    start = time.monotonic()
    result = subprocess.run(cmd, capture_output=True, text=True, env=env)
    elapsed_ms = (time.monotonic() - start) * 1000

    if result.returncode != 0:
        stderr_preview = result.stderr.strip()[:300]
        raise RuntimeError(
            f"scxsim failed (exit {result.returncode}) "
            f"ts={config.timeslice} seed={config.seed}: {stderr_preview}"
        )
    return elapsed_ms


# ---------------------------------------------------------------------------
# Batch execution with concurrency control
# ---------------------------------------------------------------------------

def run_batch_at_concurrency(
    configs: list[SweepConfig],
    threads: int,
    timeslice: int,
) -> list[RunResult]:
    """Run a batch of simulations at a given concurrency level.

    Returns one RunResult per config, preserving rep ordering.
    """
    results: list[RunResult] = []

    if threads <= 1:
        # Sequential: no process pool overhead
        for rep_idx, cfg in enumerate(configs):
            try:
                wall_ms = _run_single_sim(cfg)
                success = True
            except (RuntimeError, subprocess.CalledProcessError) as e:
                print(f"  FAILED: {e}", file=sys.stderr)
                wall_ms = 0.0
                success = False
            results.append(RunResult(
                timeslice=timeslice, threads=threads,
                rep=rep_idx, seed=cfg.seed, wall_ms=wall_ms, success=success,
            ))
        return results

    # Parallel execution
    with ProcessPoolExecutor(max_workers=threads) as executor:
        future_to_rep = {
            executor.submit(_run_single_sim, cfg): rep_idx
            for rep_idx, cfg in enumerate(configs)
        }
        rep_results: dict[int, RunResult] = {}
        for future in as_completed(future_to_rep):
            rep_idx = future_to_rep[future]
            cfg = configs[rep_idx]
            try:
                wall_ms = future.result()
                success = True
            except Exception as e:
                print(f"  FAILED rep {rep_idx}: {e}", file=sys.stderr)
                wall_ms = 0.0
                success = False
            rep_results[rep_idx] = RunResult(
                timeslice=timeslice, threads=threads,
                rep=rep_idx, seed=cfg.seed, wall_ms=wall_ms, success=success,
            )
        # Return in rep order
        results = [rep_results[i] for i in range(len(configs))]

    return results


# ---------------------------------------------------------------------------
# Summary statistics
# ---------------------------------------------------------------------------

@dataclass
class SweepSummary:
    """Aggregated statistics for one (timeslice, threads) pair."""
    timeslice: int
    threads: int
    avg_wall_ms: float
    median_wall_ms: float
    p95_wall_ms: float
    min_wall_ms: float
    max_wall_ms: float
    total_wall_ms: float


def compute_summary(run_results: list[RunResult]) -> Optional[SweepSummary]:
    """Compute summary statistics from successful runs."""
    times = [r.wall_ms for r in run_results if r.success]
    if not times:
        return None
    times_sorted = sorted(times)
    p95_idx = min(int(len(times_sorted) * 0.95), len(times_sorted) - 1)
    return SweepSummary(
        timeslice=run_results[0].timeslice,
        threads=run_results[0].threads,
        avg_wall_ms=statistics.mean(times),
        median_wall_ms=statistics.median(times),
        p95_wall_ms=times_sorted[p95_idx],
        min_wall_ms=min(times),
        max_wall_ms=max(times),
        total_wall_ms=sum(times),
    )


# ---------------------------------------------------------------------------
# CSV output
# ---------------------------------------------------------------------------

def write_raw_csv(results: list[RunResult], csv_path: Path) -> None:
    """Write per-run results to CSV."""
    csv_path.parent.mkdir(parents=True, exist_ok=True)
    with open(csv_path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=SWEEP_CSV_COLUMNS)
        writer.writeheader()
        for r in results:
            if r.success:
                writer.writerow({
                    "timeslice": r.timeslice,
                    "threads": r.threads,
                    "rep": r.rep,
                    "wall_ms": f"{r.wall_ms:.2f}",
                    "seed": r.seed,
                })
    print(f"Raw CSV written to: {csv_path}")


def write_summary_csv(summaries: list[SweepSummary], csv_path: Path) -> None:
    """Write summary statistics to CSV."""
    csv_path.parent.mkdir(parents=True, exist_ok=True)
    with open(csv_path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_CSV_COLUMNS)
        writer.writeheader()
        for s in summaries:
            writer.writerow({
                "timeslice": s.timeslice,
                "threads": s.threads,
                "avg_wall_ms": f"{s.avg_wall_ms:.2f}",
                "median_wall_ms": f"{s.median_wall_ms:.2f}",
                "p95_wall_ms": f"{s.p95_wall_ms:.2f}",
                "min_wall_ms": f"{s.min_wall_ms:.2f}",
                "max_wall_ms": f"{s.max_wall_ms:.2f}",
                "total_wall_ms": f"{s.total_wall_ms:.2f}",
            })
    print(f"Summary CSV written to: {csv_path}")


# ---------------------------------------------------------------------------
# Plotly visualization
# ---------------------------------------------------------------------------

def _downsample_timeslices(
    all_timeslices: list[int],
    max_lines: int = 10,
) -> list[int]:
    """Select up to max_lines representative timeslice values.

    Keeps extremes and evenly-spaced samples from the middle.
    """
    if len(all_timeslices) <= max_lines:
        return all_timeslices
    ts_sorted = sorted(all_timeslices)
    # Always keep first two and last two
    keep = {ts_sorted[0], ts_sorted[1], ts_sorted[-2], ts_sorted[-1]}
    # Fill remaining slots evenly from the middle
    remaining = max_lines - len(keep)
    middle = [t for t in ts_sorted if t not in keep]
    if remaining > 0 and middle:
        step = max(1, len(middle) // remaining)
        for i in range(0, len(middle), step):
            keep.add(middle[i])
            if len(keep) >= max_lines:
                break
    return sorted(keep)


def generate_sweep_html(
    summaries: list[SweepSummary],
    html_path: Path,
) -> None:
    """Generate a single HTML with 3 sweep plots."""
    try:
        import plotly.graph_objects as go  # type: ignore[import-not-found]
        from plotly.subplots import make_subplots  # type: ignore[import-not-found]
    except ImportError:
        print("WARNING: plotly not installed; skipping HTML generation.")
        print("  Install with: .venv/bin/pip install plotly pandas")
        return

    if not summaries:
        print("WARNING: no summary data to plot.")
        return

    all_timeslices = sorted(set(s.timeslice for s in summaries))
    all_threads = sorted(set(s.threads for s in summaries))

    # Build lookup: (timeslice, threads) -> SweepSummary
    lookup: dict[tuple[int, int], SweepSummary] = {
        (s.timeslice, s.threads): s for s in summaries
    }

    fig = make_subplots(
        rows=3, cols=1,
        subplot_titles=[
            "Per-Simulation Cost vs Timeslice (by Thread Count)",
            "Per-Simulation Cost vs Thread Count (by Timeslice)",
            "Simulation Cost Heatmap (Thread Count x Timeslice)",
        ],
        vertical_spacing=0.10,
        specs=[[{"type": "scatter"}], [{"type": "scatter"}], [{"type": "scatter"}]],
    )

    # --- Plot 1: Lines by thread count (X = timeslice) ---
    for threads in all_threads:
        x_vals = []
        y_vals = []
        for ts in all_timeslices:
            key = (ts, threads)
            if key in lookup:
                x_vals.append(ts)
                y_vals.append(lookup[key].avg_wall_ms)
        if x_vals:
            fig.add_trace(
                go.Scatter(
                    x=x_vals, y=y_vals,
                    mode="lines+markers",
                    name=f"{threads} threads",
                    legendgroup=f"t{threads}",
                    showlegend=True,
                ),
                row=1, col=1,
            )
    fig.update_xaxes(title_text="Timeslice (retired conditional branches)", row=1, col=1)
    fig.update_yaxes(title_text="Avg wall-clock per sim (ms)", row=1, col=1)

    # --- Plot 2: Lines by timeslice (X = thread count), downsampled ---
    sampled_ts = _downsample_timeslices(all_timeslices)
    for ts in sampled_ts:
        x_vals = []
        y_vals = []
        for threads in all_threads:
            key = (ts, threads)
            if key in lookup:
                x_vals.append(threads)
                y_vals.append(lookup[key].avg_wall_ms)
        if x_vals:
            fig.add_trace(
                go.Scatter(
                    x=x_vals, y=y_vals,
                    mode="lines+markers",
                    name=f"ts={ts}",
                    legendgroup=f"ts{ts}",
                    showlegend=True,
                ),
                row=2, col=1,
            )
    fig.update_xaxes(
        title_text="Thread count", type="log", row=2, col=1,
    )
    fig.update_yaxes(title_text="Avg wall-clock per sim (ms)", row=2, col=1)

    # --- Plot 3: Heatmap scatter ---
    x_heat = []
    y_heat = []
    z_heat = []
    for s in summaries:
        x_heat.append(s.threads)
        y_heat.append(s.timeslice)
        z_heat.append(s.avg_wall_ms)

    fig.add_trace(
        go.Scatter(
            x=x_heat, y=y_heat,
            mode="markers",
            marker=dict(
                size=14,
                color=z_heat,
                colorscale="RdYlGn_r",  # green=fast, red=slow
                showscale=True,
                colorbar=dict(title="Avg wall ms", x=1.02),
            ),
            text=[f"ts={y}, threads={x}, avg={z:.1f}ms"
                  for x, y, z in zip(x_heat, y_heat, z_heat)],
            hoverinfo="text",
            showlegend=False,
        ),
        row=3, col=1,
    )
    fig.update_xaxes(
        title_text="Thread count", type="log", row=3, col=1,
    )
    fig.update_yaxes(
        title_text="Timeslice (retired conditional branches)",
        type="log", row=3, col=1,
    )

    fig.update_layout(
        title_text="2D Parameter Sweep: Timeslice x Concurrency",
        template="plotly_dark",
        height=1500,
        width=1100,
    )

    html_path.parent.mkdir(parents=True, exist_ok=True)
    fig.write_html(str(html_path), include_plotlyjs="cdn")
    print(f"Sweep plots written to: {html_path}")


# ---------------------------------------------------------------------------
# Main sweep orchestration
# ---------------------------------------------------------------------------

def resolve_workload_path(workload_name: str) -> Path:
    """Resolve a workload name to its full path."""
    # Try as-is first (absolute or relative path)
    p = Path(workload_name)
    if p.is_file():
        return p
    # Try with .json extension
    p_json = p.with_suffix(".json")
    if p_json.is_file():
        return p_json
    # Try in workloads directory
    wl = WORKLOADS_DIR / workload_name
    if wl.is_file():
        return wl
    wl_json = WORKLOADS_DIR / f"{workload_name}.json"
    if wl_json.is_file():
        return wl_json
    print(f"ERROR: workload not found: {workload_name}", file=sys.stderr)
    print(f"  Searched: {p}, {p_json}, {wl}, {wl_json}", file=sys.stderr)
    sys.exit(1)


def parse_int_list(s: str) -> list[int]:
    """Parse a comma-separated list of integers."""
    return [int(x.strip()) for x in s.split(",") if x.strip()]


def run_sweep(args: argparse.Namespace) -> int:
    """Execute the full 2D parameter sweep."""
    workload_path = resolve_workload_path(args.workload)
    timeslices = parse_int_list(args.timeslices)
    thread_levels = parse_int_list(args.threads)
    reps = args.reps
    cpus = args.cpus
    end_time = args.end_time

    if not SCXSIM.is_file():
        if not args.no_build:
            print("=== Building scxsim (release) ===")
            result = subprocess.run(
                ["cargo", "build", "--release"],
                cwd=str(PROJECT_ROOT),
            )
            if result.returncode != 0:
                print("ERROR: cargo build failed", file=sys.stderr)
                return 1
        else:
            print(f"ERROR: {SCXSIM} not found (use without --no-build)", file=sys.stderr)
            return 1

    total_combos = len(timeslices) * len(thread_levels)
    total_sims = total_combos * reps
    print(f"=== 2D Sweep: {len(timeslices)} timeslices x "
          f"{len(thread_levels)} thread levels x {reps} reps = "
          f"{total_sims} simulations ===")
    print(f"  Scheduler:  {args.scheduler}")
    print(f"  Workload:   {workload_path.name}")
    print(f"  CPUs:       {cpus}")
    print(f"  End time:   {end_time}")
    print(f"  Timeslices: {timeslices}")
    print(f"  Threads:    {thread_levels}")
    print()

    all_results: list[RunResult] = []
    all_summaries: list[SweepSummary] = []
    combo_idx = 0
    sweep_start = time.monotonic()

    for ts in timeslices:
        for threads in thread_levels:
            combo_idx += 1
            label = f"[{combo_idx}/{total_combos}] ts={ts} threads={threads}"
            print(f"  {label} ...", end="", flush=True)

            # Build configs with unique seeds
            configs = [
                SweepConfig(
                    workload_path=str(workload_path),
                    scheduler=args.scheduler,
                    cpus=cpus,
                    timeslice=ts,
                    seed=42 + rep + ts * 1000 + threads * 100000,
                    end_time=end_time,
                )
                for rep in range(reps)
            ]

            batch_start = time.monotonic()
            batch_results = run_batch_at_concurrency(configs, threads, ts)
            batch_elapsed = (time.monotonic() - batch_start) * 1000

            all_results.extend(batch_results)
            summary = compute_summary(batch_results)
            if summary:
                all_summaries.append(summary)
                print(f" avg={summary.avg_wall_ms:.1f}ms "
                      f"median={summary.median_wall_ms:.1f}ms "
                      f"(batch {batch_elapsed:.0f}ms)")
            else:
                print(" ALL FAILED")

    sweep_elapsed = time.monotonic() - sweep_start
    print(f"\n=== Sweep complete: {len(all_summaries)}/{total_combos} combos "
          f"in {sweep_elapsed:.1f}s ===")

    # Write CSVs
    csv_path = Path(args.csv)
    write_raw_csv(all_results, csv_path)
    summary_csv = csv_path.with_stem(csv_path.stem + "_summary")
    write_summary_csv(all_summaries, summary_csv)

    # Generate plots
    html_path = Path(args.html)
    generate_sweep_html(all_summaries, html_path)

    # Print analysis
    print_sweep_analysis(all_summaries, thread_levels)

    return 0


# ---------------------------------------------------------------------------
# Analysis output
# ---------------------------------------------------------------------------

def print_sweep_analysis(
    summaries: list[SweepSummary],
    thread_levels: list[int],
) -> None:
    """Print key findings from the sweep."""
    if not summaries:
        return

    print("\n" + "=" * 70)
    print("SWEEP ANALYSIS")
    print("=" * 70)

    # Best timeslice per thread count
    print("\n--- Best timeslice per concurrency level ---")
    by_threads: dict[int, list[SweepSummary]] = {}
    for s in summaries:
        by_threads.setdefault(s.threads, []).append(s)

    for threads in sorted(by_threads):
        best = min(by_threads[threads], key=lambda s: s.avg_wall_ms)
        print(f"  threads={threads:>3}: best ts={best.timeslice:>5} "
              f"({best.avg_wall_ms:.1f}ms avg)")

    # Degradation point per timeslice
    print("\n--- Degradation onset per timeslice ---")
    by_ts: dict[int, list[SweepSummary]] = {}
    for s in summaries:
        by_ts.setdefault(s.timeslice, []).append(s)

    for ts in sorted(by_ts):
        entries = sorted(by_ts[ts], key=lambda s: s.threads)
        if len(entries) < 2:
            continue
        baseline = entries[0].avg_wall_ms
        degradation_at = None
        for entry in entries[1:]:
            if entry.avg_wall_ms > baseline * 1.10:  # 10% degradation
                degradation_at = entry.threads
                break
        if degradation_at:
            print(f"  ts={ts:>5}: degrades (>10%) at threads={degradation_at}")
        else:
            print(f"  ts={ts:>5}: no degradation observed")

    # Overall sweet spot
    print("\n--- Overall sweet spot ---")
    best_overall = min(summaries, key=lambda s: s.avg_wall_ms)
    print(f"  Best avg per-sim cost: ts={best_overall.timeslice}, "
          f"threads={best_overall.threads} -> {best_overall.avg_wall_ms:.1f}ms")

    # Best throughput (lowest total_wall_ms for the batch)
    best_throughput = min(summaries, key=lambda s: s.total_wall_ms)
    print(f"  Best batch throughput: ts={best_throughput.timeslice}, "
          f"threads={best_throughput.threads} -> "
          f"{best_throughput.total_wall_ms:.1f}ms total")
    print()


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(
        description="2D parameter sweep benchmark for scx_simulator",
    )
    parser.add_argument(
        "--scheduler", "-s", type=str, default="lavd",
        help="Scheduler name (default: lavd)",
    )
    parser.add_argument(
        "--workload", "-w", type=str, default="dsq_contention",
        help="Workload name or path (default: dsq_contention)",
    )
    parser.add_argument(
        "--timeslices", type=str,
        default="1,50,100,200,300,400,500,600,700,800,900,1000,1500,2000",
        help="Comma-separated timeslice values (retired conditional branches)",
    )
    parser.add_argument(
        "--threads", type=str, default="1,2,4,8,16",
        help="Comma-separated thread/concurrency levels",
    )
    parser.add_argument(
        "--end-time", type=str, default="200ms",
        help="Simulation end time (default: 200ms)",
    )
    parser.add_argument(
        "--reps", type=int, default=3,
        help="Repetitions per (timeslice, threads) pair (default: 3)",
    )
    parser.add_argument(
        "--cpus", type=int, default=4,
        help="Number of simulated CPUs (default: 4)",
    )
    parser.add_argument(
        "--csv", type=str, default="debug/sweep_results.csv",
        help="Output CSV path (default: debug/sweep_results.csv)",
    )
    parser.add_argument(
        "--html", type=str, default="debug/sweep_plots.html",
        help="Output HTML path (default: debug/sweep_plots.html)",
    )
    parser.add_argument(
        "--no-build", action="store_true",
        help="Skip cargo build --release",
    )

    args = parser.parse_args()
    return run_sweep(args)


if __name__ == "__main__":
    sys.exit(main())
