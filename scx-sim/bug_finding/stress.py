#!/usr/bin/env python3
"""
Stress test for scx_simulator.

Runs randomized simulation configurations in parallel, searching for stalls,
BPF errors, crashes, and other failures. All findings are reported immediately
and written to bug_finding/output/ for later analysis.

Usage:
    python3 stress.py                         # Run for 10 minutes (default)
    python3 stress.py --duration 30           # Run for 30 minutes
    python3 stress.py --jobs 8                # Use 8 parallel workers
    python3 stress.py --determinism           # Enable determinism checking
    python3 stress.py --e9patch               # Only test e9patch mode
    python3 stress.py --random-workloads      # Enable randomized workloads + params
"""
from __future__ import annotations

import argparse
import json
import logging
import multiprocessing
import multiprocessing.sharedctypes
import multiprocessing.synchronize
import os
import random
import shlex
import shutil
import signal
import subprocess
import tempfile
import sys
import time
from concurrent.futures import Future, ProcessPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).parent.parent
SCXSIM = PROJECT_ROOT / "target" / "release" / "scxsim"
WORKLOADS_DIR = PROJECT_ROOT / "crates" / "scx_simulator" / "workloads"
OUTPUT_DIR = Path(__file__).parent / "output"

SCHEDULERS = ["simple", "lavd", "cosmos", "tickless", "mitosis"]
CPU_COUNTS = [1, 2, 4, 8]
INTERLEAVE_MODES = ["off", "cooperative", "preemptive", "native-concurrent", "e9patch"]


def get_available_workloads() -> list[Path]:
    """Return sorted list of available workload files."""
    return sorted(WORKLOADS_DIR.glob("*.json"))


# ---------------------------------------------------------------------------
# Randomized workload generation
# ---------------------------------------------------------------------------

# Dimensions for random workload generation
TASK_COUNTS = [1, 2, 4, 8, 16, 32]
TASK_MULTIPLIERS = [1]
PHASE_PATTERNS = ["run_only", "run_sleep", "run_sleep_wake", "mixed"]

# Randomized simulator parameter choices
RBC_NS_CHOICES = [0, 5, 10, 50]
WATCHDOG_CHOICES = ["2s", "5s", "30s"]
WINDOW_NS_CHOICES = [1_000, 10_000, 100_000, 1_000_000, 10_000_000]
TIMESLICE_RBC_CHOICES = [(300, 750), (300, 1500), (750, 3000), (1500, 6000)]
TIMESLICE_INSN_CHOICES = [(5_000, 20_000), (20_000, 80_000), (80_000, 250_000)]
BREAK_ON_CHOICES = ["rbc", "insn"]
RUN_JITTER_CV_PPM_CHOICES = [0, 100_000, 200_000, 500_000, 1_000_000]
TICK_JITTER_STDDEV_NS_CHOICES = [0, 2_000, 10_000, 50_000, 250_000]
INITIAL_TICK_SKEW_NS_CHOICES = [0, 1_000, 10_000, 100_000, 1_000_000]

# Temporary directory for generated workloads (cleaned up at exit)
_GENERATED_WORKLOADS_DIR: Optional[Path] = None


def _get_generated_workloads_dir() -> Path:
    """Return (creating if needed) a temp directory for generated workloads."""
    global _GENERATED_WORKLOADS_DIR
    if _GENERATED_WORKLOADS_DIR is None:
        _GENERATED_WORKLOADS_DIR = Path(
            tempfile.mkdtemp(prefix="scxsim_workloads_")
        )
    return _GENERATED_WORKLOADS_DIR


def _gen_phases_run_only(rng: random.Random) -> list[dict[str, Any]]:
    """Generate a run-only phase list."""
    run_us = rng.choice([1000, 2000, 5000, 10000, 20000])
    return [{"run": run_us}]


def _gen_phases_run_sleep(rng: random.Random) -> list[dict[str, Any]]:
    """Generate a run+sleep phase list."""
    run_us = rng.choice([1000, 2000, 5000, 10000])
    sleep_us = rng.choice([1000, 5000, 10000, 20000])
    return [{"run": run_us, "sleep": sleep_us}]


def _gen_phases_run_sleep_wake(
    rng: random.Random, other_task_name: Optional[str],
) -> list[dict[str, Any]]:
    """Generate a run+sleep+wake phase list (with resume if target exists)."""
    run_us = rng.choice([2000, 5000, 10000])
    sleep_us = rng.choice([5000, 10000, 20000])
    phases: list[dict[str, Any]] = [{"run": run_us}]
    if other_task_name:
        phases[0]["resume"] = other_task_name
    phases[0]["sleep"] = sleep_us
    return phases


def _gen_phases_mixed(
    rng: random.Random, other_task_name: Optional[str],
) -> list[dict[str, Any]]:
    """Generate a mixed-pattern phase list with multiple run/sleep segments."""
    phases: list[dict[str, Any]] = []
    n_segments = rng.randint(2, 4)
    for _ in range(n_segments):
        entry: dict[str, Any] = {}
        entry["run"] = rng.choice([1000, 2000, 5000, 10000])
        if rng.random() < 0.5:
            entry["sleep"] = rng.choice([1000, 5000, 10000])
        if other_task_name and rng.random() < 0.3:
            entry["resume"] = other_task_name
        phases.append(entry)
    return phases


PATTERN_GENERATORS = {
    "run_only": lambda rng, _: _gen_phases_run_only(rng),
    "run_sleep": lambda rng, _: _gen_phases_run_sleep(rng),
    "run_sleep_wake": _gen_phases_run_sleep_wake,
    "mixed": _gen_phases_mixed,
}


def _build_task_phases(
    rng: random.Random,
    pattern: str,
    other_task_name: Optional[str],
) -> dict[str, Any]:
    """Build a single task's top-level JSON object from a phase pattern.

    For multi-segment patterns (mixed), uses the rt-app 'phases' sub-object.
    For single-segment patterns, uses flat keys directly on the task object.
    """
    gen = PATTERN_GENERATORS[pattern]
    segments = gen(rng, other_task_name)

    if len(segments) == 1:
        # Flat: keys directly on the task object
        return segments[0]
    else:
        # Multi-phase: wrap in "phases" sub-object
        phases_obj = {}
        for i, seg in enumerate(segments):
            phases_obj[f"phase{i}"] = seg
        return {"phases": phases_obj}


