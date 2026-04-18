# repm — ReproMagic

**Capture and reproduce Linux scheduling behavior as reproducible experiments.**

`repm` is a self-contained CLI tool that automates the full lifecycle of Linux scheduler benchmarking: workspace setup, trace capture, workload generation, experiment execution, and results analysis. Every number in a report traces back to a specific CSV file, line number, and computation.

## Quick Start

```bash
# Build
cd repm
cargo build --release

# Scaffold a workspace
./target/release/repm init --project-name my_experiment --new-git

# Generate an rt-app workload config
./target/release/repm gen-config --foreground 4 --background 16

# Run experiments (simulator mode — no root needed)
./target/release/repm run --mode rtapp-sim --reps 3

# Analyze results
./target/release/repm analyze --citations --cross-check --write
```

Or run the full pipeline interactively:

```bash
./target/release/repm magic
```

## Installation

### Prerequisites

- **Rust** (stable toolchain, edition 2021)
- **rt-app** — real-time workload generator (required for `rtapp-pinned` mode)
- **scxsim** — sched_ext simulator (optional, for `rtapp-sim` mode)
- **vng** — virtme-ng (optional, for `rtapp-vm` mode)
- **sudo** access (for bare-metal scheduler testing)

### Build

```bash
cd repm
cargo build --release
# Binary: target/release/repm
```

## Subcommands

### `repm init` — Scaffold a Workspace

Creates a workspace with directory structure, configuration file, CLAUDE.md, and Claude skill files.

```bash
repm init --project-name irq_latency --new-git
repm init --project-name irq_latency --use-git /path/to/repo
```

**Flags:**
| Flag | Required | Description |
|------|----------|-------------|
| `--project-name <NAME>` | Yes | Project identifier (`[a-zA-Z0-9_-]+`) |
| `--new-git` | No | Create a new git repository |
| `--use-git <PATH>` | No | Use an existing git repository |

**Created structure:**
```
my_experiment/
├── repromagic_config.toml    # Workspace configuration
├── CLAUDE.md                 # AI agent instructions (template-expanded)
├── .claude/skills/           # 5 skill files for Claude Code
├── bin/schedulers/           # Scheduler binaries
├── configs/                  # rt-app JSON configs
├── traces/                   # Captured scheduling traces
├── experiments/              # Experiment data and results
└── reports/                  # Generated reports
```

---

### `repm magic` — Full Pipeline Wizard

Chains `init → gen-config → run → analyze` into one interactive (or headless) command.

```bash
# Interactive — prompts for all parameters
repm magic

# Headless — uses defaults, good for CI
repm magic --headless --project-name ci_test

# Dry run — show the plan without executing
repm magic --dry-run --project-name ci_test

# Selective — skip stages you've already completed
repm magic --skip-init --skip-gen-config
```

**Key flags:**
| Flag | Description |
|------|-------------|
| `--headless` | No interactive prompts, use defaults |
| `--dry-run` | Show plan without executing |
| `--project-name <NAME>` | Skip project name prompt |
| `--phenomenon <TYPE>` | Phenomenon: `bad_tail_latency`, `bad_cpu_util`, `bad_throughput` |
| `--schedulers <LIST>` | Comma-separated scheduler keys |
| `--reps <N>` | Repetitions per cell |
| `--duration <SECS>` | Experiment duration |
| `--mode <MODES>` | Comma-separated: `rtapp-pinned`, `rtapp-vm`, `rtapp-sim` |
| `--foreground <N>` | Number of foreground threads |
| `--background <N>` | Number of background threads |
| `--skip-init` | Skip workspace scaffolding |
| `--skip-gen-config` | Skip config generation |
| `--skip-run` | Skip experiment execution |
| `--skip-analyze` | Skip analysis |

---

### `repm capture` — Trace Collection

Collects scheduling traces from a local or remote host via SSH.

```bash
# Local capture (30 seconds)
repm capture --duration 30

# Remote capture via SSH
repm capture --ssh root@prod-host --duration 60

# Dry run — show commands without executing
repm capture --ssh root@prod-host --dry-run
```

**Flags:**
| Flag | Default | Description |
|------|---------|-------------|
| `--ssh <HOST>` | local | Remote host (e.g., `root@prod-host`) |
| `--duration <SECS>` | 30 | Capture duration |
| `--scheduler <KEY>` | — | Scheduler key from config to run during capture |
| `--dry-run` | false | Show commands without executing |
| `--version <TAG>` | "default" | Version tag for output directory |
| `--ssh-timeout <SECS>` | config | SSH connection timeout |
| `-i, --identity <FILE>` | config | SSH identity file |

**Collected data:** host metadata (hostname, kernel, arch, CPU count, sched_ext status), `/proc/interrupts` sampling, optional LAVD stats, user-configured trace commands. All outputs include a `provenance.json` for traceability.

---

### `repm gen-config` — Generate rt-app Workload

Generates parameterized rt-app JSON configuration files for reproducible workloads.
Two modes: **parameterized** (specify timing directly) or **from-trace** (blind synthesis).

