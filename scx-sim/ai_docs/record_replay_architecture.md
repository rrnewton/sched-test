# Record/Replay Architecture Deep Dive

*Date: 2026-03-15*

## Overview

The preemptive record/replay system allows recording preemption points during a
simulation run (with PMU) and replaying them deterministically (with hardware
breakpoints or, potentially, e9patch). This document analyzes the current
architecture and evaluates the feasibility of mixing recording and replay modes.

## 1. What a PreemptionRecord Contains

Source: `crates/scx_simulator/src/unsafe_impl/preempt/mod.rs:79-116`

| Field | Type | Description |
|-------|------|-------------|
| `rbc_count` | `u64` | Raw PMU timeslice count for this preemption event |
| `instruction_pointer` | `u64` | RIP at the preemption point (from ucontext) |
| `structop_rbc` | `u64` | **Cumulative** per-worker RBC across the entire dispatch round |
| `cpu_id` | `CpuId` | Which CPU the worker was running on |
| `worker_id` | `WorkerId` | Which worker thread was preempted |
| `sequence` | `u64` | Global monotonic sequence number |
| `structop_local` | `u64` | Per-CPU structop counter (1-based) |
| `structop_global` | `u64` | Global structop counter (1-based) |
| `ops_context` | `OpsContext` | Which ops callback was active (select_cpu, dispatch, etc.) |
| `kfunc_name` | `&'static str` | Name of the kfunc being called (empty for PMU preemptions) |
| `kfunc_count` | `u32` | Number of kfuncs within the current structop |
| `insn_bytes` | `[u8; 5]` | First 5 bytes of instruction at RIP (for .so version check) |

The trace file additionally stores `rip_offset` (= `instruction_pointer - so_base`) for
ASLR resilience. On deserialization with a different so_base, the absolute RIP is
reconstructed as `new_so_base + rip_offset`.

### Key distinction: `rbc_count` vs `structop_rbc`

- `rbc_count`: The raw PMU timeslice that fired this preemption (how many branches
  since the last timer arm). This is the RELATIVE count.
- `structop_rbc`: The CUMULATIVE total of all `rbc_count` values for this worker
  in this dispatch round. Replay uses this as the ABSOLUTE coordinate to find the
  right preemption point. The counter is NEVER reset at structop/kfunc boundaries
  in replay mode.

## 2. How Replay Works: Two Modes

### Mode 1: PMU + HW Breakpoint (Default)

Source: `crates/scx_simulator/src/unsafe_impl/backend/replay.rs`

**Architecture:** Two-signal approach using SIGSTKFLT (PMU timer) and SIGTRAP
(hardware breakpoint).

**Flow:**
1. `ReplayBackend::build_target()` reads the next target from the `ReplayCursor`,
   returns a `PreemptTarget` with `RbcTarget::Absolute(structop_rbc)` and
   `target_rip = Some(instruction_pointer)`.
2. `ReplayBackend::arm()` calls `arm_replay_timer_pub(timer_fd, structop_rbc)`,
   which sets the PMU timer to fire at `structop_rbc - REPLAY_MARGIN(200)` branches.
3. When SIGSTKFLT fires (`replay_pmu_handler`):
   - Disables the PMU timer
   - Checks for overshoot (current_rbc > target.structop_rbc)
   - Arms the hardware breakpoint at `target.instruction_pointer`
   - Re-enables the PMU counter (not the signal, just counting)
4. When SIGTRAP fires (`replay_bp_handler`):
   - Validates structop context and instruction bytes
   - Yields the token (preemption point)
   - Advances the cursor
   - Arms the timer for the next target

**Nondeterminism:** PMU skid means the timer fires at approximately, not exactly,
the right branch count. If it overshoots past the target, `REPLAY_OVERSHOT` is set
and the entire dispatch round is retried.

### Mode 2: Breakpoint-Only (`--no-pmu-signal`)

**Architecture:** Single-signal approach using only SIGTRAP.

**Flow:**
1. `ReplayBackend::build_target()` returns `RbcTarget::Relative(0)` with
   `target_rip = Some(instruction_pointer)`.
2. `ReplayBackend::arm()` calls `arm_replay_breakpoint_pub(bp_fd, rip)`, arming
   the hardware breakpoint directly.
3. When SIGTRAP fires:
   - Reads the current RBC count via `read_rbc_count(timer_fd)`
   - If `current_rbc < target.structop_rbc`: re-arms the breakpoint and returns
     (wrong dynamic instance of the instruction)
   - If `current_rbc >= target.structop_rbc`: this is the right instance; yields
     the token, advances cursor, arms breakpoint for next target.

**Key:** Still needs a PMU COUNTER (timer_fd) for `read_rbc_count()` -- it just
doesn't use the PMU SIGNAL. The breakpoint fires on EVERY execution of the
target instruction, and the RBC count disambiguates which dynamic instance.

**Determinism:** Fully deterministic. No PMU skid involved because the counter
is read passively, not used to trigger a signal.

### Retry Strategy

Source: `crates/scx_simulator/src/unsafe_impl/backend/mod.rs:752-841`

`replay_dispatch_with_retry()` implements a two-tier retry:
1. **Tier 1:** Up to 3 attempts with PMU signal approach
2. **Tier 2:** Up to 2 attempts with breakpoint-only fallback (`with_bp_only()`)

## 3. Can We Record with PMU and Replay with Breakpoints?

**Answer: YES -- this is ALREADY implemented and working.**

The recording phase uses `PmuBackend` which fires random PMU timeslices. The
`preempt_handler` captures `instruction_pointer` (from ucontext) and `rbc_count`
(from reading the PMU counter). These are stored in a `PreemptionRecordStore`
and serialized to a trace file.

