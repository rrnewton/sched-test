# Installation

> **Status — stub.** This page will cover rustup setup, scx-sim
> workspace prerequisites (clang, bpftool, libelf, libzstd), and
> step-by-step build instructions for the `scxsim` binary.

## Prerequisites (TODO)

- Rust toolchain via [rustup](https://rustup.rs/) (matching the workspace's
  `rust-toolchain.toml`).
- `clang` 15+ for BPF compilation.
- `bpftool` for skeleton generation.
- `libelf-dev`, `libzstd-dev` for BPF object handling.
- (Optional) `e9patch` for `--preempt-mode e9patch`; see
  [`scripts/install_e9patch.sh`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/scripts/install_e9patch.sh).

## Building from source (TODO)

```bash
cd scx-sim
cargo build --release -p scx_simulator --bin scxsim
ls target/release/scxsim
```

## Verifying the install (TODO)

```bash
scxsim --help
scxsim run --list-schedulers
```

The latter should print the five schedulers (`simple`, `lavd`,
`cosmos`, `mitosis`, `tickless`) and the path to each `libscx_<name>.so`.

See also: the top-level [`scx-sim/README.md`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/README.md)
for the canonical install steps.
