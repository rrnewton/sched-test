# Your First Simulation

> **Status — stub.** This page will walk through `examples/hello.json`
> step by step: the JSON schema fields, what the simulator does on
> each tick, the resulting stderr summary, and the Perfetto trace.

Planned outline:

1. **Inspect the workload.** Open `examples/hello.json`; map each
   field to a concept (`global.duration`, per-task `loop`/`run`/`sleep`,
   priorities).
2. **Pick a scheduler.** Compare `--scheduler simple` vs
   `--scheduler lavd` runs of the same workload; observe summary
   differences.
3. **Set a seed.** Run with `--seed 42` twice; observe that the trace
   is byte-identical. Run with `--seed entropy`; observe that it isn't.
4. **Capture a Perfetto trace.** Use `--perfetto /tmp/hello.json`,
   open in <https://ui.perfetto.dev/>, identify per-CPU tracks, task
   slices, and `ExitKind::Normal` end-of-run marker.
5. **Bump up the noise.** Add `--no-noise` and `--no-overhead`,
   re-run; compare against the default to see the realism model in
   action.

Until this page is written, the [`scx-sim/README.md`][readme] Quick
Start section is the closest authoritative reference.

[readme]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/README.md
