---
name: scxsim-usage
description: Run deterministic scheduling simulations with scx-sim
---

# scx-sim Usage

## What it does

scx-sim is a deterministic simulator that models sched_ext scheduling
algorithms. It runs rt-app JSON workload specs and produces reproducible
scheduling traces.

## Critical constraint

**scx-sim only simulates sched_ext schedulers.** It cannot simulate EEVDF
or CFS. The combination `rtapp_sim x EEVDF` is IMPOSSIBLE. Never produce
or accept data with that label.

## Running simulations

```bash
cd scx-sim/

# Run with default config
cargo run --release -- --config ../configs/workload.json

# Run with specific scheduler variant
cargo run --release -- --config ../configs/workload.json --scheduler lavd_baseline
cargo run --release -- --config ../configs/workload.json --scheduler lavd_irq

# Run via repm
repm run --mode rtapp-sim --schedulers lavd_baseline --reps 3
```

## Output

- CSV metrics per METRICS_SPECIFICATION.md
- Deterministic: same config + same scheduler = identical output
- Useful for comparing scheduler variants without hardware noise

## Building

```bash
cd scx-sim/
cargo build --release
./validate.sh  # Must pass before trusting results
```

## When to use simulator vs bare-metal

| Question | Use simulator | Use bare-metal |
|----------|--------------|----------------|
| "Does the scheduling logic work?" | Yes | Yes |
| "How big is the effect vs EEVDF?" | No | Yes |
| "Is the result reproducible?" | Yes (deterministic) | Yes (statistical, N>=3) |
| "What's the absolute latency?" | No (simulated time) | Yes |
