#!/bin/bash
# typecheck.sh - Run mypy --strict on all Python files in the project.
# Can be run standalone or is called by validate.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

# Find all Python files (excluding venv and build dirs)
PYTHON_FILES=$(find . -name "*.py" -not -path "./.venv/*" -not -path "./target/*" -not -path "./debug/*" -not -path "./lldb_debug/*")
if [ -z "$PYTHON_FILES" ]; then
    echo "  No Python files found."
    exit 0
fi

# Resolve mypy and its stubs from ONE environment: the project venv.
#
# mypy and pip must come from the same place. The stub check below asks pip
# whether pandas-stubs is installed, and that answer is only meaningful for
# the interpreter mypy actually runs under. Preferring a PATH `mypy` while
# probing `.venv/bin/pip` reports "stubs present" and then fails with
# "Library stubs not installed for pandas" — a silent env mismatch.
#
# The venv is also the only portable place to install into: the system pip
# on Meta dev boxes refuses every install ("direct installs are not allowed
# on the Production system paths"), so auto-installing via PATH pip fails
# with a wall of unrelated text. Creating and populating the venv is
# `make install-deps`' job; this script only reports what is missing.
MYPY_CMD=".venv/bin/mypy"
PIP_CMD=".venv/bin/pip"

missing=()
[ -x "$MYPY_CMD" ] || missing+=(mypy)
{ [ -x "$PIP_CMD" ] && "$PIP_CMD" show pandas-stubs &>/dev/null; } || missing+=(pandas-stubs)

if [ ${#missing[@]} -gt 0 ]; then
    echo "ERROR: Python type-check dependencies missing from .venv: ${missing[*]}" >&2
    echo "  Install with:  make install-deps" >&2
    echo "  Or directly:   python3 -m venv .venv && .venv/bin/pip install mypy pandas-stubs" >&2
    echo "  (Checked $PWD/.venv — a PATH mypy is deliberately NOT used, because" >&2
    echo "   its stub set would not match the interpreter pip reports on.)" >&2
    exit 1
fi

echo "=== Running Python type checks (mypy --strict) ==="
$MYPY_CMD --strict $PYTHON_FILES
echo "  mypy --strict passed."
