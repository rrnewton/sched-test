#!/usr/bin/env python3
"""
Stress test for scx_simulator.

Runs randomized simulation configurations in parallel, searching for stalls,
BPF errors, crashes, and other failures. All findings are reported immediately
and written to bug_finding/output/ for later analysis.

Usage:
    python3 stress.py                  # Run for 10 minutes (default)
    python3 stress.py --duration 30    # Run for 30 minutes
    python3 stress.py --jobs 8         # Use 8 parallel workers
    python3 stress.py --determinism    # Enable determinism checking
    python3 stress.py --e9patch        # Only test e9patch mode
"""

import argparse
import logging
import os
import random
import signal
import subprocess
import tempfile
import sys
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).parent.parent
SCXSIM = PROJECT_ROOT / "target" / "release" / "scxsim"
WORKLOADS_DIR = PROJECT_ROOT / "crates" / "scx_simulator" / "workloads"
OUTPUT_DIR = Path(__file__).parent / "output"

SCHEDULERS = ["simple", "lavd", "cosmos", "tickless", "mitosis"]
CPU_COUNTS = [1, 2, 4, 8]
INTERLEAVE_MODES = ["off", "cooperative", "preemptive", "e9patch"]


def get_available_workloads() -> list[Path]:
    """Return sorted list of available workload files."""
    return sorted(WORKLOADS_DIR.glob("*.json"))


def e9_schedulers_available() -> bool:
    """Check if _e9.so scheduler variants exist (built via `make e9`)."""
    # Look for at least one _e9.so in the scheduler build dir.
    sched_dirs = list(
        PROJECT_ROOT.glob("target/release/build/scx_simulator-*/out/schedulers")
    )
    if not sched_dirs:
        return False
    return any(sched_dirs[0].glob("*_e9.so"))


# Defaults for stall detection
DEFAULT_WATCHDOG_TIMEOUT = "2s"
DEFAULT_SIM_DURATION = "4s"

# Process timeout (wall-clock) — generous to avoid false positives
PROCESS_TIMEOUT_SEC = 120

# Number of repeat runs for e9patch determinism checking
E9_DETERMINISM_REPEATS = 3

# Runtime config (set from CLI args in main)
WATCHDOG_TIMEOUT = DEFAULT_WATCHDOG_TIMEOUT
SIM_DURATION = DEFAULT_SIM_DURATION
DETERMINISM_MODE = False

# Global logger (configured in main)
log: logging.Logger = logging.getLogger("stress")


