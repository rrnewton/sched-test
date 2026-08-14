#!/bin/bash
# install_llvm_oss.sh — Install an OSS LLVM/clang toolchain with the
# compiler-rt profile runtime that `./coverage.sh` (SCX_SIM_COVERAGE=1)
# requires.
#
# Usage:
#   ./scripts/install_llvm_oss.sh                # install to ~/opt/llvm-18.1.8
#   LLVM_PREFIX=/path ./scripts/install_llvm_oss.sh
#   with-proxy ./scripts/install_llvm_oss.sh     # explicit proxy (auto-detected)
#
# After install:
#   source ~/opt/llvm-18.1.8/env.sh
#
# WHY a $HOME install and not the system clang:
#   Neither /usr/bin/clang (CentOS) nor the fb clang at /opt/llvm ships
#   libclang_rt.profile.a, so `clang -fprofile-instr-generate` cannot link.
#   Per the toolchain policy the install order is  /nix -> podman -> $HOME.
#   There is no /nix on the current dev hosts and a container is not usable
#   for a compiler that cargo's build.rs invokes in-tree, so $HOME it is.
#   Nothing is installed system-wide, no dnf is used, and no chef-owned path
#   is touched — the install therefore survives a chef run.
#
# Idempotent: re-running with a good install present is a fast no-op.

set -euo pipefail

LLVM_VERSION="${LLVM_VERSION:-18.1.8}"
PREFIX="${LLVM_PREFIX:-$HOME/opt/llvm-$LLVM_VERSION}"
# The upstream ubuntu-18.04 binary release is the one that runs on CentOS 9
# (older glibc baseline). Its only unmet shared-library need is libtinfo.so.5,
# handled by the compat/ symlink below.
TARBALL="clang+llvm-$LLVM_VERSION-x86_64-linux-gnu-ubuntu-18.04.tar.xz"
URL="https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/$TARBALL"

# Run a network command through with-proxy when it exists (Meta hosts).
net() {
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy "$@"
    else
        "$@"
    fi
}

profile_rt_present() {
    # $1 = clang binary. True when the compiler-rt profile archive that
    # -fprofile-instr-generate links against is available.
    local clang_bin="$1" rt_dir
    [ -x "$clang_bin" ] || return 1
    rt_dir=$("$clang_bin" --print-runtime-dir 2>/dev/null || true)
    [ -n "$rt_dir" ] || return 1
    ls "$rt_dir"/libclang_rt.profile*.a >/dev/null 2>&1
}

# --- Already installed? ---
if profile_rt_present "$PREFIX/bin/clang"; then
    echo "OSS LLVM already installed at $PREFIX"
    echo "Activate with:  source $PREFIX/env.sh"
    exit 0
fi

if [ -d /nix ]; then
    echo "NOTE: /nix exists on this host. A nix-provided LLVM is preferred over"
    echo "      a \$HOME install; if you have one, point BPF_CLANG at its clang"
    echo "      instead of running this script. Continuing with \$HOME install."
fi

# --- Download + extract ---
mkdir -p "$(dirname "$PREFIX")"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "Downloading $URL"
echo "  (~1.3 GiB; extracts to ~7 GiB at $PREFIX)"
net curl -fL --progress-bar -o "$WORK/$TARBALL" "$URL"

echo "Extracting to $PREFIX ..."
mkdir -p "$PREFIX"
# --strip-components=1: the tarball has a single clang+llvm-*/ top directory.
tar -xf "$WORK/$TARBALL" -C "$PREFIX" --strip-components=1

# --- libtinfo.so.5 compat shim ---
# The ubuntu-18.04 release build links against libtinfo.so.5; CentOS 9 ships
# only libtinfo.so.6. The ABI is compatible for the symbols clang uses, so a
# symlink in a private compat/ dir (added to LD_LIBRARY_PATH by env.sh) is
# enough. Nothing outside $PREFIX is modified.
if ! ldconfig -p | grep -q 'libtinfo\.so\.5'; then
    TINFO6=$(ldconfig -p | awk '/libtinfo\.so\.6 \(libc6,x86-64\)/ {print $NF; exit}')
    if [ -z "$TINFO6" ]; then
        echo "ERROR: neither libtinfo.so.5 nor libtinfo.so.6 found." >&2
        echo "  Install ncurses libs, then re-run: $(basename "$0")" >&2
        exit 1
    fi
    mkdir -p "$PREFIX/compat"
    ln -sf "$TINFO6" "$PREFIX/compat/libtinfo.so.5"
    echo "Created compat shim: $PREFIX/compat/libtinfo.so.5 -> $TINFO6"
fi

# --- env.sh ---
cat > "$PREFIX/env.sh" <<EOF
# Source this to put the OSS LLVM $LLVM_VERSION coverage toolchain on PATH.
#   source $PREFIX/env.sh
# Written by scx-sim/scripts/install_llvm_oss.sh. Needed because neither the
# system clang nor the fb clang ships libclang_rt.profile.a, which scx-sim's
# SCX_SIM_COVERAGE=1 build requires.
export LLVM_HOME="$PREFIX"
# The ubuntu-18.04 release build wants libtinfo.so.5; CentOS 9 only has .so.6.
export LD_LIBRARY_PATH="\$LLVM_HOME/compat\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
export PATH="\$LLVM_HOME/bin:\$PATH"
export BPF_CLANG="\$LLVM_HOME/bin/clang"
EOF

# --- Verify ---
export LD_LIBRARY_PATH="$PREFIX/compat${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if ! "$PREFIX/bin/clang" --version >/dev/null 2>&1; then
    echo "ERROR: $PREFIX/bin/clang does not run. Missing shared libraries:" >&2
    ldd "$PREFIX/bin/clang" 2>&1 | grep 'not found' >&2 || true
    exit 1
fi
if ! profile_rt_present "$PREFIX/bin/clang"; then
    echo "ERROR: $PREFIX/bin/clang runs but has no libclang_rt.profile*.a in" >&2
    echo "  $("$PREFIX/bin/clang" --print-runtime-dir 2>/dev/null)" >&2
    echo "  This release tarball is unusable for coverage builds." >&2
    exit 1
fi

echo ""
echo "Installed OSS LLVM $LLVM_VERSION at $PREFIX"
echo "  clang:        $("$PREFIX/bin/clang" --version | head -1)"
echo "  profile rt:   $(ls "$("$PREFIX/bin/clang" --print-runtime-dir)"/libclang_rt.profile*.a)"
echo ""
echo "Activate in your shell (needed by ./coverage.sh):"
echo "  source $PREFIX/env.sh"
