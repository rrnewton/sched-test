# Cache Reproducer Methodology

**Version:** 1.0
**Date:** 2026-04-17
**Status:** Authoritative — all experiment reports MUST conform to this methodology.

---

## 1. Purpose

This document defines the methodology for reproducing and measuring the
scheduling latency impact of LAVD's IRQ avoidance feature on cache-serving
workloads. It specifies:

- The valid experiment modes and scheduler combinations
- Independent variables (calibration parameters we control)
- Dependent variables (metrics we measure)
- Statistical and procedural requirements for valid experiments
- Data provenance rules

The goal is a rigorous, reproducible comparison of **EEVDF** (Linux default)
vs **LAVD-PreIRQ** (baseline) vs **LAVD-PostIRQ** (with IRQ avoidance) across
multiple execution environments, demonstrating that LAVD's IRQ avoidance
measurably reduces tail latency for latency-sensitive threads.

---

## 2. Valid Mode x Scheduler Matrix

Each experiment runs a workload under a specific **mode** (execution
environment) and **scheduler**. Not all combinations are possible.

### Canonical mode names

Always use these exact names. Abbreviations like "sim" or "pinned" are
ambiguous and MUST NOT appear in data, filenames, or reports.

| Mode | Description |
|------|-------------|
| `rtapp_pinned` | rt-app workload on bare metal, CPUs pinned via taskset |
| `rtapp_floating` | rt-app workload on bare metal, no CPU pinning |
| `rtapp_vm` | rt-app workload inside a virtme-ng VM |
| `purerust_pinned` | ucache_purerust binary on bare metal, CPUs pinned |
| `purerust_floating` | ucache_purerust binary on bare metal, no pinning |
| `purerust_vm` | ucache_purerust binary inside a virtme-ng VM |
| `rtapp_sim` | scx-sim deterministic simulator running rt-app JSON spec |
| `production` | Live production trace capture and analysis |

### Valid combinations

```
                        EEVDF    LAVD-PreIRQ    LAVD-PostIRQ
                        ─────    ───────────    ────────────
rtapp_pinned              ✅         ✅              ✅
purerust_pinned           ✅         ✅              ✅
rtapp_floating            ✅         ✅              ✅
purerust_floating         ✅         ✅              ✅
rtapp_vm                  ✅         ✅              ✅
purerust_vm               ✅         ✅              ✅
rtapp_sim                 ❌         ✅              ✅
production                ✅         ✅              ✅
```

### Critical constraint: rtapp_sim x EEVDF is IMPOSSIBLE

The scx-sim simulator implements the LAVD scheduling algorithm only. It
does not and cannot simulate the kernel's EEVDF scheduler. Any data
labeled "rtapp_sim EEVDF" or "sim EEVDF" is **invalid by construction**
and must be rejected.

The simulator is valuable for comparing LAVD-PreIRQ vs LAVD-PostIRQ in
a deterministic, reproducible environment — but it cannot participate
in EEVDF comparisons. Use bare-metal or VM modes for those.

### Scheduler definitions

| Scheduler | Binary / Source | Description |
|-----------|----------------|-------------|
| **EEVDF** | Linux default (no sched_ext) | Earliest Eligible Virtual Deadline First — the kernel's built-in CFS replacement. Baseline with no IRQ awareness. |
| **LAVD-PreIRQ** | `scx_lavd_baseline` (pre-IRQ commit) | LAVD without IRQ avoidance patches. Latency-aware virtual deadline scheduling but no softirq steering. |
| **LAVD-PostIRQ** | `scx_lavd_with_irq_fixes` (post-IRQ commit) | LAVD with IRQ avoidance: concentrates softirq processing on designated CPUs and steers latency-sensitive threads away from those CPUs. |

---

## 3. Workload Specification (Independent Variables)

All calibration parameters are controlled to match production characteristics.
The source of truth is `scripts/gen_config.py` (canonical v0.9/v0.10 spec).

### 3.1 Topology

