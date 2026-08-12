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

# Find mypy and its stub dependencies — install if missing.
#
# mypy and pip MUST be picked from the same environment: the stub check
# below asks $PIP_CMD whether pandas-stubs is installed, and that answer
# is only meaningful for the interpreter $MYPY_CMD actually runs under.
# Preferring a PATH `mypy` while probing `.venv/bin/pip` reports
# "stubs present" and then fails with "Library stubs not installed for
# pandas" — a silent env mismatch. So: if a .venv exists, it wins for
# BOTH; otherwise fall back to PATH for both.
MYPY_DEPS=(mypy pandas-stubs)
MYPY_CMD=""
PIP_CMD=""
if [ -d .venv ]; then
    PIP_CMD=".venv/bin/pip"
    if [ -x .venv/bin/mypy ]; then
        MYPY_CMD=".venv/bin/mypy"
    fi
else
    if command -v pip &>/dev/null; then
        PIP_CMD="pip"
    fi
    if command -v mypy &>/dev/null; then
        MYPY_CMD="mypy"
    fi
fi

HAVE_PANDAS_STUBS=false
if [ -n "$PIP_CMD" ] && $PIP_CMD show pandas-stubs &>/dev/null; then
    HAVE_PANDAS_STUBS=true
fi

if [ -z "$MYPY_CMD" ] || [ "$HAVE_PANDAS_STUBS" = false ]; then
    echo "mypy or required type stubs not found — installing..."
    if [ -n "$PIP_CMD" ]; then
        $PIP_CMD install "${MYPY_DEPS[@]}" >&2
        # Re-resolve from the SAME environment we just installed into.
        if [ "$PIP_CMD" = ".venv/bin/pip" ]; then
            MYPY_CMD=".venv/bin/mypy"
        else
            MYPY_CMD="mypy"
        fi
        if ! [ -x "$MYPY_CMD" ] && ! command -v "$MYPY_CMD" &>/dev/null; then
            echo "ERROR: installed ${MYPY_DEPS[*]} via $PIP_CMD but $MYPY_CMD is still not runnable." >&2
            exit 1
        fi
    else
        echo "ERROR: mypy/type stubs not found and no pip available to install them." >&2
        echo "  Install manually: pip install ${MYPY_DEPS[*]} (or .venv/bin/pip install ${MYPY_DEPS[*]})" >&2
        exit 1
    fi
fi

echo "=== Running Python type checks (mypy --strict) ==="
$MYPY_CMD --strict $PYTHON_FILES
echo "  mypy --strict passed."
