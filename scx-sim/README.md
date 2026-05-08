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

## Quick start

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
git clone <repo-url>
cd <repo>/scx-sim
git submodule update --init --recursive
```

### 4. Build

```bash
cargo build --workspace
```

### 5. Build schedulers

```bash
make -C schedulers
```

### 6. Run a simulation

```bash
cargo run --release -- run -s simple workloads/two_runners.json
```

## Usage

```
scxsim run [OPTIONS] [WORKLOAD]
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

## Documentation

- **[CLAUDE.md](CLAUDE.md)** — Development guidelines, coding conventions,
  system dependency details, and workflow instructions.
- **[OPTIMIZATION.md](OPTIMIZATION.md)** — Performance patterns and
  benchmarking methodology.

## License

See the repository root for license information.