```bash
# Default workload (4 fg, 16 bg threads)
repm gen-config

# Custom thread counts and timing
repm gen-config --foreground 8 --background 32 --fg-run-us 1000 --fg-sleep-us 2000

# BLIND SYNTHESIS: infer config from captured trace data
repm gen-config --from-trace scxsim_output.txt --verbose
repm gen-config --from-trace trace.json --format sim -o synthesized.json
repm gen-config --from-trace logs/ --cores 8 --duration 30

# With IRQ generator threads on even CPUs
repm gen-config --with-irq --irq-run-us 5000 --irq-sleep-us 5000

# Output to custom path
repm gen-config -o my_workload.json
```

**Flags:**
| Flag | Default | Description |
|------|---------|-------------|
| `--foreground <N>` | config (4) | Foreground (latency-sensitive) threads |
| `--background <N>` | config (16) | Background (CPU pressure) threads |
| `--cores <N>` | config (8) | Number of cores |
| `--fg-run-us <US>` | 500 | Foreground compute time (µs) |
| `--fg-sleep-us <US>` | 1500 | Foreground sleep time (µs) |
| `--bg-run-us <US>` | 130 | Background compute time (µs) |
| `--bg-sleep-us <US>` | 950 | Background sleep time (µs) |
| `--bg-priority <NICE>` | 10 | Background thread nice value |
| `--duration <SECS>` | config (30) | Experiment duration |
| `-o, --output <PATH>` | configs/rtapp.json | Output file |
| `--from-trace <PATH>` | — | Blind synthesis from trace data (see below) |
| `--format <FMT>` | rtapp | Output format: `rtapp` (phased) or `sim` (simple) |
| `--verbose` | false | Show classification decisions |
| `--log-basename <NAME>` | "repromagic" | rt-app log file prefix |
| `--with-irq` | false | Add IRQ generator threads (SCHED_FIFO, pinned to even CPUs) |

**Blind synthesis (`--from-trace`)** automatically detects the input format:

| Input | Description |
|-------|-------------|
| **scxsim verbose-summary** | Text file from `scxsim run --verbose-summary 2>file.txt` |
| **Perfetto JSON trace** | Chrome trace format with `traceEvents` array |
| **rt-app log directory** | Directory of `*.log` files from rt-app |
| **Metrics CSV** | 14-column canonical format from `repm run` |

The pipeline: **parse → classify → synthesize**. Thread classes are grouped by
run/sleep similarity (within 2×). Use `--verbose` to see classification decisions.

---

### `repm run` — Execute Experiment Matrix

Runs the full experiment matrix: scheduler × mode × repetition.

```bash
# Run with simulator (no root needed)
repm run --mode rtapp-sim --reps 5

# Run bare-metal with specific schedulers
repm run --mode rtapp-pinned --schedulers lavd_v1,eevdf --reps 3

# Dry run — show the experiment matrix
repm run --dry-run

# Create a new experiment version
repm run --new-version "irq_comparison"

# Resume a partially completed experiment
repm run --experiment v001_initial
```

**Flags:**
| Flag | Default | Description |
|------|---------|-------------|
| `--mode <MODES>` | rtapp-pinned | Comma-separated: `rtapp-pinned`, `rtapp-vm`, `rtapp-sim` |
| `--schedulers <LIST>` | all from config | Comma-separated scheduler keys |
| `--reps <N>` | config (3) | Repetitions per cell |
| `--dry-run` | false | Show matrix without executing |
| `--new-version [<NAME>]` | — | Create a new experiment version |
| `--experiment <VERSION>` | — | Resume or target specific experiment |
| `--purpose <TEXT>` | — | Purpose description for experiment README |
| `--duration <SECS>` | config (30) | Duration override |

**Execution modes:**
- **`rtapp-pinned`** — Bare metal: starts scheduler via `sudo`, runs rt-app, collects metrics from log files.
- **`rtapp-sim`** — Simulator: runs `scxsim` with generated workloads, no root needed.
- **`rtapp-vm`** — VM mode via virtme-ng (planned).

**Resume support:** Uses `.done` marker files to track completed repetitions. Only incomplete cells are re-executed on resume.

**Provenance:** Each experiment gets an immutable `provenance.json`, a frozen `config_snapshot.toml`, and an auto-generated `README.md`. Config hash validation prevents resuming experiments after configuration changes.

**CSV output schema:**
```
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
```

---

### `repm analyze` — Results Analysis

Reads experiment CSVs and generates markdown comparison tables with full source citations.

```bash
# Analyze latest experiment
repm analyze

# Analyze a specific experiment
repm analyze v001_initial

# With source citations and cross-checks
repm analyze --citations --cross-check

# Write results to RESULTS.md
repm analyze --write

# Compare two experiments
repm analyze --compare v001_baseline v002_optimized

# CSV output
repm analyze --format csv
```

