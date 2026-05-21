# What scxsim Simulates

> **Status — stub.** This page will enumerate what scxsim models, what
> it stubs out, and where the boundaries are.

Modelled (sketch):

- CPU topology (`-c/--cpus`, `--smt`).
- Per-CPU run state, idle / busy transitions, context switches.
- struct_ops callbacks invoked on the loaded scheduler `.so`.
- DSQs (dispatch queues) with FIFO semantics.
- Cgroup hierarchy + `cpu.max` quota/period throttling and
  replenishment timers (`scx_cgroup_tree` crate).
- Time advancement (virtual time, not wallclock).
- Tick jitter, context-switch overhead noise (gated by `--no-noise`
  and `--no-overhead`).

Not modelled (sketch):

- Page faults, memory pressure.
- Network and disk I/O.
- Hardware interrupts other than the scheduling tick and accounting
  timer.
- Per-core voltage/frequency dynamics.
- The full Linux fair-class scheduler — scxsim is a sched_ext
  simulator, not a CFS simulator.

See also [Concepts → Twin Design Principles](./twin-design-principles.md)
for why scxsim sometimes intentionally exaggerates a knob (and why
those exaggerations are opt-in).
