# Scheduler Config Sidecar (TOML)

> **Status — stub.** This page will document the `--config <PATH>`
> sidecar mechanism for setting per-symbol BPF globals without
> rebuilding the scheduler.

The sidecar is a TOML file with four sub-tables, one per supported
global type:

```toml
[bool_globals]
enable_cpu_bw = true

[u8_globals]
# example_u8 = 7

[u32_globals]
# slice_max_us = 1500

[u64_globals]
# preempt_decision_us = 200
```

Each entry maps a BPF global symbol name (declared in the loaded
scheduler `.so`) to its value at simulation start. The canonical
worked example is
[`tests/fixtures/h6/bug1_canonical.toml`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml),
which flips `enable_cpu_bw = true` on LAVD for the Bug-1 reproducer.
