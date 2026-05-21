# Installation

scxsim is a Rust workspace under [`scx-sim/`][scx-sim-tree]. Building
it produces a single binary, `scxsim`, plus a set of scheduler `.so`
files cached under `target/release/build/.../out/schedulers/`.

[scx-sim-tree]: https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim

## Prerequisites

| Tool | Purpose | Minimum |
|---|---|---|
| Rust toolchain | Compiling scxsim itself. Pinned by `rust-toolchain.toml`. | rustup, stable |
| `clang` | Compiling the in-tree BPF scheduler sources. | 15+ (18 recommended) |
| `bpftool` | Generating BPF skeleton headers consumed by the schedulers. | shipped with kernel-tools |
| `libelf` headers | BPF object handling. | `libelf-dev` |
| `libzstd` headers | BPF object handling. | `libzstd-dev` |
| `pkg-config` | Locating the above libraries during the build. | any |

Optional:

| Tool | Purpose |
|---|---|
| `e9patch` | Required for `--preempt-mode e9patch` (deterministic, debugger-compatible RBC). Install via [`scripts/install_e9patch.sh`][e9-install]. |
| `virtme-ng` (`vng`) | Required for the `vm-run` subcommand (real-kernel ground-truth comparison). |
| `bpftrace` | Required for `vm-run --bpf-trace`. |

[e9-install]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/scripts/install_e9patch.sh

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

From a fresh checkout of [`facebookexperimental/sched-test`][repo],
checkout the simulator branch and build the release binary:

```bash
git clone https://github.com/facebookexperimental/sched-test.git
cd sched-test
git checkout simulator.v6
cd scx-sim
cargo build --release -p scx_simulator --bin scxsim
```

[repo]: https://github.com/facebookexperimental/sched-test

The build invokes the BPF toolchain via the workspace `build.rs`, so
the first build takes 1–3 minutes; subsequent builds are incremental.
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

**2. `scxsim run --list-schedulers` confirms the five BPF schedulers
built and were cached:**

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
- **Slow first build.** Most of the time is BPF compilation, not Rust.
  Subsequent `cargo build` is fast (skeleton headers are cached); a
  full BPF rebuild only triggers when `scheds/` sources change.

See also the top-level [`scx-sim/README.md`][scxsim-readme] for the
authoritative install + build steps.

[scxsim-readme]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/README.md
