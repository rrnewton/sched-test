# VM Runs (`vm-run`)

`scxsim vm-run` is the ground-truth half of scxsim: it drives the
same workload against a real Linux kernel running inside
[virtme-ng (`vng`)][vng], with the chosen scheduler loaded as a
real BPF program in that VM. The output is directly comparable
against a `scxsim run` trace of the same workload.

[vng]: https://github.com/arighi/virtme-ng

```text
scxsim vm-run [OPTIONS] <WORKLOAD>
```

## Why this exists

scxsim is a simulator; it can be wrong. The [Twin Design Principles](../concepts/twin-design-principles.md)
section makes the policy explicit: every divergence between
`scxsim run` and a real kernel running the same workload is debt
to file. `vm-run` makes that comparison cheap by running the
real-kernel side from the same binary, with the same CLI shape,
on the same workload file.

## Prerequisites

- `vng` (virtme-ng) on `$PATH`.
- A kernel image and modules suitable for the schedulers you intend
  to load. The `vng` defaults usually suffice; otherwise pass a
  custom kernel via `vng`'s own mechanisms.
- For `--bpf-trace`: `bpftrace` and `trace_scx_ops.bt` in CWD.
- For `--wprof`: a wprof binary reachable by the VM.

## Options

| Flag | Default | Purpose |
|---|---|---|
| `-s, --scheduler <NAME>` | `simple` | Scheduler to load inside the VM (matches the simulator's `--scheduler` set: `simple`, `lavd`, `cosmos`, `mitosis`, `tickless`). |
| `-c, --cpus <N>` | `4` | Number of workload CPUs in the VM. Tracing modes add one extra VM CPU for the tracer. |
| `--wprof` | off | Record a Perfetto trace via wprof on an isolated VM CPU. Trace file written to CWD. |
| `--bpf-trace` | off | Trace scheduler `ops` callbacks and kfunc calls via `trace_scx_ops.bt`. Writes `bpf_trace.log` in CWD. |
| `--scheduler-args <ARGS>` | — | Raw shell args appended to the in-VM scheduler command. Use `=` form (`--scheduler-args=--enable-cpu-bw`) when the first arg starts with `-`. |
| `--pre-hook <PATH>` | — | Executable run inside the VM after the scheduler starts and before rt-app starts. |
| `--post-hook <PATH>` | — | Executable run inside the VM after rt-app exits and before the scheduler/tracer stop. |
| `--no-disable-aslr` | ASLR disabled | Skip ASLR-disable + re-exec. |

Hooks see `SCXSIM_*` environment variables, including
`SCXSIM_SCHED_PID` (the PID of the running scheduler inside the
VM). Use a pre-hook to install bpftrace probes that need to attach
*after* the scheduler is up; use a post-hook to capture additional
state before tear-down.

## Common invocations

### Bare run

```bash
scxsim vm-run -s lavd --cpus 4 examples/cpu_bound.json
```

Runs the workload under LAVD inside a VM. Output to stdout/stderr
is whatever the VM produces; the workload exit code propagates.

### With wprof

```bash
scxsim vm-run -s lavd --cpus 4 --wprof examples/cpu_bound.json
```

An extra VM CPU is added and isolated (via `isolcpus`) to run wprof.
A Perfetto trace lands in CWD; load directly in <https://ui.perfetto.dev/>
or compare side-by-side with a `scxsim run --perfetto file.pb
--trace-format perfetto` capture using scxtop's `load_perfetto_trace`.

### With bpftrace probes

```bash
scxsim vm-run -s lavd --cpus 4 --bpf-trace examples/cpu_bound.json
```

Captures `sched_class` entry points, `scx_bpf_*` kfunc calls with
return values, and `sched_switch` / `sched_wakeup` lifecycle events
into `bpf_trace.log` in CWD. This is the canonical fidelity-check
input: feed it together with a `--structops-jsonl` capture into
`scripts/compare_live_vs_scxsim_calls.sh`.

### Forwarding flags to the scheduler

```bash
scxsim vm-run -s lavd --cpus 4 \
    --scheduler-args="--enable-cpu-bw --slice-min-us 500" \
    examples/cpu_bound.json
```

Note the `=` syntax: required when the first forwarded arg starts
with `-`, because clap would otherwise parse `--enable-cpu-bw` as
scxsim's own flag.

### With a pre-hook

```bash
cat > /tmp/install-probes.sh <<'EOF'
#!/bin/sh
# Runs inside the VM, after lavd is up, before rt-app starts.
bpftrace -p "$SCXSIM_SCHED_PID" /trace/extra.bt &
EOF
chmod +x /tmp/install-probes.sh

scxsim vm-run -s lavd --cpus 4 \
    --pre-hook /tmp/install-probes.sh \
    examples/cpu_bound.json
```

The hook receives all `SCXSIM_*` environment variables (workload
path, scheduler name, etc.) and the scheduler's PID via
`SCXSIM_SCHED_PID`.

## Diffing against a simulator run

The two halves of the workflow:

```bash
# Simulator side.
scxsim run -s lavd --cpus 4 --duration 1s \
    --structops-jsonl /tmp/sim.jsonl \
    examples/cpu_bound.json

# Kernel side.
scxsim vm-run -s lavd --cpus 4 --bpf-trace examples/cpu_bound.json
# Produces bpf_trace.log in CWD; convert to JSONL via the
# bpftrace-side script, then diff:
scripts/compare_live_vs_scxsim_calls.sh /tmp/sim.jsonl bpf_trace.log
```

A clean diff confirms scxsim's fidelity for this workload + this
scheduler. A non-empty diff is a scxsim debt — file under the
`twin-design-principles` umbrella.

## What `vm-run` is *not*

- **Not a simulator.** Outputs depend on real wallclock, real ASLR,
  real interrupts; runs are not byte-deterministic across invocations.
- **Not faster than the simulator.** A VM run is the wallclock cost
  of running the workload, plus VM startup overhead. Use `vm-run`
  for fidelity checks, not for iteration speed.
- **Not a replacement for `scxsim run`.** They cover complementary
  modes. The recommended loop is "iterate under `run`, validate
  with `vm-run`."
