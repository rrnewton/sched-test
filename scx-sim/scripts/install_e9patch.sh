#!/bin/bash
# install_e9patch.sh — Clone and build e9patch into third_party/e9patch/
#
# Usage:
#   ./scripts/install_e9patch.sh
#   with-proxy ./scripts/install_e9patch.sh   # on Meta machines (git clone needs network)
#
# The script auto-detects the project root (directory containing this script's
# parent "scripts/" directory). It is safe to run from any working directory.

set -euo pipefail

# --- Locate project root ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

E9_DIR="$PROJECT_ROOT/third_party/e9patch"
E9_REPO="https://github.com/GJDuck/e9patch.git"

# --- Already installed? ---
if [ -x "$E9_DIR/e9tool" ] && [ -x "$E9_DIR/e9patch" ]; then
    echo "e9patch already installed at $E9_DIR"
    exit 0
fi

# --- Clone if needed ---
if [ ! -d "$E9_DIR" ]; then
    echo "Cloning e9patch into $E9_DIR ..."
    mkdir -p "$PROJECT_ROOT/third_party"
    git clone "$E9_REPO" "$E9_DIR"
else
    echo "e9patch directory exists but binaries missing; rebuilding ..."
fi

# --- Build ---
echo "Building e9patch ..."
cd "$E9_DIR"
./build.sh

# --- Verify ---
for bin in e9tool e9patch; do
    if [ ! -x "$E9_DIR/$bin" ]; then
        echo "ERROR: $bin not found after build" >&2
        exit 1
    fi
done

echo "e9patch installed successfully: $E9_DIR"
