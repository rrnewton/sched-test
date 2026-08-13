---
title: 'calibration: add a scheduling-delay metric, and pre-register its tolerance BEFORE looking at the numbers'
status: closed
priority: 2
issue_type: task
created_at: 2026-08-12T23:57:15.784197719+00:00
updated_at: 2026-08-13T00:52:18.323159320+00:00
closed_at: 2026-08-13T00:52:18.323159210+00:00
---

# Description

Follow-up to the off_cpu_time investigation. `Metric::OffCpuTime` is now recorded as not-comparable: both backends compute `(wall - cpu)/wall` correctly, but the guest's version is dominated by virtualization overhead the simulator has no concept of.

Measured on `sched_basic_proportional`:

    cg_0   off_cpu 43.18 ms   run_delay 3.69 ms    91% is not runqueue waiting
    cg_1   off_cpu 52.14 ms   run_delay 8.68 ms    83% is not runqueue waiting

## What is missing

There is no metric for the thing off_cpu_time was *meant* to test: **scheduling delay**, i.e. runnable-but-not-running. Both sides can produce it:

- live: schedstat `run_delay`, already in the sidecar as `mean_run_delay_us` / `worst_run_delay_us` with a `run_delay_measured` flag
- simulator: time between a task becoming runnable (enqueue / DSQ insert) and its next `TaskScheduled`. Not currently derived by `scxsim-calibration::sim`; the trace has the events.

That comparison would be a real fidelity check, and unlike off_cpu_time it would be comparing the same physical quantity.

## Why this is filed instead of implemented

**I have already seen both numbers** — sim off-CPU 6.0 ms against the guest's 3.69 / 8.68 ms of run_delay. Choosing a tolerance now would be choosing it with knowledge of the result, which is exactly what pre-registration in `Metric::spec()` exists to prevent. Adding a metric that passes because its bound was picked to fit is worse than not adding it: it would look like independent corroboration.

Whoever picks this up should set the tolerance from what a scheduling-delay comparison ought to bear — argued from the quantity, not from these two runs — and only then compute it.

## Also worth noting when it is built

The simulator has no interference sources at all: no IRQs, no timer ticks, no competing guest work, no host. Its scheduling delay is therefore a floor, not an estimate. That is fine for comparing scheduler *policies* and misleading for anything about jitter or tail latency, and the metric's docs should say so rather than leaving a reader to infer it.
