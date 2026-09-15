---
name: rtapp-usage
description: Build, configure, and run rt-app workload generator
---

# rt-app Usage

## What it does

rt-app generates precise scheduling workloads with configurable
compute/sleep phases. It supports `clockonly` runtime mode for
wall-clock spinning without CPU calibration.

## Config generation

**NEVER create rt-app JSON configs by hand.** Always generate them:

```bash
# From workspace defaults
repm gen-config

# With overrides
repm gen-config --cores 24 --background 48

# Output goes to configs/ directory
```

All compute phases use `runtime` with `clockonly` mode (wall-clock
spinning, no CPU calibration).

## Running workloads

```bash
# Direct invocation
sudo rt-app configs/rtapp.json

# Via repm (preferred -- handles scheduler lifecycle)
repm run --mode rtapp-pinned --reps 3
```

## Key parameters (from repromagic_config.toml)

| Parameter | What it controls |
|-----------|-----------------|
| `foreground_threads` | Number of latency-sensitive foreground threads |
| `background_threads` | Number of CPU pressure / hog threads |
| `fg_run_us` | Foreground thread compute phase duration (microseconds) |
| `fg_sleep_us` | Foreground thread sleep phase duration (microseconds) |
| `bg_run_us` | Background hog compute phase (microseconds) |
| `bg_sleep_us` | Background hog sleep phase (microseconds) |
| `cores` | Total CPU count for thread placement |

## Output

- Per-thread log files: `<basename>-<thread_name>-<tid>.log`
- Each line: `<iteration> <period_us> <run_us> <start_us> <end_us> <slack_us>`
- `slack_us` is the scheduling latency proxy used by `repm analyze`

## Thread types

- **Foreground (`fg_thread_*`)**: Latency-sensitive threads with defined
  compute + sleep characteristics. These model the critical path.
- **Background (`bg_hog_*`)**: CPU pressure threads that compete for
  resources. These model background load.
- **IRQ generators (`irq_gen_*`)**: Optional. Pinned to specific CPUs
  with SCHED_FIFO policy. Enable with `repm gen-config --with-irq`.
