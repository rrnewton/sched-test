# Top-level Makefile for scx_simulator convenience targets
#
# The primary build system is Cargo. This Makefile provides shortcuts
# for benchmark and other auxiliary tasks.

# Use the venv python if available, otherwise system python3
PYTHON := $(or $(wildcard .venv/bin/python3),python3)

.PHONY: benchmark install-e9patch

benchmark:
	$(PYTHON) scripts/benchmark.py run

install-e9patch:
	./scripts/install_e9patch.sh
