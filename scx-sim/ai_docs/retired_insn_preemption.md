# Retired Instruction Preemption: Feasibility Assessment

Research into using `PERF_COUNT_HW_INSTRUCTIONS` (retired instructions) as the
PMU preemption event instead of retired conditional branches (RBC).

## Current State

The simulator already has **full infrastructure** for instruction-level
preemption. The `PmuEvent` enum in `crates/scx_perf/src/lib.rs` defines both
events:

```rust
pub enum PmuEvent {
    RetiredBranchConditional, // "rbc" — vendor-specific raw event
    InstructionsRetired,       // "insn" — PERF_TYPE_HARDWARE + PERF_COUNT_HW_INSTRUCTIONS
}
```

The CLI exposes this via `--break-on insn`:

```
scxsim --preemptive --break-on insn --timeslice-min 100 --timeslice-max 500
```

The plumbing is wired end-to-end:

1. **`PmuConfig::resolve()`** maps `InstructionsRetired` to
   `(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS)` — a generic hardware
   event, so no vendor-specific CPUID detection is needed (unlike RBC which
   uses `PERF_TYPE_RAW` with AMD 0x00D1 or Intel 0x01C4).

2. **`RbcTimer::new_event()`** and **`RbcCounter::new_event()`** accept any
   `PmuEvent`, so both the preemption timer and the measurement counter work
   with `InstructionsRetired`.

3. **`PmuBackend`** passes `break_on` through to `setup_pmu_timer()`, which
   creates the timer with the configured event.

4. **Trace serialization** stores `# break_on: insn` in the trace header and
   round-trips correctly (tested in `test_break_on_insn_roundtrip`).

5. **`measure_skid` example** already supports `--event insn` for measuring
   skid characteristics of instruction-level counters.

## What Already Works

| Component | Status | Notes |
|-----------|--------|-------|
| PMU timer creation | Working | `try_create_pmu_timer(PmuEvent::InstructionsRetired)` |
| PMU counting | Working | `RbcCounter::new_event(config, PmuEvent::InstructionsRetired)` |
| Signal delivery | Working | Same SIGSTKFLT mechanism regardless of event type |
| CLI `--break-on insn` | Working | Wired through `BreakOn::Insn` to `PmuEvent` |
| Trace file format | Working | `# break_on: insn` header, round-trip tested |
| Skid measurement | Working | `measure_skid --event insn` example |
| Replay backend (PMU+HW BP) | Working | Event-agnostic; uses the trace's `break_on` |
| Cooperative yields | Unaffected | Event type only affects PMU timer, not kfunc yields |

## Determinism Properties

**Retired instructions are deterministic** for the same reasons as RBC:

- `PERF_COUNT_HW_INSTRUCTIONS` counts only *retired* (architecturally
  committed) instructions, not speculative ones. The Intel SDM specifies this
  as "Instructions Retired" (fixed counter 0, or architectural event 0x00C0).
- AMD's equivalent is event 0xC0 (RETIRED_INSTRUCTIONS).
- Tools like `rr` and `perf stat` rely on retired instruction counts being
  exact and deterministic.
- The Linux kernel's generic `PERF_COUNT_HW_INSTRUCTIONS` maps to the correct
  underlying fixed or PMC counter on both Intel and AMD.

**Same determinism guarantee as RBC**: same code path produces the same
retired instruction count. The PRNG-driven timeslice selection remains
deterministic. PMU skid still applies (signal delivery is delayed by pipeline
depth), but skid is bounded and affects signal delivery timing, not the
underlying counter value.

## Compatibility Matrix

| Replay Backend | Compatible? | Notes |
|----------------|-------------|-------|
| **PMU + HW breakpoint** | YES | Event-agnostic. The PMU timer fires near the target count (using whatever event was recorded), then the HW breakpoint catches the exact RIP. The `ReplayBackend` stores `break_on` from the trace and creates the timer with the same event type. |
| **Breakpoint-only** (`--no-pmu-signal`) | YES | Uses the PMU counter only for `read_rbc_count()` measurement, not for signal delivery. The counter event type is stored in the trace and restored during replay. |
| **e9patch recording** | NO | e9patch instruments **conditional branch (Jcc) instructions** via `e9tool -M 'jcc'`. The `rbc_trampoline` fires at each Jcc, decrementing a software counter. This fundamentally counts *conditional branches*, not *instructions*. An e9patch trace recorded with RBC counting cannot be replayed with instruction-level counting, and vice versa. |
| **e9patch replay** | NO | Same limitation: `E9PatchReplayBackend` arms the e9 software counter with branch deltas from the trace. If the trace was recorded with instruction counts, the branch-counting trampoline cannot reproduce those counts. |

## Pros and Cons vs RBC

### Advantages of Instruction-Level Preemption

