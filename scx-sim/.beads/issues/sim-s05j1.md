---
title: 'scxsim lavd: scx_bpf_error() in cgroup_bw prints and continues where production ejects the scheduler'
status: open
priority: 1
issue_type: bug
labels:
- no-stub
- lavd
created_at: 2026-09-25T05:18:55.275770856+00:00
updated_at: 2026-09-25T05:21:26.473167782+00:00
---

# Description

DEFECT

Inside its cgroup_bw region, `schedulers/lavd/wrapper.c` does `#undef scx_bpf_error` and then defines it as `dprintf(2, "scx_bpf_error: " fmt "\n", ...)`, under the comment "scx_bpf_error -> stderr (don't abort sim)".

The wrapper's header block describes an older version of the same override: "`scx_bpf_error` -> `bpf_printk` so library-internal "BUG:" messages surface in the printk pipeline rather than aborting the simulator". The code no longer uses `bpf_printk`.

PRODUCTION (scx 413031d44)

`scx_bpf_error()` is the `scx/scheds/include/scx/common.bpf.h` macro. It formats the message with `scx_bpf_bstr_preamble`, prefixing the file and line, and calls the `scx_bpf_error_bstr` kfunc. The kernel then disables the BPF scheduler with an error exit (SCX_EXIT_ERROR_BPF). The error is fatal to the scheduler.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

The message is printed and execution continues past the error, into code that production never runs after that point.

CONSEQUENCE

A condition that ejects the production scheduler lets the simulated one carry on (LATENT). A run that should end with an error exit instead reports whatever the scheduler does next.

Two call sites are new at this pin:
- `cbw_build_spare`: the function is COVERED, but its error branch is not taken.
- `scx_cgroup_bw_kick_idle_cb`: UNCOVERED.

The coverage profile shows neither branch taken. The re-measure's full test output, stdout and stderr together, contains no "scx_bpf_error:" line, which is what the override prints.

This breaks the No-Stub rule, because it is a silent fallback.

FIX DIRECTION

- Delete the override, so that cgroup_bw's `scx_bpf_error` reaches the simulator's own `scx_bpf_error_bstr` (`crates/scx_simulator/src/unsafe_impl/kfuncs.rs`). That records the message in `bpf_error`, and the engine's `check_bpf_error` ends the run with `ExitKind::ErrorBpf`. The rest of lavd already takes that path.
- Correct the header block's description of the override, or delete it with the override.
- If a test needs to survive an error, it should expect the exit, not suppress it.

ACCEPTANCE

- A test drives a cgroup_bw error branch, for example `cbw_build_spare`'s, and asserts that the simulated lavd exits with the BPF error exit and the library's message.
- No `scx_bpf_error` redefinition remains in the lavd wrapper.
- Until this is fixed, the override carries a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row K10 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