def generate_random_workload(rng: random.Random, cpus: Optional[int] = None) -> Path:
    """Generate a random rt-app workload JSON file.

    Picks random task count, phase pattern per task, and duration spread.
    Returns the path to the generated temporary JSON file.
    """
    if cpus is not None and TASK_MULTIPLIERS and rng.random() < 0.7:
        task_count = cpus * rng.choice(TASK_MULTIPLIERS)
    else:
        task_count = rng.choice(TASK_COUNTS)
    task_count = max(1, task_count)
    duration_sec = rng.choice([1, 2, 4])

    # Pick a dominant pattern but allow per-task variation
    dominant_pattern = rng.choice(PHASE_PATTERNS)

    # Duration spread: uniform vs skewed
    skewed = rng.random() < 0.3

    # Build task definitions
    task_names = [f"t{i}" for i in range(task_count)]
    tasks: dict[str, Any] = {}
    for idx, name in enumerate(task_names):
        # Per-task pattern: 70% dominant, 30% random
        if rng.random() < 0.7:
            pattern = dominant_pattern
        else:
            pattern = rng.choice(PHASE_PATTERNS)

        # Pick a wake target (another task, if wake patterns are used)
        other = None
        if pattern in ("run_sleep_wake", "mixed") and task_count > 1:
            candidates = [n for n in task_names if n != name]
            other = rng.choice(candidates)

        task_obj = _build_task_phases(rng, pattern, other)

        # Set priority: mostly 0, occasionally negative (higher priority)
        nice = 0
        if rng.random() < 0.2:
            nice = rng.choice([-5, -10, -15])
        task_obj["priority"] = nice
        task_obj["loop"] = -1

        # Skewed duration: vary run times by scaling some tasks
        if skewed and rng.random() < 0.4:
            _scale_run_times(task_obj, rng.choice([0.25, 0.5, 2.0, 4.0]))

        tasks[name] = task_obj

    # If we have wake patterns, ensure at least one task can be woken
    # by adding a suspend to the first wake target found
    _add_suspend_for_resume_targets(tasks)

    workload = {
        "global": {
            "duration": duration_sec,
            "default_policy": "SCHED_OTHER",
            "calibration": 19,
        },
        "tasks": tasks,
    }

    # Write to temp file
    out_dir = _get_generated_workloads_dir()
    suffix = f"_t{task_count}_{dominant_pattern}"
    fd, path = tempfile.mkstemp(
        suffix=".json", prefix=f"rand{suffix}_", dir=str(out_dir),
    )
    with os.fdopen(fd, "w") as f:
        json.dump(workload, f, indent=2)
    return Path(path)


def _scale_run_times(task_obj: dict[str, Any], factor: float) -> None:
    """Scale all 'run' values in a task object by a factor."""
    for key in list(task_obj.keys()):
        if key.startswith("run") and isinstance(task_obj[key], int):
            task_obj[key] = max(100, int(task_obj[key] * factor))
    if "phases" in task_obj and isinstance(task_obj["phases"], dict):
        for phase in task_obj["phases"].values():
            if isinstance(phase, dict):
                _scale_run_times(phase, factor)


def _add_suspend_for_resume_targets(tasks: dict[str, Any]) -> None:
    """For tasks that are resume targets, add a suspend if they lack one."""
    resume_targets: set[str] = set()
    for task_obj in tasks.values():
        _collect_resume_targets(task_obj, resume_targets)

    for target_name in resume_targets:
        if target_name in tasks:
            task_obj = tasks[target_name]
            # Only add suspend if not already present at top level
            if "suspend" not in task_obj:
                _prepend_suspend(task_obj, target_name)


def _collect_resume_targets(obj: dict[str, Any], targets: set[str]) -> None:
    """Recursively collect all resume target names from a task object."""
    for key, val in obj.items():
        if key.startswith("resume") and isinstance(val, str):
            targets.add(val)
        elif key == "phases" and isinstance(val, dict):
            for phase in val.values():
                if isinstance(phase, dict):
                    _collect_resume_targets(phase, targets)


def _prepend_suspend(task_obj: dict[str, Any], task_name: str) -> None:
    """Prepend a suspend event to a task so it can be woken by resume."""
    if "phases" in task_obj:
        # Multi-phase: add a suspend phase at the beginning
        phases = task_obj["phases"]
        new_phases = {"suspend_phase": {"suspend": task_name}}
        new_phases.update(phases)
        task_obj["phases"] = new_phases
    else:
        # Flat: we need to restructure as phases to prepend suspend
        # Extract existing events into a "main" phase, add suspend before
        event_keys = [
            k for k in task_obj
            if k not in ("priority", "loop", "cpus", "instance")
        ]
        main_phase = {k: task_obj[k] for k in event_keys}
        for k in event_keys:
            del task_obj[k]
        task_obj["phases"] = {
            "suspend_phase": {"suspend": task_name},
            "main": main_phase,
        }


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

