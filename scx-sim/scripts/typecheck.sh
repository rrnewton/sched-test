#!/bin/bash
# typecheck.sh - Run mypy --strict on all Python files via uvx (ephemeral,
# uv-cached: no project .venv, no pip install). Run standalone or via validate.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

# Find all Python files (excluding venv and build dirs)
PYTHON_FILES=$(find . -name "*.py" -not -path "./.venv/*" -not -path "./target/*" -not -path "./debug/*" -not -path "./lldb_debug/*")
if [ -z "$PYTHON_FILES" ]; then
    echo "  No Python files found."
    exit 0
fi

if ! command -v uvx &>/dev/null; then
    echo "ERROR: uvx (uv) not found — required to run the mypy typecheck." >&2
    echo "  Install uv: curl -LsSf https://astral.sh/uv/install.sh | sh" >&2
    exit 1
fi

echo "=== Running Python type checks (uvx mypy --strict) ==="
# uvx runs mypy in an ephemeral, uv-cached environment — no project .venv and no
# pip install. --with pandas-stubs: benchmark.py imports pandas (needs stubs under
# --strict). $PYTHON_FILES is intentionally unquoted (word-split into one arg per file).
uvx --with pandas-stubs mypy --strict $PYTHON_FILES
echo "  mypy --strict passed."
