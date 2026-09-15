# Installation

scxsim is a Rust workspace under [`scx-sim/`][scx-sim-tree]. Building
it produces a single binary, `scxsim`, plus a set of scheduler `.so`
files cached under `target/release/build/.../out/schedulers/`.

[scx-sim-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim

If you only want to *try* scxsim, the two quick starts below skip the
system-dependency dance entirely. Either path produces a working
`scxsim` plus the bundled `examples/`, and runs the hello workload
end-to-end. Sections further down cover hand-rolled installs on
Ubuntu / Debian / Fedora, plus optional add-ons like `e9patch` and
`virtme-ng`.

## Quick start: Docker

Zero system deps beyond Docker itself. The image bakes in clang,
Rust, libelf, and the example workloads, so the first run is the only
slow step (1–3 minutes — most of it apt + rustup downloads, then a
release-mode cargo build):

```bash
git clone --recursive https://github.com/rrnewton/sched-test.git
cd sched-test
docker build -t scxsim -f scx-sim/Dockerfile .
docker run --rm scxsim
```

(The build context is the repo root — not `scx-sim/` — because the
simulator's `build.rs` reaches up into `lib/scxtest`, `scheds/`, and
the `scx/` submodule when compiling the BPF scheduler C sources.)

The final command runs `examples/hello.json` against the `simple`
scheduler for 100 ms of simulated time. The last line should read:

```text
local_dsq_dispatches: 5
```

and exit code is 0. To run a different example — for instance, the
LAVD scheduler against `examples/cpu_bound.json` while capturing a
Perfetto trace:

```bash
docker run --rm -v /tmp:/out scxsim \
    run -s lavd --cpus 4 --duration 100ms \
        --perfetto /out/cpu_bound.json examples/cpu_bound.json
# Then drop /tmp/cpu_bound.json onto https://ui.perfetto.dev/
```

The image is intentionally single-stage: the release binary embeds
the absolute path to its scheduler `.so` directory (resolved at
`cargo build` time — see [`crates/scx_simulator/build.rs`][build-rs]),
so a multi-stage copy would leave the binary pointing at a directory
that doesn't exist in the runtime layer. For day-to-day development,
prefer the [Nix dev shell](#quick-start-nix) or [a native
install](#building-from-source) below.

[build-rs]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/build.rs

## Quick start: Nix

If you already have [Nix][nix-install] (with flakes enabled), one
command drops you into a shell with the right clang, Rust, libelf,
zlib, pkg-config, and friends — no `apt-get` or `dnf` required. The
flake itself lives at [`scx-sim/flake.nix`][flake-nix].

```bash
git clone --recursive https://github.com/rrnewton/sched-test.git
cd sched-test/scx-sim
nix develop
# inside the dev shell:
cargo build --release -p scx_simulator --bin scxsim
./target/release/scxsim run -s simple --cpus 4 --duration 100ms \
    examples/hello.json
```

Same expected last line as Docker: `local_dsq_dispatches: 5`, exit
code 0.

To run a one-liner without cloning manually (Nix fetches the flake
itself; you still need the `scx` submodule init afterward):

```bash
nix develop github:rrnewton/sched-test?dir=scx-sim --command bash -c '
    git submodule update --init --recursive &&
    cd scx-sim &&
    cargo build --release -p scx_simulator --bin scxsim &&
    ./target/release/scxsim run -s simple --cpus 4 --duration 100ms \
        examples/hello.json'
```

The flake exposes a `devShells.default` only — not a `packages.default`
— because `build.rs` reaches into `../scx` for BPF headers and the
release binary embeds an absolute `SCHEDULER_SO_DIR`, which a hermetic
`nix-build` wouldn't preserve at runtime without extra wrapping. The
dev shell is the supported path.

Both quick starts are exercised on every push by the
[`scxsim-quickstart`][qs-workflow] GitHub Actions workflow; if either
breaks, expect a red check on the next push to `simulator.v6`.

[nix-install]: https://nixos.org/download/
[flake-nix]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/flake.nix
[qs-workflow]: https://github.com/rrnewton/sched-test/blob/simulator.v6/.github/workflows/scxsim-quickstart.yml

## Prerequisites

| Tool | Purpose | Minimum |
|---|---|---|
| Rust toolchain | Compiling scxsim itself. Pinned by `rust-toolchain.toml`. | rustup, stable |
| `clang` | Compiling the in-tree scheduler C sources. Note: scxsim invokes clang with a **native** target (NOT `--target=bpf`), so the schedulers are built as ordinary native shared libraries — see [Introduction](../introduction.md). | 15+ (18 recommended) |
| `bpftool` | Generating Berkeley Packet Filter (BPF) skeleton headers that the scheduler C sources `#include`; needed even for native builds because the C source references the skeleton layout. | shipped with kernel-tools |
| `libelf` headers | ELF object-file handling (used by `libbpf-sys` build deps). | `libelf-dev` |
| `libzstd` headers | Zstandard compression headers (used by `libbpf-sys` build deps). | `libzstd-dev` |
| `pkg-config` | Locating the above libraries during the build. | any |

Optional:

| Tool | Purpose |
|---|---|
| `e9patch` | Required for `--preempt-mode e9patch` (deterministic, debugger-compatible RBC). Install via [`scripts/install_e9patch.sh`][e9-install]. |
| `virtme-ng` (`vng`) | Required for the `vm-run` subcommand (real-kernel ground-truth comparison). |
| `bpftrace` | Required for `vm-run --bpf-trace`. |

[e9-install]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/scripts/install_e9patch.sh

On Debian / Ubuntu, the apt-installable prerequisites are roughly:

```bash
sudo apt-get install -y \
    clang-18 llvm-18 \
    linux-tools-common linux-tools-generic \
    libelf-dev zlib1g-dev libzstd-dev \
    pkg-config build-essential
```

The Rust toolchain comes from [rustup][rustup]:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

[rustup]: https://rustup.rs/

## Building from source

From a fresh checkout of [`rrnewton/sched-test`][repo],
checkout the simulator branch and build the release binary:

```bash
git clone https://github.com/rrnewton/sched-test.git
cd sched-test
git checkout simulator.v6
cd scx-sim
cargo build --release -p scx_simulator --bin scxsim
```

[repo]: https://github.com/rrnewton/sched-test

The build invokes `clang` (with a native target, not the BPF target)
on each scheduler's C sources via the workspace `build.rs`, so the
first build takes 1–3 minutes; subsequent builds are incremental.
The resulting binary lands at `scx-sim/target/release/scxsim`.

Add it to `$PATH` (optional but expected in the rest of the guide):

```bash
export PATH="$PWD/target/release:$PATH"
```

## Verifying the install

Three smoke checks confirm the build is functional.

**1. `scxsim --help` parses CLI metadata:**

```bash
scxsim --help
```

```text
sched_ext simulator

Usage: scxsim [OPTIONS] <COMMAND>

Commands:
  run     Run a simulation from an rt-app workload
  vm-run  Run workload in a virtme-ng VM with a real scheduler
  replay  Replay a recorded preemption trace
  help    Print this message or the help of the given subcommand(s)
```

**2. `scxsim run --list-schedulers` confirms the five sched_ext
schedulers (native-compiled `.so` libraries) built and were cached:**

```bash
scxsim run --list-schedulers
```

```text
cosmos     /...target/release/build/scx_simulator-.../out/schedulers/libscx_cosmos.so
lavd       /...target/release/build/scx_simulator-.../out/schedulers/libscx_lavd.so
mitosis    /...target/release/build/scx_simulator-.../out/schedulers/libscx_mitosis.so
simple     /...target/release/build/scx_simulator-.../out/schedulers/libscx_simple.so
tickless   /...target/release/build/scx_simulator-.../out/schedulers/libscx_tickless.so
```

The five schedulers are documented at
[Concepts → Schedulers](../concepts/schedulers.md).

**3. End-to-end run of the bundled hello-world workload:**

```bash
scxsim run -s lavd --cpus 4 --duration 100ms examples/hello.json
```

The expected last line is `local_dsq_dispatches: 5` (the workload's
five loop iterations dispatched cleanly) and exit code 0. See
[Quick Start](./quick-start.md) for the full output and what each
field means.

## Re-execution and ASLR

The first thing scxsim does on startup is print

```text
scxsim: disabling ASLR and re-executing...
```

and re-exec itself. This is intentional: stable `.so` base addresses
are required for deterministic replay (`scxsim replay` and the
`bin_cache` regression-bisect workflow). Pass `--no-disable-aslr` to
opt out — useful when wrapping `scxsim` in a script that itself sets
process attributes you do not want clobbered, but at the cost of
losing the ASLR-stability guarantee.

## Troubleshooting build failures

- **`clang` missing or too old.** scx schedulers require clang 15+.
  Distros sometimes ship clang 14; install clang 18 explicitly and
  point `CC=clang-18` at it.
- **`linux/bpf.h` not found.** Install kernel headers
  (`linux-headers-$(uname -r)` on Debian / Ubuntu).
- **`bpftool` not on `$PATH`.** Either install `linux-tools-generic`
  or build bpftool from source; some distros only ship a kernel-tied
  variant.
- **Slow first build.** Most of the time is clang-ing the scheduler
  C sources to native `.so` libraries, not Rust. Subsequent
  `cargo build` is fast (skeleton headers are cached); a full
  scheduler-`.so` rebuild only triggers when `scheds/` sources change.

See also the top-level [`scx-sim/README.md`][scxsim-readme] for the
authoritative install + build steps.

[scxsim-readme]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/README.md
