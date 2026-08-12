# Live VM runs, as the ktstr sidecar wrote them

Each file here is a **verbatim, unedited** ktstr stats sidecar from a real
guest run. They are the live half of a calibration comparison; the simulated
half is produced on demand by running the same scenario.

Verbatim matters. A summarised or hand-transcribed VM number cannot be
re-examined when a later metric turns out to be interesting, and cannot be
audited for the `*_measured` flags that decide whether a quantity was captured
at all. The first version of this comparison had exactly that problem: the only
VM data in the tree was two integers in a `println!`, and the four metrics
nobody had thought to transcribe were unrecoverable without re-running a guest.

## `sched_basic_proportional-6.14.11-85c72e1.ktstr.json`

| | |
|---|---|
| scenario | `sched_basic_proportional` (2 spinners, 2 cgroups, 2 CPUs, 12 s) |
| kernel | source-built 6.14.11 |
| scheduler | `scx-ktstr` (ktstr's own BPF scheduler) |
| ktstr commit | `85c72e1` (PR #43, the `#[ktstr_scenario]` port) |
| result | `passed: true` |
| written by | ktstr's sidecar writer, to `$CARGO_TARGET_DIR/ktstr/<kernel>-<commit>/` |
| captured | 2026-08-12 |

Produced by the run reported on tg `ktstr-one-test-both-backends`:

```text
PASS [  19.088s] (1/1) ktstr::ktstr_sched_tests ktstr/sched_basic_proportional
```

### Reproducing it

The guest run needs a working ktstr VM path, which on this host required
putting the source checkout, `KTSTR_CACHE_DIR` and `CARGO_TARGET_DIR` on ONE
filesystem — ktstr's content-addressed store publishes pinned scheduler
artifacts with `FICLONE` and hard-bails on `EXDEV` rather than falling back to
a copy. Also needs `cargo-nextest >= 0.9.143`.

```sh
KTSTR_CACHE_DIR=<one-fs>/cache CARGO_TARGET_DIR=<one-fs>/target \
    cargo nextest run --test ktstr_sched_tests sched_basic_proportional
```

The sidecar then lands under `$CARGO_TARGET_DIR/ktstr/<kernel>-<commit>/`.

### What this file does and does not carry

It is one run, so **N = 1**. Every aggregate metric clears
`MinSamples::AGGREGATE`; nothing here can clear `MinSamples::PERCENTILE`
(N >= 100), so no percentile comparison drawn from this file may be reported as
agreement.

It carries per-cgroup `total_cpu_time_ns`, `avg_off_cpu_pct`,
`total_migrations`, `total_iterations` and run-delay, plus VM-wide `monitor`
schedstat deltas. It records `wake_measured: false` and `timer_measured: false`
— the wake-latency fields are present but were never populated, and the zeros
in them are placeholders, **not measurements**. `crate::vm` reads those flags
and yields `None`; anything that reads `p99_wake_latency_us` without checking
`wake_measured` will silently calibrate against a fabricated zero.
