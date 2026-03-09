# Top-level Makefile for scx_simulator convenience targets
#
# The primary build system is Cargo. This Makefile provides shortcuts
# for benchmark and other auxiliary tasks.

# Use the venv python if available, otherwise system python3
PYTHON := $(or $(wildcard .venv/bin/python3),python3)

<<<<<<< HEAD
.PHONY: benchmark benchmark-perf
=======
.PHONY: benchmark install-e9patch
>>>>>>> 1b45a9c (Add e9patch install script and CI integration)

benchmark:
	$(PYTHON) scripts/benchmark.py run

<<<<<<< HEAD
benchmark-perf:
	$(PYTHON) scripts/benchmark.py run --perf --no-build
=======
install-e9patch:
	./scripts/install_e9patch.sh
>>>>>>> 1b45a9c (Add e9patch install script and CI integration)
