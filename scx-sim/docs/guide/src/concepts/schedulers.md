# Schedulers

scxsim ships five BPF schedulers, built from the in-tree
[`scx`](https://github.com/sched-ext/scx) submodule and cached under
`scx-sim/target/release/build/.../out/schedulers/` after a release
build. Pick one with `--scheduler <name>`.

## The five bundled schedulers

| `--scheduler` | `.so` | Brief | Primary use |
|---|---|---|---|
| `simple` | `libscx_simple.so` | Minimal direct-dispatch scheduler. | Baseline. Useful for sanity-checking that observed behaviour comes from a real scheduler decision rather than the simulator itself. |
| `lavd` | `libscx_lavd.so` | Latency-Aware Virtual Deadline. | Primary production target. The H6/Bug-1 cgroup-bandwidth investigation revolves around this scheduler. |
| `cosmos` | `libscx_cosmos.so` | Cgroup-aware scheduler. | Workloads that exercise cgroup hierarchies; comparison target for LAVD on the same workload. |
| `mitosis` | `libscx_mitosis.so` | Cell-based scheduler. | Cell-affinity workloads. |
| `tickless` | `libscx_tickless.so` | Tickless-mode scheduler. | Workloads that probe the no-tick code path. |

## Listing what's loadable

```bash
scxsim run --list-schedulers
```

prints each scheduler name and the resolved `.so` path:

```text
cosmos     /.../target/release/build/scx_simulator-.../out/schedulers/libscx_cosmos.so
lavd       /.../target/release/build/scx_simulator-.../out/schedulers/libscx_lavd.so
mitosis    /.../target/release/build/scx_simulator-.../out/schedulers/libscx_mitosis.so
simple     /.../target/release/build/scx_simulator-.../out/schedulers/libscx_simple.so
tickless   /.../target/release/build/scx_simulator-.../out/schedulers/libscx_tickless.so
```

If a scheduler is missing, the build did not produce it; check the
top-level [`scx-sim/README.md`][readme] for the build flags that
enable each. The selection is normally automatic, but a partial
build can omit individual schedulers.

[readme]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/README.md

## Pointing at a custom `.so`

Use `--scheduler-file` to override the default `libscx_<name>.so`
lookup:

```bash
scxsim run \
    --scheduler-file /path/to/libscx_custom.so \
    --cpus 4 --duration 200ms \
    examples/cpu_bound.json
```

The basename of the `.so` must match the `libscx_<name>.so` pattern,
because downstream code (`scheduler_prefix_from_path`,
`derive_e9rip_path`) parses the prefix from the filename.

### Per-revision binary cache (bin_cache)

The bin_cache mechanism in `scripts/` builds a `libscx_<name>.so` per
revision and stores them under
`experiments/bin_cache/<sha>/libscx_<name>.so`. Combined with
`--scheduler-file`, this is how regressions are bisected without
recompiling on every commit:

```bash
for sha in <list-of-shas>; do
    scxsim run \
        --scheduler-file experiments/bin_cache/$sha/libscx_lavd.so \
        --cpus 4 --duration 500ms \
        --watchdog 80ms \
        --config tests/fixtures/h6/bug1_canonical.toml \
        tests/fixtures/h6/bug1_canonical.json
    echo "$sha: exit=$?"
done
```

Why this matters: cargo's `rerun-if-changed` does not include
`scheds/rust/scx_lavd/src/bpf`, so swapping the scx submodule SHA
between `cargo build`s yields a stale `.so` unless you point at a
SHA-specific binary. The bin_cache + `--scheduler-file` workflow is
the canonical workaround.

See `experiments/bug1_scx_version_matrix_20260512/README.md` for the
full matrix workflow.

## Per-scheduler feature gates

Many schedulers expose `const volatile` feature flags that are
**off** in scxsim's default load (`lavd_setup()` in
`schedulers/lavd/wrapper.c` is the canonical example). To turn one
on, write a TOML sidecar and pass it with `--config`:

```toml
[scheduler.bool_globals]
enable_cpu_bw = true
```

```bash
scxsim run -s lavd --config my-config.toml ... <workload>
```

See [Running Simulations → Scheduler Config Sidecar](../running-simulations/scheduler-config.md)
for the full mechanism, the four supported sub-tables, and the
loudness guarantees.

## Choosing a scheduler

Quick guidance for new users:

- **First runs:** `simple` (minimum surface area).
- **Production reproducer:** `lavd` (primary target, where most
  investigation happens).
- **Cgroup work:** `lavd` with `--config enable_cpu_bw=true`, or
  `cosmos`.
- **Comparing two schedulers on the same workload:** see
  [Recipes → Comparing Two Schedulers](../recipes/compare-schedulers.md).