# PMU token pool: limits concurrent PMU-signal preemptive instances
PMU_TOKEN_POOL: Optional[multiprocessing.synchronize.Semaphore] = None
_CPU_COUNT = os.cpu_count() or 1
MAX_PMU_CONCURRENT = max(_CPU_COUNT // 3, 4)  # Default: nproc/3 (scaling test shows ~86% efficiency)

# PMU stats counters (shared across processes)
PMU_ACQUIRED: Optional[multiprocessing.sharedctypes.Synchronized[int]] = None
PMU_REDIRECTED: Optional[multiprocessing.sharedctypes.Synchronized[int]] = None

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
class SimParams:
    """Extra simulator parameters randomized per test config."""

    rbc_ns: Optional[int] = None  # --rbc-ns value, None = default
    watchdog_timeout: Optional[str] = None  # --watchdog-timeout override
    no_noise: bool = False  # --no-noise
    no_overhead: bool = False  # --no-overhead
    fixed_priority: bool = False  # --fixed-priority control run
    window_ns: Optional[int] = None  # --window-ns for native-concurrent
    timeslice_min: Optional[int] = None  # --timeslice-min for preemptive
    timeslice_max: Optional[int] = None  # --timeslice-max for preemptive
    break_on: Optional[str] = None  # --break-on for preemptive
    env: dict[str, str] = field(default_factory=dict)

    def to_record(self) -> dict[str, Any]:
        """Serialize parameters for JSONL run manifests."""
        return {
            "rbc_ns": self.rbc_ns,
            "watchdog_timeout": self.watchdog_timeout,
            "no_noise": self.no_noise,
            "no_overhead": self.no_overhead,
            "fixed_priority": self.fixed_priority,
            "window_ns": self.window_ns,
            "timeslice_min": self.timeslice_min,
            "timeslice_max": self.timeslice_max,
            "break_on": self.break_on,
            "env": dict(sorted(self.env.items())),
        }


@dataclass
class TestConfig:
    """A single stress test configuration."""

    scheduler: str
    workload: Path
    cpus: int
    seed: int
    interleave_mode: str  # "off", "cooperative", "preemptive", "e9patch"
    iteration: int
    is_random_workload: bool = False
    sim_params: SimParams = field(default_factory=SimParams)

    @property
    def label(self) -> str:
        wl = self.workload.stem
        prefix = "rand/" if self.is_random_workload else ""
        suffix = ""
        if self.sim_params.fixed_priority:
            suffix += "/fixed"
        if self.sim_params.env:
            suffix += "/chaos"
        return (
            f"{prefix}{self.scheduler}/{wl}/c{self.cpus}"
            f"/s{self.seed}/{self.interleave_mode}{suffix}"
        )

    def to_record(self) -> dict[str, Any]:
        """Serialize this config for reproducible run records."""
        record: dict[str, Any] = {
            "iteration": self.iteration,
            "label": self.label,
            "scheduler": self.scheduler,
            "workload": str(self.workload),
            "workload_name": self.workload.stem,
            "cpus": self.cpus,
            "seed": self.seed,
            "interleave_mode": self.interleave_mode,
            "random_workload": self.is_random_workload,
            "sim_params": self.sim_params.to_record(),
            "command": format_command(build_base_cmd(self), self.sim_params.env),
        }
        if self.is_random_workload:
            try:
                record["workload_json"] = json.loads(self.workload.read_text())
            except (OSError, json.JSONDecodeError) as exc:
                record["workload_json_error"] = str(exc)
        return record


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
            f"Random workload: {self.config.is_random_workload}",
            f"Scxsim: {SCXSIM.resolve()}",
            f"CPUs: {self.config.cpus}",
            f"Seed: {self.config.seed}",
            f"Interleave: {self.config.interleave_mode}",
        ]
        params = self.config.sim_params
        if params.rbc_ns is not None:
            lines.append(f"RBC-ns: {params.rbc_ns}")
        if params.watchdog_timeout:
            lines.append(f"Watchdog override: {params.watchdog_timeout}")
        if params.no_noise:
            lines.append("Noise: disabled")
        if params.no_overhead:
            lines.append("Overhead: disabled")
        if params.fixed_priority:
            lines.append("Event ordering: fixed priority")
        if params.window_ns is not None:
            lines.append(f"Native window-ns: {params.window_ns}")
        if params.timeslice_min is not None:
            lines.append(f"Timeslice-min: {params.timeslice_min}")
        if params.timeslice_max is not None:
            lines.append(f"Timeslice-max: {params.timeslice_max}")
        if params.break_on:
            lines.append(f"Break-on: {params.break_on}")
        if params.env:
            lines.append(f"Environment: {json.dumps(params.env, sort_keys=True)}")
        lines.extend([
            "",
            "--- Reproduction command ---",
            self.repro_command(),
        ])
        # Include workload JSON content for random workloads
        if self.config.is_random_workload:
            try:
                wl_content = self.config.workload.read_text()
                lines.extend([
                    "",
                    "--- Workload JSON ---",
                    wl_content,
                ])
            except (OSError, FileNotFoundError):
                lines.extend(["", "--- Workload JSON ---", "(file deleted)"])
        lines.extend([
            "",
            "--- stderr ---",
            self.stderr or "(empty)",
            "",
            "--- stdout ---",
            self.stdout or "(empty)",
        ])
        return "\n".join(lines)

    def repro_command(self) -> str:
        cmd = build_base_cmd(self.config)
        if self.error_type.startswith("record_"):
            # Phase 1 failed during preemptive determinism
            cmd.append("--record-preemptions /tmp/repro.preempt")
        elif self.error_type.startswith(("replay_", "replay2_")):
            # Replay failed during preemptive determinism (record+replay)
            record_cmd = format_command(
                cmd + ["--record-preemptions", "/tmp/repro.preempt"],
                self.config.sim_params.env,
            )
            replay_cmd = format_command([str(SCXSIM), "replay", "/tmp/repro.preempt"])
            return f"{record_cmd} && {replay_cmd}"
        elif self.error_type == "replay_nondeterminism":
            # Record then replay twice to compare
            record_cmd = format_command(
                cmd + ["--record-preemptions", "/tmp/repro.preempt"],
                self.config.sim_params.env,
            )
            replay_cmd = format_command([str(SCXSIM), "replay", "/tmp/repro.preempt"])
            return f"{record_cmd} && {replay_cmd} && {replay_cmd}"
        elif self.error_type.startswith("e9_replay_") or self.error_type == "cross_replay_mismatch":
            # Cross-mechanism: record + e9patch replay + hw replay
            record_cmd = format_command(
                cmd + ["--record-preemptions", "/tmp/repro.preempt"],
                self.config.sim_params.env,
            )
            e9_replay = format_command([
                str(SCXSIM), "replay", "/tmp/repro.preempt",
                "--preempt-mode", "e9patch",
            ])
            hw_replay = format_command([str(SCXSIM), "replay", "/tmp/repro.preempt"])
            return f"{record_cmd} && {e9_replay} && {hw_replay}"
        elif self.error_type == "determinism":
            cmd.append("--determinism-check")
        return format_command(cmd, self.config.sim_params.env)


# ---------------------------------------------------------------------------
# Test execution
# ---------------------------------------------------------------------------