The replay phase uses `ReplayBackend` which reads the trace file and uses the
recorded `structop_rbc` (cumulative) and `instruction_pointer` to target the
exact same preemption points. The PMU is only used as a "proximity detector"
(Mode 1) or a passive counter (Mode 2, breakpoint-only).

### Why this works

1. **Instruction pointer** is deterministic -- the same code path produces the
   same instructions at the same addresses (modulo ASLR, handled by rip_offset).
2. **RBC count** is deterministic -- same code path = same retired conditional
   branches. PMU skid only affects WHEN the signal fires, not what the counter
   reads.
3. The combination (IP + cumulative RBC) uniquely identifies each dynamic
   execution of a preemption-target instruction.

### The PRNG synchronization requirement

Replay must consume PRNG values in the same sequence as recording. This is why:
- `ReplayBackend::build_target()` calls `ring.roll_timeslice()` (discards result)
- `rearm_timer()` in replay mode calls `ring.roll_timeslice()` (discards result)
- `install_replay_preempt()` sets `timeslice_min/max` to match the recording

Without matching PRNG consumption, `pick_next()` returns different worker IDs
and replay diverges.

## 4. Can We Record with PMU and Replay with E9patch?

**Answer: Not yet implemented, but architecturally feasible and attractive.**

### Current e9patch backend (recording only)

Source: `crates/scx_simulator/src/unsafe_impl/backend/e9patch.rs`

The `E9PatchBackend` uses software branch counting via e9patch-instrumented `.so`
files. The trampoline (`e9_rbc_trampoline.c`) decrements a shared counter at every
Jcc and calls `e9_preempt_yield()` when it expires. **No PMU hardware needed.**

Currently used only for recording-style runs with random PRNG timeslices.

### Design for E9PatchReplayBackend

An `E9PatchReplayBackend` would combine the trace from PMU recording with
e9patch's deterministic branch counting:

```
struct E9PatchReplayBackend {
    cursors: Vec<ReplayCursor>,    // from trace file
    timeslice_min: u64,            // for PRNG sync
    timeslice_max: u64,
    fns: E9PatchFns,               // arm/disarm function pointers
    accumulated_rbc: Cell<u64>,    // per-worker cumulative RBC
}
```

**build_target():**
```
let target = cursor.current_target()?;
let delta = target.structop_rbc - self.accumulated_rbc.get();
PreemptTarget {
    count_rbc: RbcTarget::Relative(RelativeRbc(delta)),
    target_rip: Some(target.instruction_pointer),
}
```

**arm():**
```
(self.fns.arm)(delta);  // set e9 counter to fire after `delta` branches
```

**yield handler (e9_preempt_yield):**
```
// Counter expired -- verify we're at the right instruction
// The call site's return address should match target.instruction_pointer
// (or use current_rbc tracking to verify)
cursor.advance();
accumulated_rbc += delta;
// return new counter for next target
```

### Advantages of e9patch replay

1. **No PMU hardware needed** -- works in VMs, containers, CI
2. **Fully deterministic** -- software counting, no skid
3. **No retry logic needed** -- never overshoots
4. **Single mechanism** -- no two-signal coordination
5. **Portable** -- only requires x86-64 with writable code pages

### Challenges

1. **Instruction verification**: The e9 trampoline fires at a COUNTER value, not
   at a specific IP. Need to verify the yield happens at the correct instruction.
   This could be done by checking the return address in `e9_preempt_yield()`.

2. **Counter granularity**: E9patch counts ALL conditional branches in the .so,
   while PMU counts retired conditional branches system-wide (excluding kernel).
   The counts may not be 1:1 identical if the PMU event includes branches from
   non-Jcc instructions (e.g., LOOP, CMOV with branch prediction). Need to
   validate that the counter values are compatible.

3. **Cooperative yield interaction**: The e9 counter runs during kfunc boundary
   code. In replay, the counter must be paused/resumed at the same points as
   during recording. The current `rearm_timer` mechanism handles this for PMU;
   a similar pause/resume would be needed for e9patch.

4. **PRNG synchronization**: Same requirement as breakpoint replay -- must match
   PRNG consumption sequence.

## 5. Summary of Recording and Replay Modes

| Recording Mode | Replay Mode | Status | Deterministic? | PMU Required? |
|---------------|-------------|--------|----------------|---------------|
| PMU | PMU + HW breakpoint | Implemented | No (skid) | Yes (counter + signal) |
| PMU | HW breakpoint only | Implemented | Yes | Yes (counter only) |
| PMU | E9patch | **Not implemented** | Yes | No |
| E9patch | HW breakpoint only | Possible | Yes | Yes (counter only) |
| E9patch | E9patch | Possible | Yes | No |

## 6. The Bug (sim-603bd5)

The REPLAY MISMATCH ops context divergence (trace=none, replay=select_cpu) is
NOT about the preemption mechanism. It is about the replay failing to reproduce
the exact execution path. Possible causes:

1. **PRNG sequence divergence**: A cooperative yield point was added or removed
   between recording and replay, causing PRNG consumption to differ.
2. **Missed preemption point**: If a preemption is missed (e.g., breakpoint
   doesn't fire), a different worker runs, all subsequent scheduling decisions
   diverge.
3. **Structop boundary mismatch**: The ops_context tracking may have a race
   where another worker's exit_sim clears the context before the preempted
   worker reads it.

The `current_ops_context()` function reads from TLS (set by `set_ops_context()`
when entering a callback), which should be race-free. But if the preemption
fires at a boundary between two ops callbacks, the TLS value may not match the
recording.
