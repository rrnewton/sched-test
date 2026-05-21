# Schedulers

> **Status — stub.** This page will catalogue the five bundled
> schedulers, what each is good for, and how to point scxsim at a
> scheduler built outside the workspace.

| Name | `.so` | Brief |
|---|---|---|
| `simple` | `libscx_simple.so` | Minimal scheduler, useful as a baseline and for testing the simulator itself. |
| `lavd` | `libscx_lavd.so` | Latency-aware virtual deadline; the primary production target. |
| `cosmos` | `libscx_cosmos.so` | Cgroup-aware scheduler. |
| `mitosis` | `libscx_mitosis.so` | Cell-based scheduler. |
| `tickless` | `libscx_tickless.so` | Tickless-mode scheduler. |

The five `.so` files are produced as part of the scx-sim Cargo build;
their paths can be inspected at runtime with:

```bash
scxsim run --list-schedulers
```

To run an out-of-tree scheduler:

```bash
scxsim run --scheduler-file /path/to/libscx_custom.so <workload>
```

`--scheduler-file` overrides the default `libscx_<name>.so` lookup;
combined with the bin_cache mechanism in `scripts/`, this is how
per-revision regressions are bisected.
