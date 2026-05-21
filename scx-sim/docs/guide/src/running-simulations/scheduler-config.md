# Scheduler Config Sidecar (TOML)

The `--config <PATH>` flag points at a TOML file that sets per-symbol
BPF globals (`const volatile`) inside the loaded scheduler `.so` after
load but before `ops.init()`. It is the mechanism for toggling
scheduler features without rebuilding.

## Why it exists

`DynamicScheduler::get_symbol::<T>()` requires the caller to declare
the symbol type — there is no introspection that can derive `T` from
the `.so`'s ELF/DWARF. So the TOML file segregates globals into
typed sub-tables under `[scheduler.*]`. A symbol declared in the
wrong sub-table is undefined behaviour at write time.

## File shape

All keys live under a single top-level `[scheduler]` table, split
into typed sub-tables by the C-side type of the symbol:

```toml
# All examples below are illustrative. Only symbols that actually
# exist in the loaded scheduler .so are valid.

[scheduler.bool_globals]
enable_cpu_bw = true

[scheduler.u8_globals]
# example_u8 = 7

[scheduler.u32_globals]
some_threshold = 1024

[scheduler.u64_globals]
period_ns = 1_000_000_000
```

| Sub-table | C-side type | Use for |
|---|---|---|
| `[scheduler.bool_globals]` | `const volatile bool` | Feature flags. |
| `[scheduler.u8_globals]` | `const volatile u8` | Small enums, byte-sized counts. |
| `[scheduler.u32_globals]` | `const volatile u32` | Threshold values, ratios. |
| `[scheduler.u64_globals]` | `const volatile u64` | Time values in nanoseconds, large counts. |

## Loudness guarantees

- **Unknown top-level table → loader error.** The TOML deserializer
  has `deny_unknown_fields`, so a typo like `[scheduler.bool_global]`
  (missing the `s`) fails parsing rather than silently no-op'ing.
- **Symbol not found in `.so` → loader error.** Each key is looked up
  by literal name; a missing symbol returns `Err` rather than being
  silently ignored.
- **Type mismatch with `.so` → undefined behaviour at write time.**
  The TOML caller asserts the type via the sub-table name; the loader
  trusts it. Keep the sub-table you put a key in matched to the
  symbol's actual type in BPF.

## Worked example: Bug-1 reproducer

The shipped fixture
[`tests/fixtures/h6/bug1_canonical.toml`][bug1-toml] is the canonical
worked example:

```toml
{{#include ../../../../crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml:46:47}}
```

Why this single line matters: `lavd_setup()` in
`schedulers/lavd/wrapper.c` hard-codes `enable_cpu_bw = false` for
the simulator's initial state ("Disable complex features for initial
simulation"). Without the `--config` override, LAVD's `lavd_enqueue`
short-circuits the `cgroup_throttled()` check at
`scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c:817`
(`if (enable_cpu_bw && …)`), the cgroup-bandwidth code path **never
runs**, and the Bug-1 stall doesn't reproduce — but it *looks* like a
clean run.

Invocation:

```bash
scxsim run \
    -s lavd \
    --cpus 4 \
    --duration 500ms \
    --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

See the [Reproducing a Stall Bug](../recipes/repro-stall.md) recipe
for the full walkthrough.

## Common pitfalls

1. **Forgetting `--config`.** Many recipes (including Bug-1) require
   the sidecar to enable the relevant scheduler feature. Without it,
   the workload may look like it ran cleanly when in fact the bug's
   enabling condition was bypassed.
2. **Wrong sub-table.** `enable_cpu_bw` is `bool` in LAVD; putting it
   under `[scheduler.u32_globals]` will write four bytes through a
   one-byte symbol — undefined behaviour. Use `bpftool` or the BPF
   skeleton header to confirm the C-side type.
3. **`[scheduler.bool_globals]` vs `[bool_globals]`.** The sub-tables
   live under `[scheduler]`. A top-level `[bool_globals]` table is an
   unknown-field parse error.
4. **Stale globals from a previous run.** Globals are `const volatile`
   — scxsim writes once at load time, after which the value behaves
   like a constant for the duration of the run. To change a value,
   exit and restart with a new `--config`.

[bug1-toml]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml
