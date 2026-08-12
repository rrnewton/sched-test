# Top-level Makefile for scx_simulator convenience targets
#
# The primary build system is Cargo. This Makefile provides shortcuts
# for benchmark and other auxiliary tasks.

# Use the venv python if available, otherwise system python3
PYTHON := $(or $(wildcard .venv/bin/python3),python3)

.PHONY: benchmark benchmark-perf install-e9patch rebuild-schedulers \
        test-examples check-deps install-deps

# Report every external dependency scx-sim needs, where it was found, and
# for anything missing, a copy-pasteable install command. Exits non-zero if
# a REQUIRED dependency is missing. validate.sh runs this first, so a
# missing dependency produces an explicit message instead of a confusing
# downstream failure. See scripts/deps.sh for the dependency table.
check-deps:
	@./scripts/deps.sh check

# Install everything scx-sim needs that can be installed without root.
# Anything that genuinely needs a system package manager is printed as a
# command to run rather than run automatically.
install-deps:
	@./scripts/deps.sh install

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

# Run every `examples/*.json` workload through scxsim and assert exit
# code 0. Acts as the guide's example-rot guard: if a CLI rename, an
# rt-app schema change, or a scheduler default shift breaks a config a
# guide reader is about to copy, this target is the first thing to go
# red. See `docs/guide/tests/test_examples.sh` for the full script
# (including tunable env vars such as SCXSIM_TEST_SCHEDULERS).
#
# The script auto-builds scxsim if `target/release/scxsim` is absent,
# so this target works on a fresh clone without a separate `cargo
# build` step. To re-use an already-built binary at a non-default
# location, override SCXSIM, e.g.:
#   make test-examples SCXSIM=/path/to/scxsim
#
# To widen the scheduler matrix:
#   make test-examples SCXSIM_TEST_SCHEDULERS="simple lavd"
test-examples:
	@bash docs/guide/tests/test_examples.sh
