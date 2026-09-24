---
title: 'lavd: verbose>=1 crashes at PC 0xb1 because the bpf_printk override sits after LAVD''s own sources (helper-ID pointer call)'
status: open
priority: 2
issue_type: bug
labels:
- kernel-fidelity
- lavd
- substrate-gap
depends_on:
  sim-vkpyu: related
created_at: 2026-09-24T21:29:44.268803471+00:00
updated_at: 2026-09-24T21:29:53.229740592+00:00
---

# Description

DEFECT: setting LAVD's `verbose` rodata to 1 or more through the documented `scxsim run --config` path crashes the simulator inside `lavd_init`, before any task runs. The crash jumps to PC 0xb1, which is BPF helper ID 177 (`bpf_trace_vprintk`). Under the normal ASLR re-exec the crash leaves no message at all: the process exits with status 1 (the swallowed signal is filed separately; see the linked issue).

REPRO (sched-test 8e67db95, scx 413031d44):
```
printf '[scheduler.u8_globals]\nverbose = 1\n' > /tmp/verbose1.toml
target/debug/scxsim run -s lavd -c 2 --duration 50ms \
    --config /tmp/verbose1.toml examples/simple_two_tasks.json
## exit 1; nothing printed after "scxsim: disabling ASLR and re-executing..."
## dmesg: scxsim[...]: segfault at b1 ip 00000000000000b1 ... error 14
```
With `verbose = 0` the same command exits 0.

Backtrace, taken with lldb on the child run directly (`SCX_SIM_ASLR_DISABLED=1`):
- frame #0: 0x00000000000000b1
- frame #1: libscx_lavd.so`init_per_cpu_ctx(now=0), main.bpf.c, at the `debugln("cpu[%d] max_capacity: ...")` site
- frame #2: libscx_lavd.so`lavd_init
- frame #3: `DynamicScheduler::init` (unsafe_impl/ffi.rs)

MECHANISM:
1. libbpf's `bpf_helper_defs.h` declares every BPF helper as `static <ret> (* const name)(...) = (void *) <helper id>`. In scxsim's userspace build that is an ordinary function pointer, and its value is the helper ID. A helper that no wrapper overrides compiles and links cleanly, then jumps to address == helper ID at runtime. `-Werror=implicit-function-declaration` cannot catch this, because the symbol is declared.
2. LAVD's `debugln` / `traceln` (lavd.bpf.h) call `bpf_printk` when `verbose > 0` / `verbose > 1`. libbpf's `bpf_printk` expands, by argument count, to `__bpf_printk`, which calls `bpf_trace_printk` (ID 6), or to `__bpf_vprintk`, which calls `bpf_trace_vprintk` (ID 177).
3. `schedulers/lavd/wrapper.c` does override `bpf_printk` with `dprintf(2, "[LAVD-PRINTK] " ...)`. But the `#undef` / `#define` comes AFTER `#include "main.bpf.c"` and the helper TUs (util, power, sys_stat, lock, balance, idle, lat_cri, preempt and introspec .bpf.c). It was placed to cover `cgroup_bw.bpf.c`, which is included below it. So every `debugln` / `traceln` in LAVD's own sources still calls through the helper-ID pointers.
4. The comment above that override is wrong. It says the helpers are left "undefined-weak (resolved to NULL function pointers)" and crash "with `PC ~= 0`". They are actually internal pointers initialised to the helper ID, so PC equals the ID (0x6 or 0xb1). The cosmos wrapper's comment on `bpf_task_storage_delete` describes the mechanism correctly: ID 157 lands at 0x9d.

REACH. I compiled every scx-sim C translation unit to LLVM IR on the bump branch. These are the only helper-ID pointers any of them still references:
- lavd: `@bpf_trace_printk` (6) and `@bpf_trace_vprintk` (177). They are loaded in `do_set_power_profile`, `update_effective_capacity`, `pick_idle_cpu`, `lavd_enqueue`, `lavd_dequeue`, `lavd_quiescent`, `init_per_cpu_ctx` and `get_primary_cpu`, and every one of those loads is a `debugln` / `traceln` site.
- cosmos: `@bpf_trace_printk` (6) in `scx_pmu_event_stop`, which is lib/pmu.bpf.c's "SWITCHED: %ld vs %ld" diagnostic. It cannot be reached today: `cosmos_perf_event_read_value` returns -ENOENT, and the function returns before it gets to the printk. It becomes reachable as soon as PMU counters are modelled.
- layered, mitosis, simple, tickless, and the csrc/ and scxtest/ TUs: none.

Tests are unaffected, because the scxsim-build lavd `SchedulerManifest` pins `verbose = 0`. Crashing needs the `--config` override, which is exactly what someone debugging LAVD would reach for.

PRE-EXISTING, not caused by the 2026-09-24 scx pin bump: the override was already in this position before the bump. I found it during the bump's helper-ID sweep. Commit cf1679b6 (`bpf_get_func_ret`) promised to file it separately; this is that issue. Prior intent: sim-904c9 ("bpf_printk stub: map to eprintln ... used in lavd.bpf.h ... as debug output wrapper"), which the current placement does not deliver for LAVD's own sources.

FIX DIRECTION:
- Move the `bpf_printk` override above the first scheduler-source include, so that LAVD's own diagnostics reach `[LAVD-PRINTK]` on stderr. Do the same for cosmos before `pmu.bpf.c`. Alternatively, model `bpf_trace_printk` / `bpf_trace_vprintk` once, faithfully to the kernel (format into the trace stream), in the shared substrate instead of per-wrapper macros.
- Correct the wrong comment.
- Add a build-time gate so that this class cannot come back silently. After each scheduler library is compiled, fail the build if its code still references a `bpf_helper_defs.h` helper pointer that no wrapper overrides. In IR these show up as an internal global initialised with `inttoptr (i64 <id> to ptr)`.

ACCEPTANCE:
- The repro above exits 0 and prints `[LAVD-PRINTK]` lines on stderr.
- A regression test runs LAVD with `verbose = 2` (so that `traceln` is reached too) through the rodata-override path.
- The build-time gate fails when a helper-ID pointer is reintroduced. Demonstrate it failing on a deliberately un-overridden helper.
