# scxsim examples

Short, well-commented rt-app workloads for the scxsim guide. Each file
is a runnable starting point — copy one, change the numbers, observe.

These differ from `crates/scx_simulator/workloads/` (smoke-test
fixtures) and `crates/scx_simulator/tests/fixtures/` (integration-test
fixtures, often tuned to reproduce specific bugs) in that they are
optimized for clarity, not for triggering scheduler behaviour.

## Files

| File | What it demonstrates |
|---|---|
| `hello.json` | Minimal single-task workload. The "hello world" of scxsim. |
| `cpu_bound.json` | Four CPU-bound workers exercising fan-out and time-slicing. |
| `cgroup_hierarchy.json` | Two cgroups (`/background` throttled to 20%, `/interactive` unconstrained), four tasks split across them. |

## Running them

All examples assume `scxsim` is built and on `$PATH` (or invoke as
`target/release/scxsim` from the workspace root).

```bash
# Minimal hello.
scxsim run -s lavd --cpus 4 --duration 100ms examples/hello.json

# CPU-bound, with a Perfetto trace.
scxsim run -s lavd --cpus 4 --duration 200ms \
    --perfetto /tmp/cpu_bound.json examples/cpu_bound.json

# Cgroup hierarchy with verbose per-cgroup summary.
scxsim run -s lavd --cpus 4 --duration 200ms --verbose-summary \
    examples/cgroup_hierarchy.json
```

Drop any `--perfetto` JSON output onto <https://ui.perfetto.dev/> to
view per-CPU tracks and per-task slices visually.

## Conventions

- `_comment` and `_*` fields are scxsim/rt-app idiom for inline
  comments inside JSON (JSON has no native comment syntax). They are
  parsed and ignored by scxsim's rt-app loader.
- `global.duration` is in **seconds** (fractional allowed, e.g. `0.2`
  = 200ms). Override at run time with `--duration` or `--end-time`.
- Per-task `run` and `sleep` are in **microseconds**.
- `taskgroup.cpu.max` is in the cgroup-v2 `"<quota_us> <period_us>"`
  form. `"max <period>"` is allowed for unlimited quota.

See the guide chapter
[`docs/guide/src/concepts/workloads.md`](../docs/guide/src/concepts/workloads.md)
for the full rt-app JSON schema scxsim accepts.