**Arguments & Flags:**
| Flag | Default | Description |
|------|---------|-------------|
| `[EXPERIMENT]` | auto-discover | Experiment version to analyze |
| `--compare <V1> <V2>` | — | Compare two experiment versions |
| `--thread-type <TYPE>` | "foreground" | Thread type filter |
| `--format <FMT>` | markdown | Output format: `markdown` or `csv` |
| `--citations` | false | Show source citations (file, line, computation) |
| `--write` | false | Write to `RESULTS.md` instead of stdout |
| `--cross-check` | false | Run 5 automated validation checks |

**Example output:**

```
# Experiment Results: v001_stats_7rep

Generated by `repm analyze` — all values traced to source CSVs.

| Mode | Scheduler | E2E P50 (µs) | E2E P99 (µs) | Sched P99 (µs) | IRQ Exp | CPU% | Rep |
|:-----|:----------|:------------:|:------------:|:--------------:|:-------:|:----:|:---:|
| rtapp_sim  | lavd      |         1087 |         1087 |            180 | NO DATA | 36.9% |   7 |
| rtapp_sim  | tickless  |         1093 |         1093 |           47.8 | NO DATA | 89.5% |   2 |
```

With `--citations`, every value includes a trace to `<file>:<lines> (<computation>)`.

**Cross-check validations:** P99 ≥ P50, scheduling latency ≤ E2E latency, IRQ exposure 0–100%, sim+EEVDF exclusion, CPU utilization sanity.

## Configuration Reference

`repm init` generates a `repromagic_config.toml` at the workspace root:

```toml
# repromagic_config.toml — workspace configuration

[project]
name = "my_experiment"
phenomenon = "bad_tail_latency"
# description = "Describe the scheduling phenomenon under investigation"

[defaults]
cores = 8                  # CPU cores for experiments
duration = 30              # Experiment duration in seconds
reps = 3                   # Repetitions per cell
warmup = 5                 # Warmup exclusion period in seconds

[topology]
workload_cpus = "0-7"
# irq_cpus = [0, 2, 4, 6]       # Optional: IRQ target CPUs
# generator_cpus = "8-11"        # Optional: IRQ generator CPU set

[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
# No binary — kernel default, no sched_ext

# [schedulers.lavd_v1]
# name = "lavd"
# label = "LAVD-v1"
# provenance = { repo = "https://github.com/sched-ext/scx", revision = "abc1234" }
# binary = "bin/schedulers/scx_lavd_v1_abc1234"
# flags = ["--performance"]

[workload]
foreground_threads = 4     # Latency-sensitive threads
background_threads = 16    # CPU pressure / hog threads

[capture]
# default_host = "root@prod-host"    # Default SSH target
ssh_timeout = 30
# ssh_identity = "~/.ssh/id_rsa"
remote_workdir = "/tmp/repm_capture"
collect_interrupts = true
interrupts_interval = 1
# collect_lavd_stats = false

# [[capture.trace_commands]]
# name = "proc_interrupts"
# command = "cat /proc/interrupts"
# background = false
```

### Configuration Sections

| Section | Purpose |
|---------|---------|
| `[project]` | Name, phenomenon type (`bad_tail_latency`, `bad_cpu_util`, `bad_throughput`), description |
| `[defaults]` | Core count, duration, reps, warmup period |
| `[topology]` | CPU sets for workload and IRQ threads |
| `[schedulers.<key>]` | Scheduler definitions: name, label, binary path, flags, provenance |
| `[workload]` | Thread counts and optional per-type overrides |
| `[capture]` | SSH settings, trace commands, interrupt sampling config |

## Architecture

```
repm
├── main.rs          # CLI entry point (clap derive)
├── config.rs        # TOML configuration schema and validation
├── workspace.rs     # Workspace discovery and directory management
├── templates.rs     # CLAUDE.md template expansion, skill file installation
├── trace.rs         # Trace ingestion: rt-app logs, metrics CSV, Perfetto JSON
├── synthesis.rs     # Blind synthesis: parse → classify → synthesize workloads
├── score.rs         # Convergence scoring for blind synthesis accuracy
└── commands/
    ├── init.rs      # Workspace scaffolding
    ├── magic.rs     # Full pipeline wizard
    ├── capture.rs   # Trace collection (local + SSH)
    ├── gen_config.rs # rt-app JSON generation + --from-trace blind synthesis
    ├── run.rs       # Experiment matrix execution
    └── analyze.rs   # Results analysis and comparison
```

### Pipeline Flow

```
capture (optional)        Collect traces from production
    │
    ▼
init                      Scaffold workspace + config
    │
    ▼
gen-config                Generate rt-app workload JSON
    │
    ▼
run                       Execute scheduler × mode × rep matrix
    │                     Output: per-rep CSV files + provenance
    ▼
analyze                   Markdown tables + citations + cross-checks
```

### Data Provenance

Every experiment records:
- **`provenance.json`** — immutable metadata (creation time, config hash, git state)
- **`config_snapshot.toml`** — frozen copy of config at experiment creation
- **Source citations** — every value in analysis traces to `file:lines (computation)`
- **Config hash integrity** — prevents resuming experiments after config changes

## Testing

```bash
# Unit and integration tests
cargo test

# End-to-end pipeline test
bash tests/e2e_pipeline.sh
```

## License

See the project root for license information.
