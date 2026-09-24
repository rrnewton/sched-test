---
title: 'scxsim CLI: ASLR re-exec swallows child signal deaths (a SIGSEGV is reported as a silent exit 1)'
status: open
priority: 1
issue_type: bug
labels:
- safety
- scxsim
depends_on:
  sim-g59c6: related
created_at: 2026-09-24T21:29:44.394082954+00:00
updated_at: 2026-09-24T21:29:44.394082954+00:00
---

# Description

DEFECT (No Silent Failures): when the scxsim child process is killed by a signal, the CLI's ASLR re-exec wrapper reports it as a plain `exit 1` and prints nothing. A SIGSEGV inside a scheduler library, which is the usual way a scheduler or substrate bug shows itself under scxsim, therefore looks like an ordinary error exit with no error message.

CODE: `reexec_with_aslr_disabled` (bin/scxsim/main.rs) runs the child with `std::process::Command::status()` and then calls `std::process::exit(status.code().unwrap_or(1))`. `ExitStatus::code()` is `None` when the child died from a signal, so the signal number is dropped and nothing is printed. `ensure_aslr_disabled` takes this path by default: it prints "scxsim: disabling ASLR and re-executing..." unless `SCX_SIM_ASLR_DISABLED=1` is already set, `--no-disable-aslr` is passed, or ASLR is already off.

EVIDENCE (sched-test 8e67db95). LAVD run with `verbose = 1`, which is a real helper-ID crash (see the linked issue):
- Via the wrapper: exit status 1. Stderr holds only the re-exec line. dmesg shows `segfault at b1 ip 00000000000000b1`.
- The same child run directly (`SCX_SIM_ASLR_DISABLED=1 setarch -R scxsim run ...`): exit status 139, and bash prints "Segmentation fault", and the core is dumped.

The same swallowing applies to every fatal signal. Examples: SIGABRT from a C `abort()`; SIGILL from `__builtin_trap()`, which wrapper setup code uses; SIGBUS; and SIGKILL from the OOM killer. A Rust panic is not affected, because it prints its message and exits with a code.

PRE-EXISTING, not caused by the 2026-09-24 scx pin bump. Found while reproducing the helper-ID crash.

FIX DIRECTION: when `status.code()` is `None`, read `std::os::unix::process::ExitStatusExt::signal()` / `core_dumped()`. Print a clear line such as `scxsim: child killed by signal 11 (SIGSEGV), core dumped`. Then terminate the parent the same way, so that callers and shells see the real cause: restore the default disposition and re-raise the signal, or exit with 128 + signo.

ACCEPTANCE: a test that runs a child killed by a signal through the re-exec path shows a non-1 status that encodes the signal, plus a stderr line naming the signal. For example, use a scheduler config known to fault, or a test-only hook that raises SIGSEGV.