| Parameter | Default | Production Reference | Source |
|-----------|---------|---------------------|--------|
| Total CPUs | 12 (cores 0–11) | ~250 cores per host | `gen_config.py` line 43: `cores: int = 12` |
| Tasks per CPU | 6.0 (72 tasks / 12 CPUs) | ~6.0 | `gen_config.py` defaults; production: `ai_docs/irq_avoidance_plan.md` §4 |
| IRQ CPUs | Even-numbered (0,2,4,6,8,10) | Even-numbered | `gen_config.py` line 74: `irq_cpus = [c for c in all_cpus if c % 2 == 0]` |

### 3.2 Task mix

| Thread Type | Count | Run (us) | Sleep (us) | Duty Cycle | Priority | Source |
|-------------|------:|----------|-----------|-----------|----------|--------|
| cache_worker | 8 | 500 | 1500 | 25% | SCHED_OTHER | `gen_config.py` lines 46–47 |
| ssd_reader | 1 | 35 | 250 | 12% | SCHED_OTHER | `gen_config.py` lines 49–50 |
| ssd_writer | 1 | 22 | 198 | 10% | SCHED_OTHER | `gen_config.py` lines 51–52 |
| background_hog | 56 | 130 | 950 | 12% | nice +10 | `gen_config.py` lines 53–55 |
| irq_gen | 6 | 5000 | 5000 | 50% | SCHED_FIFO | `gen_config.py` lines 56–57 |
| **Total** | **72** | | | | | |

### 3.3 Calibration targets

These are the aggregate workload characteristics we tune to match production.

| Metric | Target | Production Value | How Computed | Source |
|--------|--------|-----------------|--------------|--------|
| Aggregate CPU utilization | ~86% | 88.5% | Sum of (run / (run + sleep)) × count for each thread type, divided by total CPUs. See derivation below. | Production: `ai_docs/irq_avoidance_plan.md` §4 |
| Wake frequency | 379–621 Hz per worker | ~500 Hz | 1 / (run + sleep) for cache_worker = 1 / 2000us = 500 Hz | Production: `ai_docs/irq_avoidance_plan.md` §4 |
| Tasks per CPU | >5 | ~6.0 | total_tasks / total_CPUs = 72 / 12 = 6.0 | `gen_config.py` defaults |
| IRQ placement | Even-numbered CPUs | Even-numbered | `softirq_target_cpus` in JSON config | `gen_config.py` line 86 |

**CPU utilization derivation:**

```
Worker contribution:  8 × 500 / (500 + 1500)  = 8 × 0.25   = 2.00 CPU-equivalents
Reader contribution:  1 × 35  / (35 + 250)    = 1 × 0.123  = 0.12 CPU-equivalents
Writer contribution:  1 × 22  / (22 + 198)    = 1 × 0.10   = 0.10 CPU-equivalents
Hog contribution:    56 × 130 / (130 + 950)   = 56 × 0.120 = 6.74 CPU-equivalents
IRQ contribution:     6 × 5000/ (5000 + 5000) = 6 × 0.50   = 3.00 CPU-equivalents
─────────────────────────────────────────────────────────────
Total:                                                       11.96 CPU-equivalents
Utilization:          11.96 / 12 CPUs                      ≈ 99.7%

Source: gen_config.py default parameters (lines 43–57).
Note: IRQ generators run at SCHED_FIFO and are pinned to even CPUs,
so workload CPUs see less contention. Effective workload utilization
excluding IRQ gens: (11.96 - 3.00) / 12 ≈ 74.7%.
```

### 3.4 Compute mode: `runtime` clockonly

All compute phases use the `runtime` JSON key with `"mode": "clockonly"`:

```json
"compute": {
  "runtime": { "duration": 500, "mode": "clockonly" },
  "loop": 1
}
```

This spins using `clock_gettime(CLOCK_MONOTONIC)` — pure wall-clock time.

**Why this matters:**
- **No calibration phase:** The legacy `"run"` key required rt-app to
  calibrate CPU speed at startup (10–30s of single-threaded work),
  which skewed early utilization measurements.
- **Frequency-invariant:** Not affected by CPU frequency scaling or
  governor settings.
- **Source:** `gen_config.py` function `_runtime()` (line 37);
  `ai_docs/irq_avoidance_plan.md` §2 "The runtime clockonly spec".

### 3.5 Consistent parameters across modes

**Rule:** Every mode participating in a cross-mode comparison MUST use
matched parameters. A mismatch between modes is a **blocker** — only gaps
between production and reproducers are acceptable.

