---
title: 'cosmos: task stranded on offlined CPU (not migrated) under scxsim'
status: open
priority: 2
issue_type: bug
created_at: 2026-07-23T04:36:42.186630218+00:00
updated_at: 2026-07-23T04:36:42.186630218+00:00
---

# Description

Observed while writing CPU hotplug tests (tests/cpu_hotplug.rs). Scenario: 2 CPUs, 2 forever-running hogs (one per CPU), cpu_offline_at(CpuId(1), 30ms). Under simple/lavd/mitosis the hog that was on CPU 1 migrates to CPU 0 and both tasks keep running (rt roughly balanced). Under scx_cosmos the task that was running on CPU 1 (pid 1) is NOT migrated: its last TaskScheduled is at ~30.0ms on CPU 1, its runtime freezes at ~40ms, and it never runs again for the remaining ~56ms of the run while remaining runnable (rt1=40ms vs rt2=80ms). Simulation still exits Normal (watchdog default 30s does not fire in a 100ms run). In a real kernel, offlining a CPU migrates its runnable tasks off it, so a runnable task frozen for 56ms is a fidelity concern. Needs investigation: is handle_cpu_offline's drain/re-enqueue interacting with cosmos per-CPU/NUMA-domain DSQ placement such that the re-enqueued task lands back on the offline CPU's domain and is never re-dispatched? tests/cpu_hotplug.rs excludes cosmos from the per-task migration assertion pending this issue and documents the exclusion.
