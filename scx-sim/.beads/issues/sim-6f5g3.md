---
title: 'scxsim lavd: set_power_profile is replaced by wrapper setters (lavd_set_power_mode, lavd_set_autopilot, lavd_setup_multi_domain) that skip its early return, time accounting and cpumask rebuild, and leave power states production never holds'
status: open
priority: 1
issue_type: bug
labels:
- no-stub
- lavd
depends_on:
  sim-91a825: related
created_at: 2026-09-25T05:18:55.278806109+00:00
updated_at: 2026-09-25T05:21:26.475546213+00:00
---

# Description

DEFECT

lavd changes power profile through a syscall program, `set_power_profile`. Production runs it on every start. The simulator never runs it. Instead, `schedulers/lavd/wrapper.c` `lavd_set_power_mode()` writes the state directly, and so does `lavd_set_autopilot()` when it switches to balanced; both are called over FFI from `crates/scx_simulator/src/unsafe_impl/ffi.rs`. Their comments say they duplicate lavd's userspace logic, and there is no DANGER TODO. `lavd_setup_multi_domain()` writes `no_core_compaction` directly as well.

PRODUCTION (scx 413031d44)

- `power.bpf.c` `set_power_profile` runs `do_set_power_profile`.
- `main.rs` `run()` runs the program through test_run at startup: performance with `--performance`, powersave with `--powersave`, otherwise balanced. It runs it again whenever `--autopower` changes profile.
- Until that first call, `power_mode` is 0 (performance) and `no_core_compaction` is what the load path wrote (`bss_data.no_core_compaction = opts.no_core_compaction`). Userspace writes `no_core_compaction` nowhere else.
- `do_set_power_profile`:
  - returns early when the mode is unchanged;
  - otherwise charges the elapsed time to the old mode (`update_power_mode_time`);
  - sets `power_mode`, `no_core_compaction` and `is_powersave_mode`: performance turns compaction off, balanced and powersave turn it on;
  - on a switch into performance, sets `reinit_cpumask_for_performance`, so that `update_sys_stat` rebuilds the active and overflow cpumasks. A switch into balanced or powersave clears it.
- Scheduling decisions read `no_core_compaction` and `is_powersave_mode`. Only `update_power_mode_time` and the early return read `power_mode`.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

- The wrapper writes `power_mode`, `no_core_compaction` and `is_powersave_mode` directly.
- There is no early return and no `update_power_mode_time` call, and `reinit_cpumask_for_performance` is neither set nor cleared.
- `lavd_setup_multi_domain()`, which the `DynamicScheduler::lavd_multi_domain` constructor calls, writes `no_core_compaction = false`: "Enable core compaction so do_core_compaction() runs during update_sys_stat() and keeps nr_active_cpdoms up to date." It leaves `power_mode` at performance, where `lavd_setup` put it.
- `do_set_power_profile` itself does run, and is COVERED, through `do_autopilot`. So the real logic is in the binary; the wrapper just doesn't call it.

CONSEQUENCE

- Tests that call only `lavd_set_power_mode` or `lavd_set_autopilot` do so once, before the run, on a fresh scheduler that `lavd_setup` left in performance. They get the globals `do_set_power_profile` would have produced (LATENT). But the per-mode time counters that userspace statistics read miss the time before the first `update_sys_stat`.
- Two setup paths leave states production never holds once `run()` has called `set_power_profile`:
  - 17 tests build a multi-domain lavd and set no mode, for example `test_lavd_multi_domain_balance` in `tests/lavd.rs`. They run the whole simulation in performance with compaction on. Their scheduling decisions are balanced mode's, but the time counters charge the run to performance. If autopilot were also on and chose performance, the early return would skip the switch and leave compaction on.
  - `test_lavd_multi_domain_no_compact_balanced` selects balanced and then turns compaction off with `lavd_set_no_core_compaction(true)`. In production, balanced always has compaction on.
- A switch into performance from another mode would skip the cpumask rebuild.
- This breaks the No-Stub rule by reimplementing an interface, on a program that runs on every production start.
- The coverage classification counts `set_power_profile` as UNCOVERED (entry not delivered), although its logic is replaced. So this violation sits outside the STUBBED count.

FIX DIRECTION

- The real fix is to run `set_power_profile` as a syscall program at the start of the run, where `run()` calls it. sim-91a825 (the syscall-program injection API) blocks that.
- Until then, have `lavd_set_power_mode` and `lavd_set_autopilot` record the requested mode, and apply it with `do_set_power_profile()` once the run has started, after `ops.init`. That keeps the early return, the time accounting, the cpumask rebuild and production's order. The function is in the same translation unit. It calls `scx_bpf_now()`, so it can run only while a simulator context is installed.
- `lavd_setup` and `lavd_set_no_core_compaction` stand in for the load path's writes, so they must take effect before the mode is applied, as they do in production.
- Drop the `no_core_compaction` write from `lavd_setup_multi_domain`. Tests that want compaction select a mode that has it.

ACCEPTANCE

- `lavd_set_power_mode`, `lavd_set_autopilot` and `lavd_setup_multi_domain` no longer write `power_mode`, `no_core_compaction` or `is_powersave_mode`.
- A test that selects powersave sees `powersave_mode_ns` count from the start of the run. A test that selects performance on a fresh scheduler changes nothing (the early return).
- The multi-domain tests that need compaction select balanced or powersave.
- Once sim-91a825 lands, a test switches balanced → performance mid-run through `set_power_profile`. It asserts that the active and overflow cpumasks are rebuilt at the next `update_sys_stat`.
- Until this is fixed, the wrapper functions carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row E02 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