| What must match | How to verify |
|-----------------|---------------|
| Thread counts | Compare task counts in rt-app JSON vs purerust CLI flags |
| Timing (run/sleep) | Check that E2E P50 under EEVDF is ~400–500us for cache_worker |
| IRQ generation method | All real-system modes use rt-app-rs UDP loopback |
| Background hogs present | Confirm 56 hog threads in config (or scaled equivalent) |
| Warmup exclusion period | Same `--warmup` value across all modes |

**Historical bugs this prevents:**
- Experiment 0.4 was invalidated: rt-app had 8 workers vs purerust's 16,
  producing a 5.7x E2E P50 discrepancy (source: `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §1.1).
- Early experiments used 10x-scaled timings intended for simulator, not
  bare metal (source: `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §1.2).

---

## 4. Dependent Variables (What We Measure)

Three metrics in order of primacy, as defined by
`experiments/METRICS_SPECIFICATION.md` v2.0.

### 4.1 End-to-End (E2E) Request Latency

**Definition:** Wall-clock time from request arrival at `cache_worker` to
response completion — one full suspend → resume → reply cycle.

**Unit:** nanoseconds (ns)

**Percentiles reported:** P50, P90, P99, P99.9, Max, Mean

**Measurement per mode:**

| Mode | Method | Source |
|------|--------|--------|
| `rtapp_pinned` / `rtapp_floating` | `period_usec` column in per-thread log × 1000 | `scripts/modes/rtapp.py` `_parse_rtapp_logs()` |
| `purerust_pinned` / `purerust_floating` | Instrumented timestamps: request start → response complete | ucache_purerust CSV output |
| `rtapp_sim` | Cycle time between successive `TaskScheduled` events for `cache_worker_0` | scx-sim trace events |
| `rtapp_vm` / `purerust_vm` | Same as bare-metal counterpart, inside VM | `scripts/modes/vm.py` |
| `production` | Proxy: sum of run slices + scheduling latencies on critical path | `scripts/lib/trace_analysis.py` |

**What it includes:** Compute time + I/O wait + scheduling delays + IRQ stolen time.
**What it excludes:** Network RTT (not modeled in cartoons).

### 4.2 Scheduling Latency

**Definition:** Time from `TASK_WAKING` to first instruction on CPU
(runnable → running transition).

**Applies to:** `cache_worker` and `ssd_reader` ONLY (critical path threads).

**Unit:** nanoseconds (ns)

**Percentiles reported:** P50, P90, P99, P99.9, Max, Mean

**Measurement per mode:**

| Mode | Method | Source |
|------|--------|--------|
| `rtapp_pinned` / `rtapp_floating` | `wu_lat` column in per-thread log × 1000 | `scripts/modes/rtapp.py` |
| `purerust_pinned` / `purerust_floating` | `clock_gettime(MONOTONIC)` after futex_wait returns vs wake timestamp | ucache_purerust instrumentation |
| `rtapp_sim` | `time(TaskScheduled) - time(TaskWoke)` per PID | scx-sim trace events |
| `production` | `thread_state.dur` where `state = 'R'` (runnable) | Perfetto SQL via `scripts/lib/trace_analysis.py` |

**Why this matters:** This metric is most directly affected by IRQ avoidance.
A thread on a clean CPU gets scheduled in ~2–5 us; on an IRQ-heavy CPU, it
may wait 100–200 us for the interrupt handler to finish.

### 4.3 IRQ Exposure

**Definition:** Fraction of a thread type's total **runtime nanoseconds**
that execute on IRQ-turbulent CPUs (>1,000 NET_RX softirqs/second).

**This is runtime-weighted, NOT count-weighted.** A thread scheduled 10 times
on an IRQ CPU with 5 us slices contributes less than 2 slices of 500 us each
on clean CPUs. The runtime-weighted metric is more meaningful.

**Applies to:** `cache_worker` and `ssd_reader` (critical path threads).

**Unit:** Percentage (0–100%), lower is better.

**Measurement per mode:**

