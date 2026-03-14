# Top-level Makefile for scx_simulator convenience targets
#
# The primary build system is Cargo. This Makefile provides shortcuts
# for benchmark and other auxiliary tasks.

# Use the venv python if available, otherwise system python3
PYTHON := $(or $(wildcard .venv/bin/python3),python3)

.PHONY: benchmark benchmark-perf

benchmark:
	$(PYTHON) scripts/benchmark.py run

benchmark-perf:
	$(PYTHON) scripts/benchmark.py run --perf --no-build
