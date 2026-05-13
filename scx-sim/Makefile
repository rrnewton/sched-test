# Top-level Makefile for scx_simulator convenience targets
#
# The primary build system is Cargo. This Makefile provides shortcuts
# for benchmark and other auxiliary tasks.

# Use the venv python if available, otherwise system python3
PYTHON := $(or $(wildcard .venv/bin/python3),python3)

.PHONY: benchmark benchmark-perf install-e9patch rebuild-schedulers

benchmark:
	$(PYTHON) scripts/benchmark.py run

benchmark-perf:
	$(PYTHON) scripts/benchmark.py run --perf --no-build

install-e9patch:
	./scripts/install_e9patch.sh

# Rebuild every scheduler `.so` from current source.
#
# This is the OFFICIAL action for "I just swapped scx submodule SHAs (or
# edited scheduler source) and want to make sure scxsim picks up the
# change." After build.rs's `cargo:rerun-if-changed=` was extended to
# cover the relevant scx subtrees (see build.rs comment), normal
# `cargo build` already does the right thing on a SHA swap. This target
# is the discoverable entrypoint that codifies that — and forcibly
# re-runs the build script as a defensive belt-and-suspenders for cases
# where a stale on-disk cache predates the build.rs fix, where the
# `cargo:rerun-if-changed=` mtime tracking is defeated by an unusual
# checkout pattern, or where the user wants an obviously-fresh build to
# be CERTAIN of a known-good `.so`.
#
# Idempotent: re-running with no source changes is a fast no-op.
#
# Usage:
#   make rebuild-schedulers           # re-run build script + cargo build
#   make rebuild-schedulers SCX_SHA=<sha>  # check out SHA in scx submodule first
#
# After the build, the resulting `.so` paths and sha256 are printed so
# the user can verify a SHA swap actually produced a different binary.
rebuild-schedulers:
	@if [ -n "$(SCX_SHA)" ]; then \
	  echo ">>> Checking out scx submodule at $(SCX_SHA)"; \
	  git -C ../scx checkout --quiet --detach $(SCX_SHA); \
	fi
	@echo ">>> Touching build.rs to force build-script re-run"
	@touch crates/scx_simulator/build.rs
	@echo ">>> cargo build --release -p scx_simulator --bin scxsim"
	@cargo build --release -p scx_simulator --bin scxsim
	@echo ""
	@echo ">>> Built scheduler .so files:"
	@find target/release/build -name 'libscx_*.so' -not -path '*-uninstrumented*' \
	  -printf '%p\n' | sort | xargs -r sha256sum
