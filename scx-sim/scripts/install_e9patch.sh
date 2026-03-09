#!/bin/bash
# install_e9patch.sh — Clone and build e9patch into third_party/e9patch/
#
# Usage:
#   ./scripts/install_e9patch.sh
#   with-proxy ./scripts/install_e9patch.sh   # on Meta machines (git clone needs network)
#
# The script auto-detects the project root (directory containing this script's
# parent "scripts/" directory). It is safe to run from any working directory.
#
# We build e9patch directly (make release) rather than using upstream's
# build.sh, because build.sh checks for `markdown` which is only needed for
# `make install` (HTML doc generation), not for building the binaries.

set -euo pipefail

# --- Locate project root ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

E9_DIR="$PROJECT_ROOT/third_party/e9patch"
E9_REPO="https://github.com/GJDuck/e9patch.git"

# --- Dependency checks ---
# These mirror what e9patch's build.sh requires, minus `markdown` which is only
# needed for `make install` (HTML doc generation). We only run `make release`.

check_cmd() {
    if command -v "$1" >/dev/null 2>&1; then
        return 0
    fi
    MISSING_CMDS+=("$1 (install: sudo apt-get install $2)")
}

check_hdr() {
    if echo "#include <$1>" | gcc -E - >/dev/null 2>&1; then
        return 0
    fi
    MISSING_HDRS+=("$1 (install: sudo apt-get install $2)")
}

MISSING_CMDS=()
MISSING_HDRS=()

check_cmd gcc      build-essential
check_cmd g++      build-essential
check_cmd make     build-essential
check_cmd ar       build-essential
check_cmd ld       build-essential
check_cmd strip    build-essential
check_cmd xxd      xxd
check_hdr zlib.h   zlib1g-dev

if [ ${#MISSING_CMDS[@]} -gt 0 ] || [ ${#MISSING_HDRS[@]} -gt 0 ]; then
    echo "ERROR: missing build dependencies for e9patch:" >&2
    for item in "${MISSING_CMDS[@]}"; do
        echo "  command: $item" >&2
    done
    for item in "${MISSING_HDRS[@]}"; do
        echo "  header:  $item" >&2
    done
    echo "" >&2
    echo "Install all with:" >&2
    echo "  sudo apt-get install build-essential xxd zlib1g-dev" >&2
    exit 1
fi

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
# We invoke `make release` directly instead of upstream's build.sh to avoid
# the unnecessary `markdown` dependency check. The `release` target builds
# the vendored zydis/libdw, compiles e9patch+e9tool, and strips the binaries.
echo "Building e9patch ..."
cd "$E9_DIR"
make clean
make -j"$(nproc)" release

# --- Verify ---
for bin in e9tool e9patch; do
    if [ ! -x "$E9_DIR/$bin" ]; then
        echo "ERROR: $bin not found after build" >&2
        exit 1
    fi
done

echo "e9patch installed successfully: $E9_DIR"
