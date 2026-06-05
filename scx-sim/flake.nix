{
  # Nix flake for scxsim — gives a one-command, hermetic dev shell with
  # every system dep the README's "Step-by-step build" lists (clang,
  # llvm, libelf, zlib, pkg-config, build tools) plus a stable Rust
  # toolchain. Use it instead of `apt-get install` on machines where the
  # package manager isn't available or convenient (NixOS, macOS via
  # nix-darwin, dev VMs, …).
  #
  # Quick start (from this directory):
  #
  #     nix develop
  #     cargo build --release -p scx_simulator --bin scxsim
  #     ./target/release/scxsim run -s simple --cpus 4 --duration 100ms \
  #         examples/hello.json
  #
  # One-liner (no clone needed — flake fetches the repo itself):
  #
  #     nix develop github:rrnewton/sched-test?dir=scx-sim \
  #         --command bash -c '
  #           git submodule update --init --recursive &&
  #           cd scx-sim &&
  #           cargo build --release -p scx_simulator --bin scxsim &&
  #           ./target/release/scxsim run -s simple --cpus 4 \
  #               --duration 100ms examples/hello.json'
  #
  # (Sched-ext's scheduler `.so` files require the `scx` submodule, so
  # the flake itself cannot package scxsim as a self-contained
  # `nix run` target — `build.rs` reaches into ../scx for headers.
  # The dev-shell approach above is the supported path.)
  description = "scx_simulator — deterministic discrete-event simulator for sched_ext schedulers";

  # Pin nixpkgs by SHA, not by channel name. `nixos-unstable` would
  # work for *modern* nix (2.18+) via the flake.lock, but
  # cachix/install-nix-action used in CI sometimes resolves channel
  # names against the runner's NIX_PATH and silently downgrades to an
  # older nixpkgs (we observed it picking 24.05 / rustc 1.77 even with
  # flake.lock pointing at unstable, which then fails on the Cargo.lock
  # v4 / rustc-1.88-required combo). A pinned SHA bypasses that
  # entirely. SHA below is nixos-unstable from 2026-05-31, giving
  # clang 21.1.8 + rustc 1.95.0. Bump when the workspace MSRV bumps.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/331800de5053fcebacf6813adb5db9c9dca22a0c";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        devShells.default = pkgs.mkShell {
          # Build dependencies. Mirrors scx-sim/README.md "Install
          # system dependencies" + .github/workflows/scxsim-examples.yml.
          # Keep in sync with both — if a new dep lands in CI, add it here.
          nativeBuildInputs = with pkgs; [
            # Rust toolchain. Pin via `rustup` if you want exact-version
            # match with rust-toolchain.toml; the nixpkgs `rustc` /
            # `cargo` here track a recent stable.
            rustc
            cargo
            rustfmt
            clippy

            # BPF scheduler code is compiled as userspace C, so we need
            # clang (the default value of BPF_CLANG in build.rs:35).
            clang
            llvm

            # Linker / build glue.
            pkg-config

            # libbpf-sys consumers want libelf + zlib headers.
            elfutils
            zlib

            # xxd is used during the build (csrc generators).
            xxd

            # Git for submodule init; users without nix-managed git can
            # skip this, but it makes `nix develop` self-sufficient.
            git
          ];

          # Make `BPF_CLANG` point at the nixpkgs clang explicitly so
          # build.rs doesn't pick up a system clang of unknown version.
          BPF_CLANG = "${pkgs.clang}/bin/clang";

          shellHook = ''
            echo "scxsim dev shell — clang=$(clang --version | head -1)"
            echo "                   rustc=$(rustc --version)"
            echo ""
            echo "Build:  cargo build --release -p scx_simulator --bin scxsim"
            echo "Run:    ./target/release/scxsim run -s simple --cpus 4 \\"
            echo "            --duration 100ms examples/hello.json"
          '';
        };
      });
}