def build_base_cmd(config: TestConfig) -> list[str]:
    """Build the base scxsim command for a configuration."""
    # Use sim_params watchdog if set, otherwise global default
    watchdog = config.sim_params.watchdog_timeout or WATCHDOG_TIMEOUT
    cmd = [
        str(SCXSIM),
        "run",
        str(config.workload),
        "-s", config.scheduler,
        "-c", str(config.cpus),
        "--seed", str(config.seed),
        "--watchdog-timeout", watchdog,
        "--end-time", SIM_DURATION,
    ]
    if config.interleave_mode == "cooperative":
        cmd.append("--interleave")
    elif config.interleave_mode == "preemptive":
        cmd.append("--preemptive")
    elif config.interleave_mode == "e9patch":
        cmd.extend(["--preemptive", "--preempt-mode", "e9patch"])
    elif config.interleave_mode == "native-concurrent":
        cmd.append("--native-concurrent")

    # Apply extra simulator parameters
    params = config.sim_params
    if params.rbc_ns is not None:
        cmd.extend(["--rbc-ns", str(params.rbc_ns)])
    if params.no_noise:
        cmd.append("--no-noise")
    if params.no_overhead:
        cmd.append("--no-overhead")
    if params.fixed_priority:
        cmd.append("--fixed-priority")
    if params.window_ns is not None and config.interleave_mode == "native-concurrent":
        cmd.extend(["--window-ns", str(params.window_ns)])
    if params.timeslice_min is not None and config.interleave_mode in ("preemptive", "e9patch"):
        cmd.extend(["--timeslice-min", str(params.timeslice_min)])
    if params.timeslice_max is not None and config.interleave_mode in ("preemptive", "e9patch"):
        cmd.extend(["--timeslice-max", str(params.timeslice_max)])
    if params.break_on and config.interleave_mode in ("preemptive", "e9patch"):
        cmd.extend(["--break-on", params.break_on])
    return cmd


def build_env(config: TestConfig) -> dict[str, str]:
    """Build subprocess environment for a configuration."""
    env = os.environ.copy()
    env.update(config.sim_params.env)
    return env


def format_command(cmd: list[str], extra_env: Optional[dict[str, str]] = None) -> str:
    """Return a shell-replayable command string."""
    parts = []
    for key, value in sorted((extra_env or {}).items()):
        parts.append(f"{key}={shlex.quote(value)}")
    parts.extend(shlex.quote(str(arg)) for arg in cmd)
    return " ".join(parts)


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


_SIMULATION_METRIC_KEYS = [
    "total_events", "total_ticks", "total_yields", "total_preempts",
    "total_sleeps", "total_wakes", "total_idle_periods",
    "global_dsq_dispatches", "local_dsq_dispatches",
]


def _extract_simulation_metrics(stdout: str) -> dict[str, str]:
    """Extract determinism-relevant metrics from scxsim stdout."""
    metrics: dict[str, str] = {}
    for line in stdout.splitlines():
        stripped = line.strip()
        for key in _SIMULATION_METRIC_KEYS:
            if stripped.startswith(f"{key}:"):
                metrics[key] = stripped.split(":")[-1].strip()
    return metrics


def _compare_simulation_metrics(
    metrics_a: dict[str, str],
    metrics_b: dict[str, str],
    label_a: str,
    label_b: str,
) -> str:
    """Compare two sets of simulation metrics.

    Returns empty string if identical, otherwise a description of first
    divergence.
    """
    all_keys = sorted(set(metrics_a) | set(metrics_b))
    for key in all_keys:
        val_a = metrics_a.get(key, "(missing)")
        val_b = metrics_b.get(key, "(missing)")
        if val_a != val_b:
            return (
                f"metric '{key}' differs ({label_a} vs {label_b}): "
                f"{val_a} vs {val_b}"
            )
    return ""


def _run_replay(
    trace_path: str,
    extra_args: Optional[list[str]] = None,
) -> subprocess.CompletedProcess[str]:
    """Execute a scxsim replay command and return the CompletedProcess."""
    cmd = [str(SCXSIM), "replay", trace_path]
    if extra_args:
        cmd.extend(extra_args)
    return subprocess.run(
        cmd, capture_output=True, text=True, timeout=PROCESS_TIMEOUT_SEC
    )


def _make_finding(
    config: TestConfig,
    error_type: str,
    exit_code: int,
    stderr: str,
    stdout: str,
    start: float,
) -> Finding:
    """Create a Finding with wall_time computed from start."""
    return Finding(
        config=config,
        error_type=error_type,
        exit_code=exit_code,
        stderr=stderr.strip(),
        stdout=stdout.strip(),
        wall_time_sec=time.monotonic() - start,
    )


def _record_preemptions(
    config: TestConfig, trace_path: str, start: float,
) -> tuple[Optional[Finding], Optional[subprocess.CompletedProcess[str]]]:
    """Record preemption points. Returns (finding, result) -- finding is set
    only on failure."""
    cmd = build_base_cmd(config) + ["--record-preemptions", trace_path]
    result = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=PROCESS_TIMEOUT_SEC,
        env=build_env(config),
    )
    if result.returncode != 0:
        error_type = classify_error(result.returncode, result.stderr)
        return (
            _make_finding(
                config, f"record_{error_type}",
                result.returncode, result.stderr, result.stdout, start,
            ),
            None,
        )
    return None, result


def _check_replay_replay(
    config: TestConfig,
    replay1_result: subprocess.CompletedProcess[str],
    replay2_result: subprocess.CompletedProcess[str],
    start: float,
) -> Optional[Finding]:
    """Compare two replay runs, return Finding on mismatch."""
    metrics1 = _extract_simulation_metrics(replay1_result.stdout)
    metrics2 = _extract_simulation_metrics(replay2_result.stdout)
    diff = _compare_simulation_metrics(metrics1, metrics2, "replay1", "replay2")
    if diff:
        combined_stderr = (
            f"replay nondeterminism: {diff}\n\n"
            f"--- replay 1 stdout ---\n{replay1_result.stdout.strip()}\n\n"
            f"--- replay 2 stdout ---\n{replay2_result.stdout.strip()}"
        )
        return _make_finding(
            config, "replay_nondeterminism",
            1, combined_stderr, replay1_result.stdout, start,
        )
    return None


