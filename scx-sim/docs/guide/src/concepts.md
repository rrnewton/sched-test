# Concepts

The conceptual surface area you need to use scxsim productively.

- [What scxsim Simulates](./concepts/what-scxsim-simulates.md) — and
  what it deliberately does not.
- [Twin Design Principles](./concepts/twin-design-principles.md) —
  match-production by default; opt-in exaggerated knobs for stress
  testing.
- [rt-app Workloads](./concepts/workloads.md) — the JSON schema scxsim
  accepts.
- [Schedulers](./concepts/schedulers.md) — the five bundled `.so`s and
  how to point at a custom one.
- [Determinism and Seeds](./concepts/determinism.md) — what is
  deterministic, what isn't, and how to harden a reproducer.
- [Cgroup Bandwidth](./concepts/cgroup-bw.md) — how `cpu.max`,
  throttling, and the accounting timer behave inside scxsim.
