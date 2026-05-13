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

# Find mypy and its stub dependencies — install if missing
MYPY_DEPS=(mypy pandas-stubs)
MYPY_CMD=""
PIP_CMD=""
if command -v mypy &>/dev/null; then
    MYPY_CMD="mypy"
elif [ -x .venv/bin/mypy ]; then
    MYPY_CMD=".venv/bin/mypy"
fi
if [ -d .venv ]; then
    PIP_CMD=".venv/bin/pip"
elif command -v pip &>/dev/null; then
    PIP_CMD="pip"
fi

HAVE_PANDAS_STUBS=false
if [ -n "$PIP_CMD" ] && $PIP_CMD show pandas-stubs &>/dev/null; then
    HAVE_PANDAS_STUBS=true
fi

if [ -z "$MYPY_CMD" ] || [ "$HAVE_PANDAS_STUBS" = false ]; then
    echo "mypy or required type stubs not found — installing..."
    if [ -n "$PIP_CMD" ]; then
        $PIP_CMD install "${MYPY_DEPS[@]}" >&2
        if [ -x .venv/bin/mypy ]; then
            MYPY_CMD=".venv/bin/mypy"
        else
            MYPY_CMD="mypy"
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
