# Schedulers

scxsim ships five sched_ext schedulers, built from the in-tree
[`scx`](https://github.com/sched-ext/scx) submodule and cached under
`scx-sim/target/release/build/.../out/schedulers/` after a release
build. Each scheduler's C source — the same source that the kernel
build compiles to BPF (Berkeley Packet Filter) bytecode — is here
compiled with clang's **native** target to produce a `libscx_<name>.so`
shared library; see [Introduction](../introduction.md) for the
pipeline distinction. Pick one with `--scheduler <name>`.

## The five bundled schedulers

| `--scheduler` | `.so` | Brief | Primary use |
|---|---|---|---|
| `simple` | `libscx_simple.so` | Minimal direct-dispatch scheduler. | Baseline. Useful for sanity-checking that observed behaviour comes from a real scheduler decision rather than the simulator itself. |
| `lavd` | `libscx_lavd.so` | Latency-Aware Virtual Deadline. | Primary production target. The H6/Bug-1 cgroup-bandwidth investigation revolves around this scheduler. |
| `cosmos` | `libscx_cosmos.so` | Cgroup-aware scheduler. | Workloads that exercise cgroup hierarchies; comparison target for LAVD on the same workload. |
| `mitosis` | `libscx_mitosis.so` | Cell-based scheduler. | Cell-affinity workloads. |
| `tickless` | `libscx_tickless.so` | Tickless-mode scheduler. | Workloads that probe the no-tick code path. |

## `tickless`: timer substrate history

`tickless` is fully exercised today, but was not until the timer substrate
landed. Recorded here because the failure mode is instructive.

Its defining mechanism — the periodic BPF timer — did not run at all.
`llvm-cov` put `scx_tickless/src/bpf/main.bpf.c` at 20 of 28 functions, and
all eight that never executed were the one path the scheduler exists for:
`init_timer`, `start_timer`, `start_timer_on_cpu`, `sched_timerfn`,
`tick_interval_ns`, plus the `dispatch_cpu` / `dispatch_all_cpus` /
`is_pcpu_task` bounce path the callback drives. `ops.dispatch` degenerated to
a bare `scx_bpf_dsq_move_to_local(SHARED_DSQ)`.

Three defects were stacked, each hiding the next:

1. The per-run arena reset rewound the bump pointer to zero, wiping the
   primary-CPU `bpf_cpumask` that `tickless_setup()` allocates at load time
   (mb `sim-hfvmf`). `is_primary_cpu()` was therefore false forever, so
   `tickless_init()` never reached `init_timer()`. This also affected cosmos.
2. The wrapper had no `bpf_timer` plumbing at all (mb `sim-rq117`). libbpf
   declares those helpers as function pointers holding the raw helper id, so
   the call would have jumped to address 169 rather than failing to link.
3. `cpu_ctx_stor` was registered without pre-seeding, so `try_lookup_cpu_ctx()`
   returned `NULL` and `init_timer()` bailed with `-ENOENT`. Kernel
   `BPF_MAP_TYPE_ARRAY` maps are preallocated and never return `NULL` for an
   in-range index, so this was a substrate fidelity bug.

Fixing any one alone left the others masked; the second would have turned a
silent gap into a `SIGSEGV`. With all three fixed, `main.bpf.c` reaches
**28/28 functions (100%)** and 85% of lines, and `sched_timerfn` fires.

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

[readme]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/README.md

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
