# Dockerfile for scxsim — a deterministic discrete-event simulator
# for sched_ext schedulers. Used by README "Quick start (Docker)".
#
# Build (from the scx-sim/ directory):
#     docker build -t scxsim .
# Run the hello example:
#     docker run --rm scxsim
# Run a different example with the lavd scheduler and capture a trace:
#     docker run --rm -v /tmp:/out scxsim \
#         run -s lavd --cpus 4 --duration 100ms \
#             --perfetto /out/cpu_bound.json examples/cpu_bound.json
#     # then upload /tmp/cpu_bound.json to https://ui.perfetto.dev/
#
# Single-stage on purpose: the scxsim release binary embeds the
# absolute path to its scheduler `.so` directory (the OUT_DIR resolved
# at build time, see crates/scx_simulator/build.rs:218 setting
# SCHEDULER_SO_DIR). A naive multi-stage copy would leave the binary
# pointing at a path that does not exist in the runtime image, so we
# build and run in the same image with the source tree intact.

FROM ubuntu:24.04

# Pin to noninteractive so apt does not block on tzdata.
ENV DEBIAN_FRONTEND=noninteractive

# System deps mirror the README's "Install system dependencies (Ubuntu)"
# section. Keep in sync.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang llvm libelf-dev zlib1g-dev \
        build-essential xxd pkg-config \
        ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*

# Install rustup + a minimal stable rust toolchain.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /scxsim
# Expect the build context to be the scx-sim/ directory itself.
# The .dockerignore at scx-sim/.dockerignore strips target/, .git/,
# and other transient state to keep the build context small.
COPY . /scxsim

# Build the scxsim release binary. The Cargo build script (build.rs)
# also builds the scheduler .so libraries as a side effect, so a single
# `cargo build --release` is enough to produce a runnable image.
RUN cargo build --release -p scx_simulator --bin scxsim

# Symlink the release binary onto $PATH for ergonomic invocation.
RUN ln -s /scxsim/target/release/scxsim /usr/local/bin/scxsim

ENTRYPOINT ["scxsim"]
CMD ["run", "-s", "simple", "--cpus", "4", "--duration", "100ms", "examples/hello.json"]
