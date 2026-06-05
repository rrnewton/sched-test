# scx_simulator (scxsim)

A deterministic, discrete-event simulator for
[sched_ext](https://github.com/sched-ext/scx) schedulers. It runs real
scheduler code (compiled as shared libraries) against synthetic workloads
without requiring a Linux kernel, enabling fast iteration, reproducible bug
finding, and regression testing.

## Key features

- **Deterministic execution** — seeded PRNG controls all nondeterminism (tick
  jitter, context-switch noise, event tiebreaking). Same seed = same results.
- **Real scheduler code** — BPF schedulers (LAVD, Mitosis, Cosmos, etc.) are
  compiled as userspace `.so` libraries and loaded at runtime.
- **rt-app workloads** — JSON workload specs describe tasks, phases, CPU
  affinity, and timing, compatible with the rt-app format.
- **Trace replay** — record preemption traces and replay them deterministically.
- **Perfetto integration** — export simulation timelines for visualization.
- **Hardware breakpoint support** — replay mode uses PMU counters for exact
  preemption point reproduction.

## Quick start (Docker)

The fastest way to try scxsim end-to-end. Requires only Docker on the
host — no Rust toolchain, no system build dependencies (clang, libelf,
…). The image installs everything internally and bakes in the
example workloads.

```bash
git clone --recursive https://github.com/rrnewton/sched-test.git
cd sched-test/scx-sim
docker build -t scxsim .
docker run --rm scxsim
```

(`--recursive` initialises the `scx` submodule which the scheduler
build needs. If you already cloned without it: `git submodule update
--init --recursive`.)

That last command runs `examples/hello.json` against the `simple`
scheduler for 100 ms of simulated time and prints a per-CPU
structop / kfunc summary. Replace the default with any example —
including capturing a Perfetto trace for visualization:

```bash
docker run --rm -v /tmp:/out scxsim \
    run -s lavd --cpus 4 --duration 100ms \
        --perfetto /out/cpu_bound.json examples/cpu_bound.json
# upload /tmp/cpu_bound.json to https://ui.perfetto.dev/
```

See [`examples/`](examples/) for the full set of runnable workloads
and the [scxsim guide](docs/guide/src/getting-started/quick-start.md)
for next steps.

> The Dockerfile is single-stage on purpose — the release binary embeds
> the absolute path to its scheduler `.so` directory (`SCHEDULER_SO_DIR`,
> resolved at build time), so the image must keep the source tree in
> place. For a real development environment, follow the
> "[Step-by-step build](#step-by-step-build)" instructions below
> instead.

## Quick start (Nix)

If [Nix](https://nixos.org/download/) (with flakes) is already
installed, `flake.nix` provides a dev shell with every system dep
(clang, libelf, zlib, pkg-config, Rust) so you can skip
`apt-get install` entirely:

```bash
git clone --recursive https://github.com/rrnewton/sched-test.git
cd sched-test/scx-sim
nix develop
# inside the dev shell:
cargo build --release -p scx_simulator --bin scxsim
./target/release/scxsim run -s simple --cpus 4 --duration 100ms \
    examples/hello.json
```

The flake intentionally exposes a `devShell` only (not a buildable
package) — the same reason the Dockerfile is single-stage. See the
[guide's installation page][guide-install] for the full discussion
plus a `nix develop github:...` one-liner that fetches the flake
without a manual clone.

[guide-install]: docs/guide/src/getting-started/installation.md#quick-start-nix

## Step-by-step build

For local development or when Docker is not available.

### 1. Install system dependencies

**Ubuntu / Debian:**

```bash
sudo apt-get install -y clang llvm libelf-dev zlib1g-dev \
    build-essential xxd pkg-config
```

**Fedora / RHEL:**

```bash
sudo dnf install clang llvm elfutils-libelf-devel zlib-devel \
    gcc make vim-common pkgconf-pkg-config
```

### 2. Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 3. Clone and initialize submodules

```bash
git clone https://github.com/rrnewton/sched-test.git
cd sched-test
git submodule update --init --recursive
cd scx-sim
```

### 4. Build

```bash
cargo build --release -p scx_simulator --bin scxsim
```

The build script also compiles the scheduler `.so` libraries
(`libscx_simple.so`, `libscx_lavd.so`, …) as a side effect — no
separate `make -C schedulers` step is required.

### 5. Run a simulation

```bash
target/release/scxsim run -s simple --cpus 4 --duration 100ms \
    examples/hello.json
```

Or try one of the other guide examples:

```bash
target/release/scxsim run -s lavd --cpus 4 --duration 200ms \
    --perfetto /tmp/cpu_bound.json examples/cpu_bound.json
```

See [`examples/README.md`](examples/README.md) for the full inventory.

## Usage

```
scxsim simulate [OPTIONS] [WORKLOAD]
```

| Option | Default | Description |
|--------|---------|-------------|
| `-s, --scheduler` | `simple` | Scheduler to load (`simple`, `lavd`, `mitosis`, `cosmos`, `tickless`) |
| `-c, --cpus` | `4` | Number of simulated CPUs |
| `--smt` | `1` | SMT threads per core |
| `--seed` | `42` | PRNG seed (integer or `"entropy"`) |
| `--end-time` *(alias `--duration`)* | — | Simulation duration (e.g., `100ms`, `1s`) |
| `--watchdog-timeout` *(alias `--watchdog`)* | `30s` | Stall-detection timeout (`0`/`off` to disable) |
| `--config <PATH>` | — | TOML scheduler-config sidecar (typed BPF-global writes; see below) |
| `--fixed-priority` | off | Deterministic insertion-order tiebreaking |

### Exit codes

`scxsim run` maps each simulator `ExitKind` to a stable per-variant
process exit code. Automation should discriminate on these codes plus
the matching stable single-line stderr marker, NOT on the summary text:

| Variant                              | Exit | Stderr marker                                                     |
|--------------------------------------|-----:|-------------------------------------------------------------------|
| `Normal`                             |    0 | (none)                                                            |
| Generic CLI/IO error                 |    1 | `error: <msg>`                                                    |
| `ErrorStall { pid, runnable_for_ns}` |   42 | `scxsim: ExitKind::ErrorStall pid=<N> runnable_for_ns=<N>`        |
| `ErrorBpf(msg)`                      |   43 | `scxsim: ExitKind::ErrorBpf <msg>`                                |
| `ErrorDispatchLoopExhausted`         |   44 | `scxsim: ExitKind::ErrorDispatchLoopExhausted cpu=<N>`            |
| `ErrorCgroupExhausted`               |   45 | `scxsim: ExitKind::ErrorCgroupExhausted cgroup_name=… …`          |

### Scheduler-config TOML (`--config`)

A TOML sidecar declares per-symbol BPF-global writes to apply to the
loaded scheduler `.so` immediately after load. Sub-tables segregate
symbols by primitive type because the FFI layer cannot introspect ELF
symbol types — the caller must declare `T`:

```toml
[scheduler.bool_globals]
enable_cpu_bw = true

[scheduler.u8_globals]
# (none for this example)

[scheduler.u32_globals]
# some_threshold = 1024

[scheduler.u64_globals]
# period_ns = 1_000_000_000
```

A missing symbol in the loaded `.so` is a hard error so config typos are
loud rather than silent no-ops. Type mismatches are undefined behavior
(the caller is asserting `T` matches the ELF declaration).

Worked example — the Bug-1 canonical reproducer:

```bash
scxsim run crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json \
           --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
           --watchdog 80ms -s lavd --cpus 4 --duration 500ms
```

deterministically exits 42 with
`scxsim: ExitKind::ErrorStall pid=1 runnable_for_ns=80000793`.

```
scxsim vm-run [OPTIONS] <WORKLOAD>
```

| Option | Default | Description |
|--------|---------|-------------|
| `-s, --scheduler` | `simple` | Scheduler binary to run (`scx_simple`, `scx_lavd`, etc.) |
| `-c, --cpus` | `4` | Number of workload CPUs; tracing adds one VM CPU for the tracer |
| `--wprof` | off | Record a Perfetto trace using wprof |
| `--bpf-trace` | off | Record scheduler ops and kfunc calls using bpftrace |

```
scxsim replay <TRACE_FILE>
```

Replays a previously recorded preemption trace for deterministic reproduction.

## Available schedulers

| Scheduler | Description |
|-----------|-------------|
| `simple` | Minimal round-robin scheduler for testing |
| `lavd` | Latency-Aware Virtual Deadline scheduler |
| `mitosis` | Cgroup-aware scheduler with task migration |
| `cosmos` | Multi-domain scheduler |
| `tickless` | Tickless/event-driven scheduler |

## Project structure

```
scx-sim/
├── crates/
│   ├── scx_simulator/    # Core simulation engine
│   ├── scx_perf/         # PMU / hardware breakpoint support
│   └── scx_cgroup_tree/  # Cgroup hierarchy modeling
├── csrc/                 # C support code (task structs, SDT stubs)
├── schedulers/           # Scheduler source and build system
├── workloads/            # Example rt-app JSON workload specs
├── scripts/              # Benchmarking and utility scripts
├── lldb_debug/           # lldb data formatters + bug1_diagnose helper (see Debugging)
├── CLAUDE.md             # Development guidelines (for contributors and AI agents)
└── OPTIMIZATION.md       # Performance optimization guide
```

## Testing

```bash
# Run the full validation suite (unit tests, integration tests, lints)
./validate.sh

# Run unit tests only
cargo nextest run        # or: cargo test
```

## Debugging

scxsim runs as a normal userspace process, so any debugger that handles
Rust release binaries works. We ship a small set of LAVD-focused lldb
data formatters and a `bug1_diagnose` custom command under
[`lldb_debug/`](lldb_debug/README.md). They were authored for the
cgroup-bandwidth runnable-task stall (cpu-bw-stall-bug / H6 path) but
are reusable for any LAVD investigation that needs pretty-printed
`SimTask`, `BandwidthManager`, `DsqId`, etc.

End-to-end demo (uses the canonical Bug-1 reproducer):

```bash
./scx-sim/lldb_debug/worked_example.sh
```

See [`lldb_debug/README.md`](lldb_debug/README.md) for the full
formatter inventory, usage in interactive sessions, and known
limitations.

## Documentation

- **[scxsim guide](docs/guide/src/SUMMARY.md)** (mdbook source) —
  full conceptual + how-to documentation: getting started, running
  simulations, recipes (compare schedulers, verify determinism,
  reproduce a stall, lldb), architecture overview, and CLI reference.
  Render locally with `make -C docs/guide serve`.
- **[examples/README.md](examples/README.md)** — runnable rt-app
  workloads referenced from the guide.
- **[CLAUDE.md](CLAUDE.md)** — Development guidelines, coding conventions,
  system dependency details, and workflow instructions.
- **[OPTIMIZATION.md](OPTIMIZATION.md)** — Performance patterns and
  benchmarking methodology.
- **[lldb_debug/README.md](lldb_debug/README.md)** — lldb data formatters
  and `bug1_diagnose` command for debugging LAVD scheduler state inside
  scxsim.

## License

See the repository root for license information.