def run_determinism_preemptive(config: TestConfig) -> Optional[Finding]:
    """Record preemption points, replay twice, compare replay outputs.

    Checks:
    1. Record with PMU succeeds
    2. Replay with HW breakpoint succeeds
    3. Two replays of same trace match (replay_nondeterminism)

    Note: we do NOT compare record vs replay metrics because PMU
    recording is inherently nondeterministic (hardware skid), so the
    recording run's trace metrics (total_events, total_ticks, etc.)
    will naturally differ from the replay's.  The meaningful
    determinism check is replay-vs-replay: same trace, same mechanism.
    """
    start = time.monotonic()
    tmpfile = None
    try:
        tmpfile = tempfile.NamedTemporaryFile(
            suffix=".preempt", delete=False, prefix="scxsim_"
        )
        tmpfile.close()

        # Phase 1: record preemption points (nondeterministic PMU)
        finding, _record_result = _record_preemptions(
            config, tmpfile.name, start,
        )
        if finding:
            return finding

        # Phase 2: replay preemption points (deterministic hw breakpoint)
        replay1 = _run_replay(tmpfile.name)
        if replay1.returncode != 0:
            error_type = classify_error(replay1.returncode, replay1.stderr)
            return _make_finding(
                config, f"replay_{error_type}",
                replay1.returncode, replay1.stderr, replay1.stdout, start,
            )

        # Phase 3: replay again, compare with first replay
        replay2 = _run_replay(tmpfile.name)
        if replay2.returncode != 0:
            error_type = classify_error(replay2.returncode, replay2.stderr)
            return _make_finding(
                config, f"replay2_{error_type}",
                replay2.returncode, replay2.stderr, replay2.stdout, start,
            )

        return _check_replay_replay(config, replay1, replay2, start)

    except subprocess.TimeoutExpired:
        return _make_finding(
            config, "timeout", -1,
            f"process timed out after {PROCESS_TIMEOUT_SEC}s", "", start,
        )
    except Exception as e:
        return _make_finding(config, "other", -1, str(e), "", start)
    finally:
        if tmpfile and os.path.exists(tmpfile.name):
            os.unlink(tmpfile.name)


def run_determinism_preemptive_e9_replay(
    config: TestConfig,
) -> Optional[Finding]:
    """Record with PMU, replay with e9patch, compare outputs.

    Cross-mechanism test: verifies that e9patch replay of a PMU-recorded
    trace produces the same results as HW breakpoint replay.

    Checks:
    1. Record with PMU succeeds
    2. Replay with e9patch succeeds
    3. Replay with HW breakpoint succeeds
    4. e9patch replay vs HW breakpoint replay metrics match
    """
    start = time.monotonic()
    tmpfile = None
    try:
        tmpfile = tempfile.NamedTemporaryFile(
            suffix=".preempt", delete=False, prefix="scxsim_"
        )
        tmpfile.close()

        # Phase 1: record preemption points (nondeterministic PMU)
        finding, _record_result = _record_preemptions(
            config, tmpfile.name, start,
        )
        if finding:
            return finding

        # Phase 2: replay with e9patch
        e9_replay = _run_replay(
            tmpfile.name, ["--preempt-mode", "e9patch"],
        )
        if e9_replay.returncode != 0:
            error_type = classify_error(
                e9_replay.returncode, e9_replay.stderr,
            )
            return _make_finding(
                config, f"e9_replay_{error_type}",
                e9_replay.returncode, e9_replay.stderr,
                e9_replay.stdout, start,
            )

        # Phase 3: replay with HW breakpoint (reference)
        hw_replay = _run_replay(tmpfile.name)
        if hw_replay.returncode != 0:
            error_type = classify_error(
                hw_replay.returncode, hw_replay.stderr,
            )
            return _make_finding(
                config, f"replay_{error_type}",
                hw_replay.returncode, hw_replay.stderr,
                hw_replay.stdout, start,
            )

        # Phase 4: compare e9patch replay vs HW breakpoint replay metrics
        e9_metrics = _extract_simulation_metrics(e9_replay.stdout)
        hw_metrics = _extract_simulation_metrics(hw_replay.stdout)
        diff = _compare_simulation_metrics(
            e9_metrics, hw_metrics, "e9patch_replay", "hw_replay",
        )
        if diff:
            combined_stderr = (
                f"cross-mechanism replay mismatch: {diff}\n\n"
                f"--- e9patch replay stdout ---\n"
                f"{e9_replay.stdout.strip()}\n\n"
                f"--- hw replay stdout ---\n"
                f"{hw_replay.stdout.strip()}"
            )
            return _make_finding(
                config, "cross_replay_mismatch",
                1, combined_stderr, e9_replay.stdout, start,
            )

        return None

    except subprocess.TimeoutExpired:
        return _make_finding(
            config, "timeout", -1,
            f"process timed out after {PROCESS_TIMEOUT_SEC}s", "", start,
        )
    except Exception as e:
        return _make_finding(config, "other", -1, str(e), "", start)
    finally:
        if tmpfile and os.path.exists(tmpfile.name):
            os.unlink(tmpfile.name)