| Mode | Method | Source |
|------|--------|--------|
| `rtapp_pinned` / `rtapp_floating` | Per-iteration: `run_duration × is_irq_cpu(cpu_id)`, summed and divided by total runtime | `scripts/modes/rtapp.py` |
| `purerust_pinned` / `purerust_floating` | `sched_getcpu()` at compute phase boundaries; weight by phase duration | ucache_purerust instrumentation |
| `rtapp_sim` | `TaskScheduled → TaskStopped` intervals: duration on IRQ CPUs / total runtime | scx-sim trace events |
| `production` | `sched_slice`: CPU in IRQ set, weighted by `dur` | Perfetto SQL |

### 4.4 Supplementary metrics

| Metric | Unit | When Reported |
|--------|------|---------------|
| CPU utilization (per-CPU, aggregate) | % via `/proc/stat` | All bare-metal and VM modes |
| Wake frequency (observed) | Hz | All modes; cross-check against configured 500 Hz |
| Softirq delta | count | All bare-metal modes; from `/proc/softirqs` |
| LAVD lat_cri | dimensionless | rtapp_sim only (LAVD scheduler internal) |

---

## 5. Statistical and Procedural Requirements

### 5.1 Randomized run order

The scheduler × condition × rep matrix MUST be shuffled before execution
to avoid systematic ordering effects (e.g., thermal throttling, background
load drift).

**Implemented by:** `scripts/run_experiment.py` line ~100 (random.shuffle
of run matrix). Source: `ai_docs/irq_avoidance_plan.md` §7.

### 5.2 Minimum repetitions

- **N ≥ 3** repetitions per (scheduler × condition) cell. N ≥ 5 preferred.
- Report N alongside every percentile value.
- **Flag N=1 captures as a limitation** — single-rep data is directional
  only and must not be used for statistical claims.

**Sample count requirements for percentiles:**
- N ≥ 100 for P50, P90, P99
- N ≥ 1,000 for P99.9
- Source: `experiments/METRICS_SPECIFICATION.md` §"Validity Checks" item 2.

### 5.3 Warmup exclusion

Discard the first measurement period to exclude:
- Scheduler ramp-up transients
- CPU frequency governor settling
- Cache cold-start effects

**Default:** 1 second for rtapp/purerust modes (source: `scripts/modes/rtapp.py`
warmup parameter); 5 seconds recommended per `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §3.3.

### 5.4 Duration matching

All scheduler runs in a comparison MUST have the same duration (±5%).
Source: `experiments/METRICS_SPECIFICATION.md` §"Validity Checks" item 4.

### 5.5 Cross-checking

Every metric MUST be cross-checked from ≥ 2 independent sources before
publication. Examples:

| Cross-check | What to compare |
|-------------|-----------------|
| rtapp_pinned vs purerust_pinned | E2E P99 within 2x |
| Bare-metal vs VM | Scheduling latency within 5x |
| Observed CPU util vs computed | `/proc/stat` vs gen_config.py derivation |
| Observed wake frequency vs configured | Measured Hz vs 500 Hz target |

**Discrepancies >5x** between modes require investigation before publication.
Source: `ai_docs/irq_avoidance_plan.md` §7 "Cross-checking".

### 5.6 Absolute thresholds preferred

Use absolute thresholds, not relative comparisons:
- **Good:** "P99 scheduling latency < 100 us"
- **Bad:** "20% better than baseline" (baseline may shift between experiments)

### 5.7 Softirq consistency check

NET_RX softirq delta must be within 2x across all scheduler runs in the
same experiment. If >2x, the comparison is confounded and must be noted.
Source: `experiments/METRICS_SPECIFICATION.md` §"Validity Checks" item 1.

---

## 6. Data Provenance Rules

**Every number in a table MUST cite its source file path and computation
method.** No "magic numbers" — every value must be traceable.

### 6.1 Required provenance metadata

Every experiment directory MUST contain `provenance.json` with:

| Field | Example | Source |
|-------|---------|--------|
| Git commit | `4b15871` | `git rev-parse HEAD` |
| Git branch | `centralize-dispatch` | `git branch --show-current` |
| Dirty state | `false` | `git diff --quiet` |
| Kernel version | `6.12.0-rc5+` | `uname -r` |
| Hostname | `devbig079` | `hostname` |
| CPU model | `AMD EPYC 9D85` | `/proc/cpuinfo` |
| Binary mtimes | `2026-04-16T14:30:00` | `stat` on scheduler binaries |
| Experiment parameters | full JSON | Copy of gen_config.py args |

Source: `ai_docs/irq_avoidance_plan.md` §7 "Provenance".

### 6.2 Table annotation format

When presenting results in reports, every numeric cell MUST include a
citation. Format:

```
P99 sched latency: 42,000 ns
  Source: experiments/0.10/data/rtapp_pinned/combined_results.csv
  Filter: mode=rtapp_pinned, scheduler=LAVD_IRQ, condition=level2_nice_hints,
          metric_name=sched_latency, percentile=p99
  Aggregation: median of 3 reps (rep 2 selected as median)
  N per rep: 11,045 samples
