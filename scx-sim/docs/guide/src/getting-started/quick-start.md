# Quick Start

The shortest path from a built `scxsim` binary to a Perfetto-loadable
trace.

```bash
# From the scx-sim/ root, against the bundled hello-world example:
scxsim run \
    --scheduler lavd \
    --cpus 4 \
    --duration 200ms \
    --perfetto /tmp/hello.json \
    examples/hello.json
```

You should see (abridged):

```text
scxsim: disabling ASLR and re-executing...
... scheduler: lavd  cpus: 4  duration: 200ms  seed: 42 ...
... scxsim: ExitKind::Normal ...
```

Then drop `/tmp/hello.json` onto <https://ui.perfetto.dev/> to view the
schedule visually.

## What just happened (TODO)

This page will be expanded to:

- Annotate each CLI flag.
- Explain the stderr summary line-by-line.
- Show the Perfetto track layout (per-CPU tracks, per-task slices,
  cgroup metadata).

For now, see [Your First Simulation](./first-simulation.md) for a
narrative walk-through.
