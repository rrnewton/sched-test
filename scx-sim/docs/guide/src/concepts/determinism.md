# Determinism and Seeds

> **Status — stub.** This page will cover the determinism guarantees
> (and intentional non-guarantees) and how to harden a reproducer.

Planned content:

- The contract: **same workload + same `--seed` + same scheduler `.so`
  + same scxsim revision ⇒ byte-identical trace.**
- The `--seed` flag (`u32` integer, or the literal `entropy` for OS
  randomness; falls back to env `SCX_SIM_SEED`, then to `42`).
- The `--fixed-priority` flag (insertion-order tiebreak instead of
  PRNG-randomized; used to detect ordering-dependent bugs).
- `--no-noise`, `--no-overhead`, `--no-rbc`, `--rbc-ns` — knobs that
  trade realism for determinism granularity.
- `--determinism-check` — runs twice and compares the checkpoint
  sequences; the canonical CI-side check.
- Why ASLR is disabled and the process re-execs (`--no-disable-aslr`
  to opt out); rationale: stable `.so` base addresses for
  deterministic replay.
- Record/replay: capture preemption sites with
  `--record-preemptions PATH` and re-run with `scxsim replay`.
