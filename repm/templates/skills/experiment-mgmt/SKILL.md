---
name: experiment-mgmt
description: Manage experiment versions, provenance, README, CHANGELOG
---

# Experiment Management

## Directory structure

```
experiments/
+-- METRICS_SPECIFICATION.md      # Authoritative metrics schema
+-- v001_initial/
|   +-- README.md                 # What, why, parameters
|   +-- CHANGELOG.md              # Changes since previous version
|   +-- provenance.json           # IMMUTABLE after creation
|   +-- config_snapshot.toml      # Frozen copy of config at run time
|   +-- data/
|   |   +-- rtapp_pinned/         # Per-mode subdirectories
|   |   |   +-- eevdf/            # Per-scheduler within mode
|   |   |   |   +-- rep_001.csv
|   |   |   |   +-- rep_002.csv
|   |   |   |   +-- rep_003.csv
|   |   |   +-- lavd_baseline/
|   |   +-- combined_results.csv
|   +-- reports/
|       +-- RESULTS.md
+-- v002_tuned_hogs/
    +-- ...
```

## Creating a new experiment version

1. **Increment version number**: `v001` -> `v002` -> `v003`
2. **Choose a descriptive suffix**: `v002_tuned_hogs`, `v003_48cpu`
3. **Run `repm run`**: This writes `provenance.json` automatically

```bash
# Auto-create next version
repm run --new-version tuned_hogs --purpose "Evaluate tuned bg thread timing"

# First run in workspace auto-creates v001_initial
repm run --reps 3
```

## When to create a new version

A new version is REQUIRED when ANY of these change after provenance
has been written:

- Workload config (repromagic_config.toml `[workload]` or `[topology]`)
- Scheduler binary (different build, different revision)
- Scheduler flags (different CLI arguments)
- Experiment infrastructure code (scripts, repm itself)
- Kernel version
- Host machine

## When a new version is NOT required

- Re-running the same experiment to add more reps (append to existing data/)
- Regenerating reports from existing data
- Adding notes to README.md or CHANGELOG.md

## Provenance rules

- `provenance.json` is written ONCE at experiment start
- It is NEVER modified after creation
- It records: git rev, binary stat, config hash, host, kernel, timestamp
- If you need to change anything, create a NEW experiment version

## Resume support

`repm run` automatically skips completed reps (non-empty CSV files).
To add more reps to an existing experiment:

```bash
repm run --experiment v001_initial --reps 5
# Skips already-completed reps, runs only the new ones
```

## Cross-checking protocol

Every result claim requires at least 2 independent sources:

| Claim | Source 1 | Source 2 |
|-------|----------|----------|
| "P99 latency is X" | rt-app slack column | Perfetto sched trace |
| "Scheduling change works" | Simulator A vs B | Bare-metal A vs B |
| "Effect size is Y%" | Mode 1 | Mode 2 |

## Skeptic checklist

Before accepting any result, verify:

- [ ] Provenance JSON matches what you think was tested
- [ ] Sample count (N) is sufficient (>=3 reps, >=100 samples per metric)
- [ ] Warmup period was excluded
- [ ] No mode x scheduler constraint violations
- [ ] Config hash in provenance matches current config (if claiming "same config")
- [ ] Binary mtimes are plausible (not stale)
