# rt-app Workloads

scxsim accepts rt-app-compatible JSON workloads. A workload describes
a set of named tasks, what they do in their inner loop (`run` /
`sleep` / `resume` / `suspend` actions), and optional cgroup
membership for each task.

> **Status — stub.** This page will fully document the JSON schema as
> scxsim parses it (`safe/rtapp.rs`), including the scxsim-specific
> extensions to baseline rt-app (e.g. the `taskgroup.cpu.max` field).

## Minimal shape

```json
{
  "global": {
    "duration": 0.5,
    "default_policy": "SCHED_OTHER"
  },
  "tasks": {
    "hello": {
      "loop": 5,
      "run": 1000,
      "sleep": 1000
    }
  }
}
```

- `global.duration` is in **seconds** (fractional allowed; e.g. `0.5`
  = 500ms). Overrides apply via `--end-time` / `--duration`.
- `tasks.<name>.loop` is iteration count (`-1` = infinite, capped by
  `global.duration`).
- `tasks.<name>.run` and `tasks.<name>.sleep` are in **microseconds**.

## Cgroup hierarchy

Append a `taskgroup` object to a task to place it in a cgroup:

```json
"yes_0": {
    "loop": -1,
    "run": 200000,
    "taskgroup": { "path": "/test_bw_tight", "cpu.max": "10000 100000" }
}
```

- `path` is the cgroup path (must start with `/`).
- `cpu.max`, when present, sets the cgroup's bandwidth in the same
  `"<quota_us> <period_us>"` form as the real cgroup-v2 file. `"max
  <period>"` is allowed for unlimited quota.

The example workloads under `scx-sim/examples/` exercise each of these
patterns; see [Example Workloads](../reference/example-workloads.md).
