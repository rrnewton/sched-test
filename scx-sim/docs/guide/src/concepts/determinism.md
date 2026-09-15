# Determinism and Seeds

scxsim's headline guarantee is that two runs of the same workload
under the same configuration produce byte-identical output. This
page is the precise statement of that guarantee, the knobs that
control it, and the verification path.

## The contract

> **Same workload + same `--seed` + same scheduler `.so` + same
> scxsim revision ⇒ byte-identical trace.**

"Byte-identical" applies to:

- the stderr summary block,
- the `--perfetto` output (JSON or protobuf),
- the `--structops-jsonl` output,
- the `--record-preemptions` output,
- the verbose-summary block.

It does **not** apply to:

- wallclock-derived metadata (e.g. file timestamps),
- log lines printed before the simulator's own re-exec (the
  pre-re-exec lines come from a non-deterministic Rust startup path
  and are filtered out of the diff harness),
- traces captured under stress-mode flags with `--seed entropy` (by
  design — see [Twin Design Principles](./twin-design-principles.md)).

## The `--seed` flag

```text
--seed <SEED>
    PRNG seed (u32 integer or "entropy" for OS randomness).
    Falls back to SCX_SIM_SEED env var, then default (42).
```

(**PRNG** = Pseudo-Random Number Generator — see [Glossary](../glossary.md).)

The seed feeds three sources of randomness:

1. **Tick jitter.** Small offsets on per-CPU scheduling-tick
   timestamps (suppressed by `--no-noise`).
2. **Context-switch overhead noise.** Per-switch noise on top of
   the structop RBC (Retired Branch Count, a hardware PMU event —
   see [Glossary](../glossary.md)) cost (suppressed by
   `--no-overhead`).
3. **Event tiebreaking.** When two events have the same simulated
   timestamp, default behaviour PRNG-randomizes their order. This is
   how rare ordering-dependent bugs become statistically discoverable
   across seed sweeps. Override with `--fixed-priority` (use
   insertion order instead).

Default seed is `42` if neither `--seed` nor `SCX_SIM_SEED` is set.

## Knobs that affect what is deterministic

| Flag | Effect on the trace |
|---|---|
| `--seed N` | Pin the PRNG. Re-runs are byte-identical. |
| `--seed entropy` | OS randomness. Re-runs *intentionally* differ; used for fuzzing across seeds. |
| `--fixed-priority` | Insertion-order tiebreaking. Removes one degree of nondeterminism but masks ordering bugs the PRNG would expose. Avoid except for ground-truth tracing. |
| `--no-noise` | Remove tick jitter. |
| `--no-overhead` | Remove context-switch-overhead noise. |
| `--no-rbc` / `--rbc-ns 0` | Disable PMU-derived scheduler overhead. Equivalent to "the scheduler is instantaneous." |
| `--rbc-ns N` | Set ns charged per retired conditional branch (default `10`). Higher = heavier scheduler. |

The strict-determinism upper bound — used by tests that want zero
variance — is:

```bash
scxsim run -s lavd --seed 42 --no-noise --no-overhead --rbc-ns 0 ...
```

## `--determinism-check`

The single-line CI gate:

```bash
scxsim run --scheduler lavd --cpus 4 --duration 50ms --seed 42 \
    --determinism-check examples/hello.json
```

Last line on success:

```text
Determinism check PASSED: 22 checkpoints matched
```

`--determinism-check` runs the simulation twice internally with the
same seed, enables an aggressive checkpoint mode that records
deterministic state at every scheduling event, and compares the
checkpoint sequences from both runs. On divergence it exits non-zero
with a report of the first checkpoint that differs.

The 22-checkpoint count above is workload-dependent (hello.json is
small). The `cpu_bound` workload, for example, produces ~355
checkpoints across a 100 ms simulation. Number of checkpoints does
not matter; the success line is what CI matches.

## ASLR and re-execution

The first thing scxsim does is print:

```text
scxsim: disabling ASLR and re-executing...
```

and re-exec itself with `personality(ADDR_NO_RANDOMIZE)`. This makes
`.so` base addresses stable across runs, which is a requirement for
deterministic replay (`scxsim replay`) and for the `bin_cache`
regression-bisect workflow.

Opt out with `--no-disable-aslr` when wrapping scxsim in a script
that itself sets process attributes you don't want clobbered — at the
cost of losing the determinism guarantee for replay.

## What are valid preemption targets?

Determinism depends on where in the scheduler's execution we are
allowed to preempt one structop and run another. Not all preemption
points are reproducible across runs, and the trade-offs differ by
target.