```

### 6.3 Rep aggregation method

When aggregating across repetitions, use **median rep selection**:
sort reps by E2E P99, pick the middle one. This is more robust than
mean-of-percentiles (which can be dominated by outlier reps).

Source: `scripts/results_table.py` (median rep aggregation logic).

### 6.4 CSV schema

All experiment data MUST use the v2.0 CSV schema (14 columns):

```
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
```

Source: `experiments/METRICS_SPECIFICATION.md` v2.0 §"Common CSV Schema".

---

## 7. Core Separation and IRQ Generation

### 7.1 Bare-metal CPU layout (reference host: AMD EPYC 9D85)

| CPU Set | Cores | L3 Group | Role |
|---------|-------|----------|------|
| Workload | 0–11 | L3_0 | cache_worker, ssd_reader, ssd_writer, background_hog |
| IRQ generators | 16–19 | L3_4 | UDP loopback packet generators (SCHED_FIFO) |

Source: `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §2.1.

**Verification:**
```bash
lscpu -e=CPU,CACHE | awk 'NR==1 || ($1<=11 || ($1>=16 && $1<=19))'
```

### 7.2 IRQ generation method

All real-system modes (bare-metal and VM) use **rt-app-rs UDP loopback** for
softirq generation. This produces NET_RX softirqs on target CPUs via RPS
(Receive Packet Steering).

| Parameter | Value | Source |
|-----------|-------|--------|
| Generator count | 6 (one per IRQ CPU) | `gen_config.py` line 74 |
| Generator run/sleep | 5000us / 5000us (50% duty) | `gen_config.py` lines 56–57 |
| Packet size | 64 bytes | `gen_config.py` line 59: `softirq_packet_size` |
| Target CPUs | Even-numbered (0,2,4,6,8,10) | `gen_config.py` line 86 |
| Scheduling policy | SCHED_FIFO | `gen_config.py` task definition |

The simulator uses **synthetic periodic IRQ** events instead of a real
network stack (acceptable gap — documented in §8).

### 7.3 RPS verification

Before trusting IRQ data, verify that >90% of NET_RX softirqs land on
target CPUs:

```bash
# Before run: snapshot /proc/softirqs
# After run:  diff NET_RX column
# Target CPUs should show >90% of total NET_RX delta
```

Source: `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §2.2.

---

## 8. Known Acceptable Gaps

These are inherent to the reproducer approach and do NOT block experiments.
They MUST be documented in each experiment's README.

| Gap | Why Acceptable | Source |
|-----|---------------|--------|
| Production uses ~250 cores; reproducers use 12–16 | Schedulers scale ~linearly; ratios should hold | `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §7 |
| rtapp_sim has no real network stack | Synthetic IRQ captures the scheduling effect | ibid. |
| rtapp_sim IRQ duty cycle (10%) vs real-system (50%) | Known calibration gap, filed as `fix-sim-irq-calibration` | `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` §1.4 |
| rtapp_sim IRQ targets CPUs 0–4 vs real-system even CPUs | Known pattern mismatch, filed for fix | ibid. |
| Production traces are observation, not controlled | Different methodology; comparison is directional only | ibid. |
| PureRust has simple run/sleep; production has wake chains | PureRust is a calibrated approximation | ibid. |
| rtapp_sim x EEVDF is impossible | Simulator only implements LAVD; use bare-metal for EEVDF | This document §2 |

### Anti-patterns (blockers)