def setup_logging() -> Path:
    """Set up logging to both console and a timestamped file."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    timestamp = datetime.now().strftime("%Y-%m-%d_%H%M%S")
    log_path = OUTPUT_DIR / f"stress_{timestamp}.log"

    # Configure root logger
    log.setLevel(logging.DEBUG)

    # File handler - detailed
    file_handler = logging.FileHandler(log_path)
    file_handler.setLevel(logging.DEBUG)
    file_handler.setFormatter(logging.Formatter(
        "%(asctime)s %(levelname)s %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S"
    ))
    log.addHandler(file_handler)

    # Console handler - minimal (we do our own progress display)
    console_handler = logging.StreamHandler()
    console_handler.setLevel(logging.WARNING)
    console_handler.setFormatter(logging.Formatter("%(message)s"))
    log.addHandler(console_handler)

    return log_path


@dataclass
class TestConfig:
    """A single stress test configuration."""

    scheduler: str
    workload: Path
    cpus: int
    seed: int
    interleave_mode: str  # "off", "cooperative", "preemptive", "e9patch"
    iteration: int

    @property
    def label(self) -> str:
        wl = self.workload.stem
        return (
            f"{self.scheduler}/{wl}/c{self.cpus}"
            f"/s{self.seed}/{self.interleave_mode}"
        )


@dataclass
class Finding:
    """A bug finding from a stress test run."""

    config: TestConfig
    error_type: str  # "crash", "stall", "bpf_error", "timeout", "other"
    exit_code: int
    stderr: str
    stdout: str
    wall_time_sec: float
    timestamp: str = field(default_factory=lambda: datetime.now().isoformat())

    def summary(self) -> str:
        return f"[{self.error_type}] {self.config.label} (exit={self.exit_code})"

    def report(self) -> str:
        lines = [
            f"Finding: {self.error_type}",
            f"Timestamp: {self.timestamp}",
            f"Wall time: {self.wall_time_sec:.2f}s",
            f"Exit code: {self.exit_code}",
            "",
            f"Scheduler: {self.config.scheduler}",
            f"Workload: {self.config.workload.resolve()}",
            f"Scxsim: {SCXSIM.resolve()}",
            f"CPUs: {self.config.cpus}",
            f"Seed: {self.config.seed}",
            f"Interleave: {self.config.interleave_mode}",
            "",
            "--- Reproduction command ---",
            self.repro_command(),
            "",
            "--- stderr ---",
            self.stderr or "(empty)",
            "",
            "--- stdout ---",
            self.stdout or "(empty)",
        ]
        return "\n".join(lines)

    def repro_command(self) -> str:
        cmd = build_base_cmd(self.config)
        if self.error_type == "determinism":
            cmd.append("--determinism-check")
        return " ".join(cmd)


# ---------------------------------------------------------------------------
# Test execution
# ---------------------------------------------------------------------------


def build_base_cmd(config: TestConfig) -> list[str]:
    """Build the base scxsim command for a configuration."""
    cmd = [
        str(SCXSIM),
        "run",
        str(config.workload),
        "-s", config.scheduler,
        "-c", str(config.cpus),
        "--seed", str(config.seed),
        "--watchdog-timeout", WATCHDOG_TIMEOUT,
        "--end-time", SIM_DURATION,
    ]
    if config.interleave_mode == "cooperative":
        cmd.append("--interleave")
    elif config.interleave_mode == "preemptive":
        cmd.append("--preemptive")
    elif config.interleave_mode == "e9patch":
        cmd.extend(["--preemptive", "--preempt-mode", "e9patch"])
    return cmd


def classify_error(returncode: int, stderr: str) -> str:
    """Classify an error based on return code and stderr output."""
    if returncode < 0:
        signum = -returncode
        sig_name = signal.Signals(signum).name
        return f"crash({sig_name})"
    elif "DETERMINISM FAILURE" in stderr:
        return "determinism"
    elif "ErrorStall" in stderr:
        return "stall"
    elif "ErrorBpf" in stderr:
        return "bpf_error"
    elif "ErrorDispatchLoopExhausted" in stderr:
        return "dispatch_loop"
    elif "ErrorCgroupExhausted" in stderr:
        return "cgroup_exhausted"
    else:
        return "other"


def run_determinism_preemptive(config: TestConfig) -> Optional[Finding]:
    """Record preemption points in run 1, replay them in run 2, compare."""
    start = time.monotonic()
    tmpfile = None
    try:
        tmpfile = tempfile.NamedTemporaryFile(
            suffix=".preempt", delete=False, prefix="scxsim_"
        )
        tmpfile.close()

        # Run 1: record preemption points (nondeterministic PMU)
        cmd1 = build_base_cmd(config) + ["--record-preemptions", tmpfile.name]
        result1 = subprocess.run(
            cmd1, capture_output=True, text=True, timeout=PROCESS_TIMEOUT_SEC
        )
        if result1.returncode != 0:
            elapsed = time.monotonic() - start
            error_type = classify_error(result1.returncode, result1.stderr)
            return Finding(
                config=config,
                error_type=f"record_{error_type}",
                exit_code=result1.returncode,
                stderr=result1.stderr.strip(),
                stdout=result1.stdout.strip(),
                wall_time_sec=elapsed,
            )

        # Run 2: replay preemption points (deterministic hw breakpoint)
        cmd2 = build_base_cmd(config) + ["--replay-preemptions", tmpfile.name]
        result2 = subprocess.run(
            cmd2, capture_output=True, text=True, timeout=PROCESS_TIMEOUT_SEC
        )
        elapsed = time.monotonic() - start

        if result2.returncode != 0:
            error_type = classify_error(result2.returncode, result2.stderr)
            return Finding(
                config=config,
                error_type=f"replay_{error_type}",
                exit_code=result2.returncode,
                stderr=result2.stderr.strip(),
                stdout=result2.stdout.strip(),
                wall_time_sec=elapsed,
            )

        # Both runs succeeded — determinism check passed
        return None

    except subprocess.TimeoutExpired:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="timeout",
            exit_code=-1,
            stderr=f"process timed out after {PROCESS_TIMEOUT_SEC}s",
            stdout="",
            wall_time_sec=elapsed,
        )
    except Exception as e:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="other",
            exit_code=-1,
            stderr=str(e),
            stdout="",
            wall_time_sec=elapsed,
        )
    finally:
        if tmpfile and os.path.exists(tmpfile.name):
            os.unlink(tmpfile.name)


def normalize_stdout(stdout: str) -> str:
    """Remove nondeterministic fields from stdout for comparison.

    The structop summary's `rbc` column measures PMU overhead (real CPU
    cycles), which varies between runs. Strip that column so only
    deterministic fields (structops, kfuncs, interlv) remain.
    """
    lines = []
    in_structop_table = False
    rbc_col_idx = None

    for line in stdout.splitlines():
        stripped = line.strip()

        # Detect the structop header line to find the rbc column index
        if "structops" in stripped and "kfuncs" in stripped:
            in_structop_table = True
            parts = stripped.split()
            try:
                rbc_col_idx = parts.index("rbc")
            except ValueError:
                rbc_col_idx = None
            if rbc_col_idx is not None:
                del parts[rbc_col_idx]
            lines.append("  ".join(parts))
            continue

        # Inside the structop table: strip the rbc column from data lines
        if in_structop_table and rbc_col_idx is not None:
            parts = stripped.split()
            if len(parts) > rbc_col_idx:
                del parts[rbc_col_idx]
                lines.append("  ".join(parts))
                continue
            elif not stripped:
                # Empty line ends the table
                in_structop_table = False
                rbc_col_idx = None

        lines.append(line)

    return "\n".join(lines)


def compare_outputs(stdout1: str, stdout2: str, run_a: int, run_b: int) -> str:
    """Compare two stdout strings line-by-line and return diff description.

    Returns empty string if identical, otherwise a summary of first divergence.
    Normalizes stdout to remove nondeterministic PMU overhead columns.
    """
    norm1 = normalize_stdout(stdout1).strip().splitlines()
    norm2 = normalize_stdout(stdout2).strip().splitlines()
    for i, (l1, l2) in enumerate(zip(norm1, norm2)):
        if l1 != l2:
            return (
                f"stdout diverges at line {i + 1} (run {run_a} vs {run_b}):\n"
                f"  run {run_a}: {l1!r}\n"
                f"  run {run_b}: {l2!r}"
            )
    if len(norm1) != len(norm2):
        return (
            f"stdout line count differs (run {run_a} vs {run_b}): "
            f"{len(norm1)} vs {len(norm2)}"
        )
    return ""


def run_determinism_e9patch(config: TestConfig) -> Optional[Finding]:
    """Run the same e9patch config N times and compare stdout for determinism.

    e9patch is fully deterministic (no PMU skid), so identical seeds must
    produce identical output. This is a stronger check than --determinism-check
    because it also compares the final stdout summary (not just checkpoints).
    """
    start = time.monotonic()
    results = []
    env = os.environ.copy()
    env["RUST_LOG"] = "warn"

    try:
        for i in range(E9_DETERMINISM_REPEATS):
            cmd = build_base_cmd(config)
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=PROCESS_TIMEOUT_SEC,
                env=env,
            )
            if result.returncode != 0:
                elapsed = time.monotonic() - start
                error_type = classify_error(result.returncode, result.stderr)
                return Finding(
                    config=config,
                    error_type=f"e9_run{i + 1}_{error_type}",
                    exit_code=result.returncode,
                    stderr=result.stderr.strip(),
                    stdout=result.stdout.strip(),
                    wall_time_sec=elapsed,
                )
            results.append(result)

        elapsed = time.monotonic() - start

        # Compare all runs against the first
        for i in range(1, len(results)):
            diff = compare_outputs(
                results[0].stdout, results[i].stdout, 1, i + 1
            )
            if diff:
                combined_stderr = (
                    f"e9patch determinism failure: {diff}\n\n"
                    f"--- run 1 stderr ---\n{results[0].stderr.strip()}\n\n"
                    f"--- run {i + 1} stderr ---\n{results[i].stderr.strip()}"
                )
                return Finding(
                    config=config,
                    error_type="determinism",
                    exit_code=1,
                    stderr=combined_stderr,
                    stdout=results[0].stdout.strip(),
                    wall_time_sec=elapsed,
                )

        # All runs identical
        return None

    except subprocess.TimeoutExpired:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="timeout",
            exit_code=-1,
            stderr=f"process timed out after {PROCESS_TIMEOUT_SEC}s",
            stdout="",
            wall_time_sec=elapsed,
        )
    except Exception as e:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="other",
            exit_code=-1,
            stderr=str(e),
            stdout="",
            wall_time_sec=elapsed,
        )


def run_one(config: TestConfig) -> Optional[Finding]:
    """Run a single simulation and return a Finding if it fails."""
    # In determinism mode, use specialized handlers per interleave mode.
    if DETERMINISM_MODE:
        if config.interleave_mode == "preemptive":
            return run_determinism_preemptive(config)
        elif config.interleave_mode == "e9patch":
            return run_determinism_e9patch(config)

    cmd = build_base_cmd(config)

    # In determinism mode (cooperative/off), use --determinism-check flag
    if DETERMINISM_MODE:
        cmd.append("--determinism-check")

    start = time.monotonic()
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=PROCESS_TIMEOUT_SEC,
        )
        elapsed = time.monotonic() - start

        if result.returncode == 0:
            return None

        error_type = classify_error(result.returncode, result.stderr)

        return Finding(
            config=config,
            error_type=error_type,
            exit_code=result.returncode,
            stderr=result.stderr.strip(),
            stdout=result.stdout.strip(),
            wall_time_sec=elapsed,
        )

    except subprocess.TimeoutExpired:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="timeout",
            exit_code=-1,
            stderr=f"process timed out after {PROCESS_TIMEOUT_SEC}s",
            stdout="",
            wall_time_sec=elapsed,
        )
    except Exception as e:
        elapsed = time.monotonic() - start
        return Finding(
            config=config,
            error_type="other",
            exit_code=-1,
            stderr=str(e),
            stdout="",
            wall_time_sec=elapsed,
        )


def generate_configs(
    rng: random.Random,
    schedulers: list[str],
    workloads: list[Path],
    modes: list[str],
) -> TestConfig:
    """Generate a random test configuration."""
    mode = rng.choice(modes)
    cpus = rng.choice(CPU_COUNTS)
    # e9patch with 1 CPU has no concurrent dispatch and thus no preemption,
    # so bias toward 2+ CPUs for e9patch mode.
    if mode == "e9patch" and cpus == 1:
        cpus = rng.choice([2, 4, 8])
    return TestConfig(
        scheduler=rng.choice(schedulers),
        workload=rng.choice(workloads),
        cpus=cpus,
        seed=rng.randint(0, 2**32 - 1),
        interleave_mode=mode,
        iteration=0,
    )


def save_finding(finding: Finding, finding_num: int) -> Path:
    """Write a finding report to disk."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    filename = f"finding_{finding_num:04d}_{finding.error_type}.txt"
    path = OUTPUT_DIR / filename
    path.write_text(finding.report())
    return path


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description="Stress test scx_simulator")
    parser.add_argument(
        "--duration",
        type=float,
        default=10,
        help="Duration in minutes (default: 10). Supports fractions like 0.5 for 30s.",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=os.cpu_count(),
        help=f"Parallel workers (default: {os.cpu_count()})",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=None,
        help="Master PRNG seed for reproducibility",
    )
    parser.add_argument(
        "--schedulers",
        type=str,
        default=None,
        help=f"Comma-separated list of schedulers (default: all: {','.join(SCHEDULERS)})",
    )
    parser.add_argument(
        "--workloads",
        type=str,
        default=None,
        help="Comma-separated list of workload names (without .json). Use --list-workloads to see available.",
    )
    parser.add_argument(
        "--list-workloads",
        action="store_true",
        help="List available workloads and exit",
    )
    parser.add_argument(
        "--sim-duration",
        type=str,
        default=DEFAULT_SIM_DURATION,
        help=f"Simulation duration in virtual time (default: {DEFAULT_SIM_DURATION})",
    )
    parser.add_argument(
        "--watchdog",
        type=str,
        default=DEFAULT_WATCHDOG_TIMEOUT,
        help=f"Watchdog timeout for stall detection (default: {DEFAULT_WATCHDOG_TIMEOUT})",
    )
    parser.add_argument(
        "--determinism",
        action="store_true",
        help="Enable strict determinism checking: run each seed twice and verify identical behavior",
    )
    parser.add_argument(
        "--e9patch",
        action="store_true",
        help="Only test e9patch interleave mode (requires _e9.so variants)",
    )
    parser.add_argument(
        "--no-e9patch",
        action="store_true",
        help="Exclude e9patch interleave mode",
    )
    args = parser.parse_args()

    # Handle --list-workloads early (before other setup)
    all_workloads = get_available_workloads()
    if args.list_workloads:
        print("Available workloads:")
        for wl in all_workloads:
            print(f"  {wl.stem}")
        sys.exit(0)

    # Set global config from CLI args
    global WATCHDOG_TIMEOUT, SIM_DURATION, DETERMINISM_MODE
    WATCHDOG_TIMEOUT = args.watchdog
    SIM_DURATION = args.sim_duration
    DETERMINISM_MODE = args.determinism

    # Set up logging first
    log_path = setup_logging()

    if args.schedulers:
        schedulers = [s.strip() for s in args.schedulers.split(",")]
        unknown = [s for s in schedulers if s not in SCHEDULERS]
        if unknown:
            print(f"error: unknown scheduler(s): {', '.join(unknown)}", file=sys.stderr)
            print(f"available: {', '.join(SCHEDULERS)}", file=sys.stderr)
            sys.exit(1)
    else:
        schedulers = SCHEDULERS

    # Filter workloads
    workload_names = [wl.stem for wl in all_workloads]
    if args.workloads:
        requested = [w.strip() for w in args.workloads.split(",")]
        unknown = [w for w in requested if w not in workload_names]
        if unknown:
            print(f"error: unknown workload(s): {', '.join(unknown)}", file=sys.stderr)
            print(f"available: {', '.join(workload_names)}", file=sys.stderr)
            print("use --list-workloads to see all available workloads", file=sys.stderr)
            sys.exit(1)
        workloads = [wl for wl in all_workloads if wl.stem in requested]
    else:
        workloads = all_workloads

    if not workloads:
        print("error: no workloads found", file=sys.stderr)
        sys.exit(1)

    if not SCXSIM.exists():
        print(f"error: {SCXSIM} not found; run 'cargo build --release' first",
              file=sys.stderr)
        sys.exit(1)

    # Determine which interleave modes to test
    has_e9 = e9_schedulers_available()
    if args.e9patch:
        if not has_e9:
            print(
                "error: --e9patch requires _e9.so variants. "
                "Build with: make -C schedulers e9",
                file=sys.stderr,
            )
            sys.exit(1)
        modes = ["e9patch"]
    elif args.no_e9patch:
        modes = [m for m in INTERLEAVE_MODES if m != "e9patch"]
    elif has_e9:
        modes = INTERLEAVE_MODES
    else:
        modes = [m for m in INTERLEAVE_MODES if m != "e9patch"]

    master_seed = args.seed if args.seed is not None else random.randint(0, 2**32 - 1)
    rng = random.Random(master_seed)
    deadline = time.monotonic() + args.duration * 60
    start_time = time.monotonic()

    mode_str = "determinism" if DETERMINISM_MODE else "standard"
    print(f"Stress test: {args.duration}min, {args.jobs} workers, "
          f"master seed={master_seed}")
    print(f"Mode: {mode_str}")
    print(f"Schedulers: {', '.join(schedulers)}")
    print(f"Workloads: {', '.join(wl.stem for wl in workloads)}")
    print(f"Interleave modes: {', '.join(modes)}")
    print(f"Watchdog: {WATCHDOG_TIMEOUT}, sim duration: {SIM_DURATION}")
    if "e9patch" in modes:
        print(f"e9patch determinism repeats: {E9_DETERMINISM_REPEATS}")
    print(f"Output: {OUTPUT_DIR}")
    print(f"Log: {log_path}")
    print()

    log.info("=" * 60)
    log.info("Stress test started")
    log.info("=" * 60)
    log.info(f"Duration: {args.duration} minutes")
    log.info(f"Workers: {args.jobs}")
    log.info(f"Master seed: {master_seed}")
    log.info(f"Mode: {mode_str}")
    log.info(f"Schedulers: {', '.join(schedulers)}")
    log.info(f"Workloads: {', '.join(wl.stem for wl in workloads)}")
    log.info(f"Interleave modes: {', '.join(modes)}")
    log.info(f"Watchdog timeout: {WATCHDOG_TIMEOUT}")
    log.info(f"Sim duration: {SIM_DURATION}")

    findings: list[Finding] = []
    total_runs = 0
    completed_runs = 0

    # Pre-generate a batch of configs
    batch_size = args.jobs * 4

    try:
        with ProcessPoolExecutor(max_workers=args.jobs) as pool:
            pending = {}
            iteration = 0

            while time.monotonic() < deadline or pending:
                # Submit new work while under deadline
                while len(pending) < batch_size and time.monotonic() < deadline:
                    config = generate_configs(rng, schedulers, workloads, modes)
                    config.iteration = iteration
                    iteration += 1
                    future = pool.submit(run_one, config)
                    pending[future] = config
                    total_runs += 1

                # Collect results
                done = []
                for future in list(pending.keys()):
                    if future.done():
                        done.append(future)

                if not done and pending:
                    # Wait for at least one to complete
                    try:
                        next_done = next(as_completed(pending, timeout=5))
                        done.append(next_done)
                    except StopIteration:
                        pass
                    except TimeoutError:
                        continue

                for future in done:
                    config = pending.pop(future)
                    completed_runs += 1
                    try:
                        finding = future.result()
                    except Exception as e:
                        finding = Finding(
                            config=config,
                            error_type="executor_error",
                            exit_code=-1,
                            stderr=str(e),
                            stdout="",
                            wall_time_sec=0,
                        )

                    if finding is not None:
                        findings.append(finding)
                        num = len(findings)
                        path = save_finding(finding, num)
                        log.warning(f"BUG #{num}: {finding.summary()} -> {path.name}")
                        print(
                            f"  BUG #{num}: {finding.summary()}"
                            f"  -> {path.name}"
                        )
                    else:
                        log.debug(f"PASS: {config.label}")

                # Progress update every batch
                elapsed_min = (time.monotonic() - start_time) / 60
                remaining_min = max(0, (deadline - time.monotonic()) / 60)
                rate = completed_runs / max(elapsed_min, 0.01)
                sys.stdout.write(
                    f"\r  {completed_runs} runs, "
                    f"{len(findings)} bugs, "
                    f"{rate:.0f} runs/min, "
                    f"{remaining_min:.1f}min left"
                )
                sys.stdout.flush()

                # Log progress periodically (roughly every 100 runs)
                if completed_runs % 100 < len(done):
                    log.info(
                        f"Progress: {completed_runs} runs, {len(findings)} bugs, "
                        f"{rate:.0f} runs/min, {remaining_min:.1f}min left"
                    )

    except KeyboardInterrupt:
        log.info("Interrupted by user")
        print("\n\nInterrupted by user.")

    # Final report
    elapsed_total = (time.monotonic() - start_time) / 60
    print(f"\n\n{'=' * 60}")
    print(f"Stress test complete")
    print(f"{'=' * 60}")
    print(f"  Total runs:  {completed_runs}")
    print(f"  Findings:    {len(findings)}")
    print(f"  Master seed: {master_seed}")
    print(f"  Elapsed:     {elapsed_total:.1f} minutes")

    log.info("=" * 60)
    log.info("Stress test complete")
    log.info("=" * 60)
    log.info(f"Total runs: {completed_runs}")
    log.info(f"Findings: {len(findings)}")
    log.info(f"Master seed: {master_seed}")
    log.info(f"Elapsed: {elapsed_total:.1f} minutes")

    if findings:
        print(f"\n  Findings by type:")
        type_counts: dict[str, int] = {}
        for f in findings:
            type_counts[f.error_type] = type_counts.get(f.error_type, 0) + 1
        for error_type, count in sorted(type_counts.items()):
            print(f"    {error_type}: {count}")
            log.info(f"  {error_type}: {count}")
        print(f"\n  Reports written to: {OUTPUT_DIR}")
    else:
        print(f"\n  No bugs found.")
        log.info("No bugs found.")

    log.info(f"Log file: {log_path}")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
