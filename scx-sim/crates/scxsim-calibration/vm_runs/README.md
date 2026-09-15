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

**Pin `KTSTR_KERNEL` to a ktstr-built kernel.** This is not optional and it is
the single thing most likely to cost you an evening. With no kernel pinned,
ktstr resolves whatever is in the cache, and if that happens to be a *distro*
kernel the guest never comes up:

```text
send_sys_rdy failed within boot budget ... port_exists=false
stage: init script never started (kernel or mount failure)
```

Six retries, every scenario, including ones with a known-good committed
baseline. The diagnostic points at boot slowness and the documented remedy
(`KTSTR_NO_PERF_MODE=1`) does not help, because the cause is the kernel rather
than host contention — measured 2026-08-13, identical failure at
`elapsed_ms` 10602 and 10605. Checking `CONFIG_VIRTIO_CONSOLE` does not clear
the distro kernel either: Fedora's has it built in (`=y`) and still fails.

A ktstr-built kernel carries a `ktstr_kconfig_hash` in its `metadata.json`;
a distro entry does not. That field is the fastest way to tell which kind a
cache entry is.

```sh
KTSTR_KERNEL=<one-fs>/cache/kernels/6.14.11-tarball-x86_64-kcbe1d947f \
KTSTR_CACHE_DIR=<one-fs>/cache CARGO_TARGET_DIR=<one-fs>/target \
    cargo nextest run --test ktstr_sched_tests sched_basic_proportional
```

Measured 2026-08-14 with that pin, on the same host the failure above came
from: `sched_basic_proportional` PASS in 18.145 s, and
`cover_cgroup_io_compute_imbalance` PASS in 18.610 / 18.196 / 18.209 /
18.133 s. Of that ~18.2 s, 12.0 s is the scenario's declared duration, so boot
plus teardown plus evaluation is roughly 6.2 s.

Pinning a path also sidesteps the network: `--kernel 6.14` resolves the latest
6.14.x over the wire and needs `with-proxy`, while `KTSTR_KERNEL=<path>` reads
a cached kernel and needs neither network nor credentials.

If the cache holds no ktstr-built kernel, build one. `cargo-ktstr` is not on
`PATH` by default (it builds into ktstr's own target dir), and the build
downloads a tarball, so it needs `with-proxy`:

```sh
with-proxy cargo ktstr kernel build --kernel 6.14.11
```

`--kernel` takes the version as a flag value, not positionally. Verified from
`cargo ktstr kernel --help`; the build itself was not re-run for this note,
since the cache already held the entry.

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

## Retention: copy the sidecar here before the target dir is cleaned

**A VM run's sidecar is not reproducible without another VM run.** It lands in
`$CARGO_TARGET_DIR/ktstr/<kernel>-<commit>/`, which is build output — a
`cargo clean`, a target-dir change or a fresh worktree removes it, and nothing
warns you. Regenerating one costs a full guest boot plus the environment in the
walkthrough's Prerequisites (a `cargo-ktstr` on `PATH`, a device-matched clone
directory for `FICLONE`, and `with-proxy` for kernel-version resolution).

So: **after any VM run whose numbers you intend to cite, copy the sidecar into
this directory in the same commit as whatever cites it.** Not afterwards, not
"once it looks interesting" — by then the target dir has usually turned over.

This is written as an instruction rather than an argument because the argument
was already here and did not prevent the loss. Four scenarios were run on a
live guest on 2026-08-13 — `sched_cpuset_split`, `sched_dynamic_add`,
`sched_perf_positive`, `sched_verifier_stats_populated`, all at ktstr
`f5d0fce` — their per-cgroup CPU times were quoted in a report, and **the
sidecars themselves no longer exist anywhere on disk.** What survives of those
runs is a set of ~430-byte extracts, and only because a separate piece of work
happened to extract from them the same day. Those extracts land in
`crates/ktstr-scenario-replay/baselines/*.vm.json` with the cross-backend
CPU-time check; if that path is not in your checkout yet, that work has not
merged.

### The baselines are not a substitute, and the difference is specific

Those extracts carry `per_cgroup_cpu_time_ns` plus provenance. That is the right shape for
what they do — a cross-backend CPU-time check that stays reviewable in a diff
and carries no strings from the recording host.

They are not a replacement for the sidecar. Everything else the calibration
compares is dropped: `mean_run_delay_us`, `avg_off_cpu_pct`, `total_migrations`,
wake latency and timer latency, and the `*_measured` flags that say whether a
quantity was captured at all. Concretely, the full 13-metric calibration can be
extended to a new scenario **only** if that scenario's sidecar is in this
directory; a baseline extract supports the CPU-time comparison and nothing
further.

If you are recording a run, keep both: the sidecar here, the extract wherever
the check wants it.
