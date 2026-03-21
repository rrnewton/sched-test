#!/bin/bash
# typecheck.sh - Run mypy --strict on all Python files in the project.
# Can be run standalone or is called by validate.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

# Find all Python files (excluding venv and build dirs)
PYTHON_FILES=$(find . -name "*.py" -not -path "./.venv/*" -not -path "./target/*" -not -path "./debug/*")
if [ -z "$PYTHON_FILES" ]; then
    echo "  No Python files found."
    exit 0
fi

# Find mypy — install if missing
MYPY_CMD=""
if command -v mypy &>/dev/null; then
    MYPY_CMD="mypy"
elif [ -x .venv/bin/mypy ]; then
    MYPY_CMD=".venv/bin/mypy"
fi

if [ -z "$MYPY_CMD" ]; then
    echo "mypy not found — installing..."
    if [ -d .venv ]; then
        .venv/bin/pip install mypy >&2
        MYPY_CMD=".venv/bin/mypy"
    elif command -v pip &>/dev/null; then
        pip install mypy >&2
        MYPY_CMD="mypy"
    else
        echo "ERROR: mypy not found and no pip available to install it." >&2
        echo "  Install manually: pip install mypy (or .venv/bin/pip install mypy)" >&2
        exit 1
    fi
fi

echo "=== Running Python type checks (mypy --strict) ==="
$MYPY_CMD --strict $PYTHON_FILES
echo "  mypy --strict passed."