1. **Finer granularity**: Every instruction is a potential preemption point, not
   just branches. Straight-line code between branches can be preempted.

2. **More events per unit of code**: Roughly 3-10x more instructions than
   conditional branches in typical code. This means:
   - More preemption opportunities per structop
   - Better coverage of code paths that have long branch-free sequences
   - The PRNG explores a denser space of possible interleavings

3. **No vendor-specific detection**: `PERF_COUNT_HW_INSTRUCTIONS` is a generic
   hardware event that works across all x86 CPUs without CPUID-based event code
   lookup. RBC requires vendor-specific raw event codes (Intel 0x01C4 vs AMD
   0x00D1).

4. **Fixed counter availability**: On Intel, retired instructions uses fixed
   counter 0 (IA32_FIXED_CTR0), which is dedicated and always available. RBC
   uses a programmable PMC which may be contended with other profiling tools.

### Disadvantages

1. **Higher overhead**: With instruction-level counting, the PMU fires more
   frequently for the same timeslice value. Users must increase
   `timeslice_min`/`timeslice_max` to avoid excessive preemption overhead.
   The current defaults of `timeslice_min=1, timeslice_max=1` would cause
   immediate preemption on every instruction.

2. **Larger skid impact**: PMU skid is measured in instructions (not branches).
   The absolute skid in instructions may be similar to branch-level skid, but
   since the event density is higher, the *relative* skid (skid / timeslice)
   may be larger. This could make PMU-signal replay less reliable than with RBC.
   The `measure_skid --event insn` tool can quantify this.

3. **No e9patch compatibility**: The e9patch backend instruments Jcc
   instructions specifically. Instruction-level preemption would require a
   fundamentally different instrumentation approach (e.g., instrumenting every
   instruction, which e9patch does not support efficiently).

4. **Counter naming confusion**: The codebase uses "RBC" (Retired Branch
   Count) extensively in variable names, struct fields, comments, and trace
   formats. Using instruction counts would make these names misleading. The
   `rbc_count` field in `PreemptionRecord`, `structop_rbc` in trace files,
   `E9_SHARED_RBC` global symbol, etc. would all be semantically incorrect
   when counting instructions. A rename to a generic "PMU count" would be
   a significant refactoring effort.

## Required Changes (if proceeding)

### Already Done (no changes needed)

- `PmuEvent::InstructionsRetired` variant
- `PmuConfig::resolve()` for instruction events
- `RbcTimer::new_event()` / `RbcCounter::new_event()` parameterization
- `--break-on insn` CLI flag
- Trace header `# break_on: insn` serialization
- `measure_skid --event insn` skid characterization

### Recommended Timeslice Guidance

When using `--break-on insn`, timeslice values should be scaled up by the
instruction-to-branch ratio (typically 3-10x):

- RBC defaults: `--timeslice-min 1 --timeslice-max 500`
- INSN equivalent: `--timeslice-min 10 --timeslice-max 5000`

This should be documented in `--help` text and/or validated at startup.

### Optional: Naming Refactor

The `rbc_count` / `structop_rbc` field names in `PreemptionRecord` are
semantically tied to branches. For a generic system, these could be renamed to
`pmu_count` / `structop_pmu_count`. This is a large rename touching:

- `PreemptionRecord::rbc_count` and `structop_rbc`
- `StructopInfo::rbc_total`
- `E9SharedRbc` / `E9_SHARED_RBC` symbols
- Trace file format fields (`rbc=`, `timeslice=`)
- Various comments and doc strings

This rename is **not required for correctness** — the fields work regardless
of which event they count — but would improve clarity.

### Not Feasible: e9patch Instruction Instrumentation

Making e9patch count instructions instead of branches would require:
- Instrumenting every instruction (not just Jcc), which e9patch's `-M` matcher
  does not support efficiently
- Massive overhead: the trampoline would fire on every instruction instead of
  every ~3-10th instruction
- A complete redesign of the e9patch integration

This is **not recommended**. The e9patch backend should remain branch-counting
only. Users wanting instruction-level preemption should use the PMU backend.

## Conclusion

**Instruction-level preemption via `--break-on insn` is already implemented
and functional.** The infrastructure was designed to be event-agnostic from the
start, with `PmuEvent` parameterization throughout the stack.

The main gap is **practical validation**: no one has run stress tests with
`--break-on insn` to characterize:
1. Optimal timeslice ranges for instruction-level counting
2. Skid characteristics (run `measure_skid --event insn`)
3. Whether instruction-level preemption finds different bugs than RBC
4. Performance overhead compared to RBC at equivalent preemption rates

The e9patch backend is fundamentally incompatible with instruction-level
counting and should remain branch-counting only. PMU and HW breakpoint replay
backends work correctly with either event type.