def compare_outputs(stdout1: str, stdout2: str, run_a: int, run_b: int) -> str:
    """Compare two stdout strings line-by-line and return diff description.

    Returns empty string if identical, otherwise a summary of first divergence.
    For e9patch mode, EVERYTHING in stdout should be deterministic — the rbc
    column is the software-counted conditional branch count (no PMU overhead).
    """
    lines1 = stdout1.strip().splitlines()
    lines2 = stdout2.strip().splitlines()
    for i, (l1, l2) in enumerate(zip(lines1, lines2)):
        if l1 != l2:
            return (
                f"stdout diverges at line {i + 1} (run {run_a} vs {run_b}):\n"
                f"  run {run_a}: {l1!r}\n"
                f"  run {run_b}: {l2!r}"
            )
    if len(lines1) != len(lines2):
        return (
            f"stdout line count differs (run {run_a} vs {run_b}): "
            f"{len(lines1)} vs {len(lines2)}"
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
    env = build_env(config)
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


# Worker-local copies of config generation parameters (set by _init_worker)
_WORKER_SCHEDULERS: list[str] = []
_WORKER_WORKLOADS: list[Path] = []
_WORKER_MODES: list[str] = []
_WORKER_USE_RANDOM_WORKLOADS: bool = False
_WORKER_RANDOM_PARAMS: bool = False
_WORKER_CHAOS: bool = False


def _init_worker(
    pmu_pool: Optional[multiprocessing.synchronize.Semaphore],
    pmu_acquired: Optional[multiprocessing.sharedctypes.Synchronized[int]],
    pmu_redirected: Optional[multiprocessing.sharedctypes.Synchronized[int]],
    schedulers: list[str],
    workloads: list[str],
    modes: list[str],
    use_random_workloads: bool,
    random_params: bool,
    chaos: bool,
    cpu_counts: list[int],
    task_counts: list[int],
    task_multipliers: list[int],
) -> None:
    """Initializer for pool workers: install shared PMU state as globals."""
    global PMU_TOKEN_POOL, PMU_ACQUIRED, PMU_REDIRECTED
    global _WORKER_SCHEDULERS, _WORKER_WORKLOADS, _WORKER_MODES
    global _WORKER_USE_RANDOM_WORKLOADS, _WORKER_RANDOM_PARAMS, _WORKER_CHAOS
    global CPU_COUNTS, TASK_COUNTS, TASK_MULTIPLIERS
    PMU_TOKEN_POOL = pmu_pool
    PMU_ACQUIRED = pmu_acquired
    PMU_REDIRECTED = pmu_redirected
    _WORKER_SCHEDULERS = schedulers
    _WORKER_WORKLOADS = [Path(w) for w in workloads]
    _WORKER_MODES = modes
    _WORKER_USE_RANDOM_WORKLOADS = use_random_workloads
    _WORKER_RANDOM_PARAMS = random_params
    _WORKER_CHAOS = chaos
    CPU_COUNTS = cpu_counts
    TASK_COUNTS = task_counts
    TASK_MULTIPLIERS = task_multipliers


def needs_pmu_token(config: TestConfig) -> bool:
    """Check if this config needs PMU hardware (preemptive mode, not e9patch)."""
    return config.interleave_mode == "preemptive"


def _run_config(config: TestConfig) -> Optional[Finding]:
    """Execute a single simulation config and return a Finding on failure."""
    # In determinism mode, use specialized handlers per interleave mode.
    if DETERMINISM_MODE:
        if config.interleave_mode == "preemptive":
            finding = run_determinism_preemptive(config)
            if finding:
                return finding
            # If e9patch schedulers are available, also test cross-mechanism
            # replay (PMU record -> e9patch replay) on ~30% of runs.
            if e9_schedulers_available() and random.random() < 0.3:
                return run_determinism_preemptive_e9_replay(config)
            return None
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
            env=build_env(config),
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


# Number of retries to find a non-PMU config when PMU tokens are exhausted.
_PMU_RETRY_LIMIT = 3


def _record_pmu_acquired() -> None:
    """Atomically increment the PMU-acquired counter."""
    if PMU_ACQUIRED is not None:
        with PMU_ACQUIRED.get_lock():
            PMU_ACQUIRED.value += 1


def _record_pmu_redirected() -> None:
    """Atomically increment the PMU-redirected counter."""
    if PMU_REDIRECTED is not None:
        with PMU_REDIRECTED.get_lock():
            PMU_REDIRECTED.value += 1


def run_one(config: TestConfig) -> Optional[Finding]:
    """Run a single simulation, gating PMU access via the token pool.

    If this config needs PMU and no token is immediately available, we retry
    up to _PMU_RETRY_LIMIT times to regenerate a non-PMU config.  If all
    retries also need PMU, we sleep briefly and block-acquire a token.
    """
    if not needs_pmu_token(config) or PMU_TOKEN_POOL is None:
        return _run_config(config)

    if PMU_TOKEN_POOL.acquire(block=False):
        _record_pmu_acquired()
        try:
            return _run_config(config)
        finally:
            PMU_TOKEN_POOL.release()

    # PMU busy -- try to regenerate a non-PMU config.
    # We use a per-call RNG seeded from the config to stay deterministic
    # within each worker while still producing varied retries.
    retry_rng = random.Random(config.seed ^ config.iteration)
    for _ in range(_PMU_RETRY_LIMIT):
        alt = generate_configs(
            retry_rng,
            _WORKER_SCHEDULERS,
            _WORKER_WORKLOADS,
            _WORKER_MODES,
            use_random_workloads=_WORKER_USE_RANDOM_WORKLOADS,
            random_params=_WORKER_RANDOM_PARAMS,
            chaos=_WORKER_CHAOS,
        )
        alt.iteration = config.iteration
        if not needs_pmu_token(alt):
            _record_pmu_redirected()
            return _run_config(alt)

    # All retries also need PMU -- sleep briefly then block.
    time.sleep(0.1)
    PMU_TOKEN_POOL.acquire(block=True)
    _record_pmu_acquired()
    try:
        return _run_config(config)
    finally:
        PMU_TOKEN_POOL.release()


def _random_sim_params(
    rng: random.Random,
    mode: str,
    random_params: bool,
    chaos: bool,
) -> SimParams:
    """Generate randomized simulator parameters."""
    params = SimParams()
    if not random_params and not chaos:
        return params

    # Randomize rbc-ns: 50% chance of non-default
    if rng.random() < 0.5:
        params.rbc_ns = rng.choice(RBC_NS_CHOICES)
    # Randomize watchdog: 30% chance of non-default
    if rng.random() < 0.3:
        params.watchdog_timeout = rng.choice(WATCHDOG_CHOICES)
    # Randomize noise/overhead: 15% chance each of disabling
    if rng.random() < 0.15:
        params.no_noise = True
    if rng.random() < 0.15:
        params.no_overhead = True

    if mode == "native-concurrent":
        params.window_ns = rng.choice(WINDOW_NS_CHOICES)

    if mode in ("preemptive", "e9patch"):
        params.break_on = rng.choice(BREAK_ON_CHOICES)
        if params.break_on == "insn":
            params.timeslice_min, params.timeslice_max = rng.choice(TIMESLICE_INSN_CHOICES)
        else:
            params.timeslice_min, params.timeslice_max = rng.choice(TIMESLICE_RBC_CHOICES)

    # Default scxsim behavior is randomized same-time event ordering. Keep that
    # for chaos runs and include occasional fixed-priority controls.
    if rng.random() < (0.05 if chaos else 0.15):
        params.fixed_priority = True

    if chaos and not params.no_noise:
        params.env = {
            "SCX_SIM_RUN_JITTER_CV_PPM": str(rng.choice(RUN_JITTER_CV_PPM_CHOICES)),
            "SCX_SIM_TICK_JITTER_STDDEV_NS": str(rng.choice(TICK_JITTER_STDDEV_NS_CHOICES)),
            "SCX_SIM_INITIAL_TICK_SKEW_NS": str(rng.choice(INITIAL_TICK_SKEW_NS_CHOICES)),
        }
    return params


def generate_configs(
    rng: random.Random,
    schedulers: list[str],
    workloads: list[Path],
    modes: list[str],
    use_random_workloads: bool = False,
    random_params: bool = False,
    chaos: bool = False,
) -> TestConfig:
    """Generate a random test configuration.

    When use_random_workloads is True, 50% of configs use a randomly generated
    workload and randomized simulator parameters.
    """
    mode = rng.choice(modes)
    cpus = rng.choice(CPU_COUNTS)
    # e9patch with 1 CPU has no concurrent dispatch and thus no preemption,
    # so bias toward 2+ CPUs for e9patch mode.
    if mode == "e9patch" and cpus == 1:
        cpus = rng.choice([2, 4, 8])

    is_random = use_random_workloads and rng.random() < (0.8 if chaos else 0.5)
    if is_random:
        workload = generate_random_workload(rng, cpus)
    else:
        workload = rng.choice(workloads)
    sim_params = _random_sim_params(rng, mode, random_params, chaos)

    return TestConfig(
        scheduler=rng.choice(schedulers),
        workload=workload,
        cpus=cpus,
        seed=rng.randint(0, 2**32 - 1),
        interleave_mode=mode,
        iteration=0,
        is_random_workload=is_random,
        sim_params=sim_params,
    )


def save_finding(finding: Finding, finding_num: int) -> Path:
    """Write a finding report to disk."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    filename = f"finding_{finding_num:04d}_{finding.error_type}.txt"
    path = OUTPUT_DIR / filename
    path.write_text(finding.report())
    return path


def parse_int_list(value: str, flag: str) -> list[int]:
    """Parse a comma-separated positive integer list."""
    parsed: list[int] = []
    for item in value.split(","):
        item = item.strip()
        if not item:
            continue
        try:
            number = int(item)
        except ValueError:
            raise argparse.ArgumentTypeError(
                f"{flag} entries must be integers: {item!r}"
            )
        if number <= 0:
            raise argparse.ArgumentTypeError(
                f"{flag} entries must be positive: {item!r}"
            )
        parsed.append(number)
    if not parsed:
        raise argparse.ArgumentTypeError(f"{flag} must not be empty")
    return parsed


def write_run_record(record_path: Optional[Path], record: dict[str, Any]) -> None:
    """Append one JSONL run-manifest record."""
    if record_path is None:
        return
    record_path.parent.mkdir(parents=True, exist_ok=True)
    with record_path.open("a") as f:
        json.dump(record, f, sort_keys=True)
        f.write("\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description="Stress test scx_simulator")
    parser.add_argument(
        "--duration",
        type=float,
        default=10,
        help="Duration in minutes (default: 10). Supports fractions like 0.5 for 30s.",
    )
    parser.add_argument(
        "--max-runs",
        type=int,
        default=None,
        help="Stop after submitting this many runs, still bounded by --duration.",
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
    parser.add_argument(
        "--random-workloads",
        action="store_true",
        help="Enable randomized workload generation: 50%% fixed, 50%% randomly "
             "generated rt-app JSON. Also randomizes simulator parameters "
             "(rbc-ns, watchdog timeout, noise, overhead).",
    )
    parser.add_argument(
        "--random-params",
        action="store_true",
        help="Randomize simulator knobs even when --random-workloads is disabled.",
    )
    parser.add_argument(
        "--chaos",
        action="store_true",
        help="Enable aggressive timing/interleaving chaos knobs and run manifests.",
    )
    parser.add_argument(
        "--high-concurrency",
        action="store_true",
        help="Use large default CPU/task matrices for simulator concurrency stress.",
    )
    parser.add_argument(
        "--modes",
        type=str,
        default=None,
        help=f"Comma-separated interleave modes (default: auto from {','.join(INTERLEAVE_MODES)})",
    )
    parser.add_argument(
        "--cpu-counts",
        type=str,
        default=None,
        help="Comma-separated simulated CPU counts for random configs.",
    )
    parser.add_argument(
        "--task-counts",
        type=str,
        default=None,
        help="Comma-separated task counts for random workload generation.",
    )
    parser.add_argument(
        "--task-multipliers",
        type=str,
        default=None,
        help="Comma-separated task_count=cpus*multiplier choices for random workloads.",
    )
    parser.add_argument(
        "--run-record",
        type=Path,
        default=None,
        help="Append submitted/completed run records as JSONL at this path.",
    )
    parser.add_argument(
        "--max-pmu",
        type=int,
        default=MAX_PMU_CONCURRENT,
        help=f"Max concurrent PMU preemptive runs (default: {MAX_PMU_CONCURRENT})",
    )
    args = parser.parse_args()

    # Handle --list-workloads early (before other setup)
    all_workloads = get_available_workloads()
    if args.list_workloads:
        print("Available workloads:")
        for wl in all_workloads:
            print(f"  {wl.stem}")
        sys.exit(0)

    if args.max_runs is not None and args.max_runs <= 0:
        print("error: --max-runs must be positive", file=sys.stderr)
        sys.exit(1)

    # Set global config from CLI args
    global WATCHDOG_TIMEOUT, SIM_DURATION, DETERMINISM_MODE
    global CPU_COUNTS, TASK_COUNTS, TASK_MULTIPLIERS
    WATCHDOG_TIMEOUT = args.watchdog
    SIM_DURATION = args.sim_duration
    DETERMINISM_MODE = args.determinism

    if args.high_concurrency:
        CPU_COUNTS = [16, 32, 64, 128]
        TASK_COUNTS = [64, 128, 256, 512]
        TASK_MULTIPLIERS = [4, 8, 10]
    if args.cpu_counts:
        CPU_COUNTS = parse_int_list(args.cpu_counts, "--cpu-counts")
    if args.task_counts:
        TASK_COUNTS = parse_int_list(args.task_counts, "--task-counts")
    if args.task_multipliers:
        TASK_MULTIPLIERS = parse_int_list(args.task_multipliers, "--task-multipliers")

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
    if args.modes:
        modes = [m.strip() for m in args.modes.split(",") if m.strip()]
        unknown_modes = [m for m in modes if m not in INTERLEAVE_MODES]
        if unknown_modes:
            print(f"error: unknown mode(s): {', '.join(unknown_modes)}", file=sys.stderr)
            print(f"available: {', '.join(INTERLEAVE_MODES)}", file=sys.stderr)
            sys.exit(1)
        if "e9patch" in modes and not has_e9:
            print(
                "error: modes include e9patch but _e9.so variants are missing. "
                "Build with: make -C schedulers e9",
                file=sys.stderr,
            )
            sys.exit(1)
    elif args.e9patch:
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
    if args.random_workloads:
        mix = "80% random, 20% fixed" if args.chaos else "50% random, 50% fixed"
        print(f"Random workloads: enabled ({mix})")
        print(f"  Task counts: {TASK_COUNTS}")
        print(f"  Task multipliers: {TASK_MULTIPLIERS}")
        print(f"  Phase patterns: {PHASE_PATTERNS}")
    if args.random_params or args.random_workloads or args.chaos:
        print(f"  RBC-ns choices: {RBC_NS_CHOICES}")
        print(f"  Watchdog choices: {WATCHDOG_CHOICES}")
    if args.chaos:
        print(f"Chaos env RUN_JITTER_CV_PPM: {RUN_JITTER_CV_PPM_CHOICES}")
        print(f"Chaos env TICK_JITTER_STDDEV_NS: {TICK_JITTER_STDDEV_NS_CHOICES}")
        print(f"Chaos env INITIAL_TICK_SKEW_NS: {INITIAL_TICK_SKEW_NS_CHOICES}")
    print(f"Interleave modes: {', '.join(modes)}")
    print(f"CPU counts: {CPU_COUNTS}")
    print(f"Watchdog: {WATCHDOG_TIMEOUT}, sim duration: {SIM_DURATION}")
    if args.max_runs is not None:
        print(f"Max runs: {args.max_runs}")
    print(f"Max PMU concurrent: {args.max_pmu}")
    if "e9patch" in modes:
        print(f"e9patch determinism repeats: {E9_DETERMINISM_REPEATS}")
    print(f"Output: {OUTPUT_DIR}")
    print(f"Log: {log_path}")
    if args.run_record:
        print(f"Run record: {args.run_record}")
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
    log.info(f"CPU counts: {CPU_COUNTS}")
    log.info(f"Task counts: {TASK_COUNTS}")
    log.info(f"Task multipliers: {TASK_MULTIPLIERS}")
    log.info(f"Watchdog timeout: {WATCHDOG_TIMEOUT}")
    log.info(f"Sim duration: {SIM_DURATION}")
    log.info(f"Max runs: {args.max_runs}")
    log.info(f"Max PMU concurrent: {args.max_pmu}")
    log.info(f"Run record: {args.run_record}")

    # Initialize shared PMU token pool and stats counters
    pmu_pool = multiprocessing.Semaphore(args.max_pmu)
    pmu_acquired = multiprocessing.Value("i", 0)
    pmu_redirected = multiprocessing.Value("i", 0)

    findings: list[Finding] = []
    total_runs = 0
    completed_runs = 0
    record_path: Optional[Path] = args.run_record
    if record_path is not None and record_path.exists():
        record_path.unlink()

    # Pre-generate a batch of configs
    batch_size = args.jobs * 4

    # Serialize workload paths as strings for cross-process pickling
    workload_strs = [str(w) for w in workloads]

    try:
        with ProcessPoolExecutor(
            max_workers=args.jobs,
            initializer=_init_worker,
            initargs=(
                pmu_pool, pmu_acquired, pmu_redirected,
                schedulers, workload_strs, modes,
                args.random_workloads,
                args.random_params or args.random_workloads,
                args.chaos,
                CPU_COUNTS,
                TASK_COUNTS,
                TASK_MULTIPLIERS,
            ),
        ) as pool:
            pending: dict[Future[Optional[Finding]], TestConfig] = {}
            iteration = 0

            def under_run_limit() -> bool:
                return args.max_runs is None or total_runs < args.max_runs

            while (time.monotonic() < deadline and under_run_limit()) or pending:
                # Submit new work while under deadline
                while (
                    len(pending) < batch_size
                    and time.monotonic() < deadline
                    and under_run_limit()
                ):
                    config = generate_configs(
                        rng, schedulers, workloads, modes,
                        use_random_workloads=args.random_workloads,
                        random_params=args.random_params or args.random_workloads,
                        chaos=args.chaos,
                    )
                    config.iteration = iteration
                    iteration += 1
                    write_run_record(record_path, {
                        "event": "submitted",
                        "timestamp": datetime.now().isoformat(),
                        "config": config.to_record(),
                    })
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
                        write_run_record(record_path, {
                            "event": "completed",
                            "timestamp": datetime.now().isoformat(),
                            "iteration": config.iteration,
                            "status": "finding",
                            "finding_number": num,
                            "error_type": finding.error_type,
                            "finding_path": str(path),
                        })
                        log.warning(f"BUG #{num}: {finding.summary()} -> {path.name}")
                        print(
                            f"  BUG #{num}: {finding.summary()}"
                            f"  -> {path.name}"
                        )
                    else:
                        write_run_record(record_path, {
                            "event": "completed",
                            "timestamp": datetime.now().isoformat(),
                            "iteration": config.iteration,
                            "status": "pass",
                        })
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
    pmu_acq = pmu_acquired.value
    pmu_redir = pmu_redirected.value
    print(f"\n\n{'=' * 60}")
    print(f"Stress test complete")
    print(f"{'=' * 60}")
    print(f"  Total runs:  {completed_runs}")
    print(f"  Findings:    {len(findings)}")
    print(f"  Master seed: {master_seed}")
    print(f"  Elapsed:     {elapsed_total:.1f} minutes")
    print(f"  PMU tokens:  {pmu_acq} acquired, {pmu_redir} redirected to non-PMU")

    log.info("=" * 60)
    log.info("Stress test complete")
    log.info("=" * 60)
    log.info(f"Total runs: {completed_runs}")
    log.info(f"Findings: {len(findings)}")
    log.info(f"Master seed: {master_seed}")
    log.info(f"Elapsed: {elapsed_total:.1f} minutes")
    log.info(f"PMU tokens: {pmu_acq} acquired, {pmu_redir} redirected to non-PMU")

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

    # Clean up generated workload temp directory
    if _GENERATED_WORKLOADS_DIR and _GENERATED_WORKLOADS_DIR.exists():
        shutil.rmtree(_GENERATED_WORKLOADS_DIR, ignore_errors=True)
        log.info(f"Cleaned up generated workloads: {_GENERATED_WORKLOADS_DIR}")

    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