| Gap | Why It's a Blocker |
|-----|-------------------|
| Thread count mismatch between modes | Modes MUST match for valid comparison |
| Timing scale mismatch (e.g., 10x) | Modes MUST use production timing |
| One mode has background hogs, another doesn't | Modes MUST match CPU contention |
| Different IRQ generation methods | Modes MUST use same mechanism |
| Missing warmup exclusion | Early transients dominate tail percentiles |
| Reporting rtapp_sim × EEVDF data | Invalid by construction — sim cannot model EEVDF |

---

## 9. Experiment Lifecycle

### Pre-experiment checklist

Copy into each experiment's README and check off before running:

```
### Pre-experiment checklist

- [ ] Thread counts: 8 workers, 1 reader, 1 writer, 56 hogs, 6 irq_gen (72 total)
- [ ] Timing: worker run=500us sleep=1500us (v0.9 production scale)
- [ ] Compute mode: "runtime" clockonly (NOT legacy "run" with calibration)
- [ ] IRQ method: rt-app-rs UDP loopback in all real-system modes
- [ ] Core separation: workload 0-11 (L3_0), generators 16-19 (L3_4)
- [ ] RPS verified: NET_RX on even CPUs (>90% concentration)
- [ ] Background hogs: 56 threads, run=130us, sleep=950us, nice=+10
- [ ] Warmup: excluded (1s minimum, 5s recommended)
- [ ] CSV schema: v2.0 (14 columns, mode column, rep column)
- [ ] Rep column: actual rep number, not hardcoded "1"
- [ ] Builds: cargo build --release for all Rust binaries
- [ ] Scheduler binaries verified: strings | grep for expected features
- [ ] Provenance.json written before first run
- [ ] Experiment lock acquired (if producing kept data)
- [ ] Randomized run order enabled
- [ ] N ≥ 3 reps per cell (flag N=1 as limitation if unavoidable)
```

### Post-experiment validation

```
- [ ] Softirq consistency: NET_RX delta within 2x across scheduler runs
- [ ] Sample counts adequate: N ≥ 100 for percentiles, N ≥ 1000 for P99.9
- [ ] Duration matched: all runs within ±5%
- [ ] Cross-check: ≥ 2 modes agree within 5x on primary metrics
- [ ] No rtapp_sim × EEVDF data present
- [ ] All numeric claims cite source file path and computation method
```

---

## 10. Reference: Production Baselines

These are the production values we calibrate against.

| Metric | Production Value | Source |
|--------|-----------------|--------|
| CPU utilization | 88.5% | `ai_docs/irq_avoidance_plan.md` §4 "Calibration (4/4 targets passing)" |
| Wake frequency | ~500 Hz per worker | `ai_docs/irq_avoidance_plan.md` §4; derived from 2ms period |
| Tasks per CPU | ~6.0 | `ai_docs/irq_avoidance_plan.md` §4 |
| LAVD P99 sched latency advantage | ~27x vs EEVDF | `ai_docs/irq_avoidance_plan.md` §5 "Key Findings" |
| LAVD cache_worker CPU spread | Concentrated on quieter CPUs | `ai_docs/irq_avoidance_plan.md` §5 |

---

## 11. File Reference

| File | Purpose |
|------|---------|
| `scripts/gen_config.py` | Source of truth for workload parameters |
| `experiments/METRICS_SPECIFICATION.md` | Authoritative metrics schema v2.0 |
| `experiments/PRE_EXPERIMENT_REQUIREMENTS.md` | Pre-experiment checklist and lessons learned |
| `ai_docs/irq_avoidance_plan.md` | Project plan, calibration status, key findings |
| `ai_docs/repromagic_plan.md` | ReproMagic (`repm`) tool plan, subcommands, workspace |
| `scripts/run_experiment.py` | Experiment orchestrator (randomization, matrix) |
| `scripts/modes/rtapp.py` | rt-app mode: latency parsing, IRQ exposure |
| `scripts/modes/purerust.py` | PureRust mode: ucache_purerust runner |
| `scripts/modes/vm.py` | VM mode: virtme-ng integration |
| `scripts/results_table.py` | Median-rep aggregation for results tables |
| `scripts/calibration_compare.py` | Cross-mode calibration gap analysis |
| `sched-test1/scx-sim/scripts/modes/simulator.sh` | Simulator experiment runner |