1. **Natural yield points.** A scheduler structop naturally yields
   back to userspace / kernel when it is DONE executing (e.g.
   `ops.enqueue` returns, `ops.dispatch` finishes its work). These
   are deterministic by construction — the boundary is defined by
   the scheduler's own control flow, not by an external counter.

2. **Branch-based preemption (RBC).** Additional preemptions in the
   middle of scheduler C code can target conditional branches. This
   is efficient because of the precise, deterministic Retired Branch
   Counter (RBC) in the CPU. RBC counting **is deterministic under
   speculation**, so seeds using RBC-based preemption are fully
   reproducible across runs WITHOUT a recording step. This is the
   default break mode (`break_on: rbc` in preemption-trace headers).

3. **Arbitrary instruction preemption.** For finer-grained
   interleaving exploration, we can preempt at arbitrary
   instructions — not just conditional branches. However, this
   relies on counters (e.g. retired instruction count) that are
   **intrinsically nondeterministic under speculation**. Therefore:

   - The ONLY way to get reproducibility with arbitrary-instruction
     preemption is to **record nondeterministically first**, then
     **replay** using the e9patch backend to insert traps at the
     exact instructions we know we want to break on.
   - **Disadvantage:** requires a recording phase; no
     run-from-cold reproducibility from seed alone.
   - **Advantage:** can explore interleavings that RBC-only
     preemption CANNOT reach — for example, splitting up a block
     of write instructions with no conditional branch between
     them.

## Record / replay

Record / replay is NOT a "stronger" form of determinism — it is a
different trade-off in the preemption-target space described above:

| Mode | Reproducible without recording? | Interleavings reachable |
|---|---|---|
| RBC preemption (default) | **Yes** — RBC is deterministic under speculation. Same seed → same trace. | Conditional-branch boundaries only. |
| Arbitrary-instruction + record/replay | **No** — recording phase is mandatory; instruction-count signal delivery is nondeterministic. | Any instruction, including inside straight-line code with no branches. |

Use record/replay when you specifically need to reach an
interleaving that RBC preemption cannot — for example, splitting a
contiguous block of writes that has no intervening conditional
branch. For everything else, the default RBC mode gives byte-
identical reruns from `--seed` alone with no recording step.

The capture / replay commands:

```bash
# Capture (records arbitrary-instruction preemption sites observed
# during a nondeterministic run)
scxsim run -s lavd --cpus 4 --duration 200ms \
    --record-preemptions /tmp/preempts.txt \
    examples/cpu_bound.json

# Replay (later, possibly on another machine with the same .so).
# Uses e9patch to insert traps at the exact recorded instruction
# addresses, so the recorded interleaving is reproduced bit-for-bit.
scxsim replay /tmp/preempts.txt
```

The preemption trace is a small text file with a metadata header
followed by one line per recorded preemption point. The header
captures the seed, the workload, the scheduler `.so` path, and a
hash of the `.so` so that mismatches are caught at load time:

```text
# scxsim preemption trace
# workers: 4
# break_on: rbc
# total: 0
# nr_cpus: 4
# nr_tasks: 1
# seed: 42
# duration_ns: 30000000
# scheduler: lavd
# so_hash: 0xa4f5c653d5aed108
# so_path: /.../scx-sim/target/release/build/scx_simulator-.../out/schedulers/libscx_lavd.so
```

The `break_on:` field records which preemption-target class was in
use during capture (`rbc` for the default branch-based mode, or
`insn` for arbitrary-instruction mode). A replay run rejects a
trace whose `so_hash` does not match the loaded scheduler binary —
mismatched `.so` files cannot reproduce the recorded instruction
addresses.

See [Running Simulations → Replaying Preemption Traces](../running-simulations/replay.md)
for the full file format.

## What is *not* deterministic

By design:

- **`--seed entropy`** — uses OS randomness. Re-runs differ.
- **`vm-run` outputs** — driven by a real kernel; subject to real
  wallclock jitter, ASLR, real interrupts.
- **Wallclock metadata.** Trace files have a creation time; the
  events inside are deterministic but the file's mtime is not.

By bug:

- Any divergence under fixed `--seed`, fixed `.so`, fixed scxsim
  revision is a bug, and `--determinism-check` is the standing CI
  gate that catches it.

## Verifying a reproducer

The hardening checklist for "is my Bug-1 reproducer deterministic?":

1. Pin everything: `--seed 42 --scheduler-file /path/to/specific.so
   --config repro.toml`.
2. `--determinism-check`. Must PASS.
3. Two-runs diff: see [Recipes → Verifying Determinism](../recipes/verify-determinism.md).
4. Cross-machine: capture `--record-preemptions /tmp/p.txt` and have
   a teammate `scxsim replay /tmp/p.txt` on their machine.
