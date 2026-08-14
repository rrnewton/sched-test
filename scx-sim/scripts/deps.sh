#!/bin/bash
# deps.sh — single source of truth for scx-sim's external dependencies.
#
# Usage:
#   ./scripts/deps.sh check              # report every dependency + how to fix
#   ./scripts/deps.sh check --strict     # also fail on optional deps
#   ./scripts/deps.sh install            # install everything that is missing
#   ./scripts/deps.sh install --tier coverage
#
# Normally invoked as `make check-deps` / `make install-deps`, and from
# validate.sh (which aborts when a required dependency is missing rather
# than failing later with a confusing error).
#
# Tiers:
#   required  — needed to build and to pass ./validate.sh
#   validate  — validate.sh runs without it but SKIPS a gate (loud warning)
#   coverage  — needed by ./coverage.sh
#   optional  — needed only by a specific extra workflow (VM runs, tracing)
#
# Adding a dependency: call register() below and define check_<id> (and,
# when it can be installed unattended, install_<id>). Everything else —
# reporting, exit codes, the install driver — is shared.
#
# PORTABILITY RULE: never hardcode an absolute path under a home directory,
# and never resolve two halves of one toolchain from two environments (the
# mypy-from-~/.local vs pip-from-.venv bug). Resolve from $PATH, from an
# explicit env var, or from an in-repo path — and say which one was used.

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_DIR="$(pwd)"
VENV="$REPO_DIR/.venv"

# Python packages the venv must carry, split by what needs them.
VENV_TYPECHECK_PKGS=(mypy pandas-stubs)
VENV_BENCHMARK_PKGS=(pandas plotly)

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
DETAIL=""   # set by a check to describe what it found
FIX=""      # set by a check to give the copy-pasteable install action
detail() { DETAIL="$*"; }
fix()    { FIX="$*"; }

# Run a network command through with-proxy when available (Meta hosts).
net() {
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy "$@"
    else
        "$@"
    fi
}

# Package-manager install line for system packages. Chef may revert dnf
# installs on Meta hosts, so this is only ever offered for things that
# genuinely have no $HOME-local alternative.
sys_hint() {   # $1 = dnf packages, $2 = apt packages
    if command -v dnf >/dev/null 2>&1; then
        echo "sudo dnf install -y $1   # note: chef may revert this"
    elif command -v apt-get >/dev/null 2>&1; then
        echo "sudo apt-get install -y $2"
    else
        echo "install with your package manager: $1"
    fi
}

# ---------------------------------------------------------------------------
# Dependency registry
# ---------------------------------------------------------------------------
declare -a DEP_IDS=()
declare -A DEP_TIER=() DEP_WHY=()

register() {   # $1 = id, $2 = tier, $3 = why it is needed
    DEP_IDS+=("$1")
    DEP_TIER["$1"]="$2"
    DEP_WHY["$1"]="$3"
}

register rust        required "cargo/rustc — builds everything"
register rustfmt     required "validate.sh: cargo fmt --check"
register clippy      required "validate.sh: cargo clippy -D warnings"
register nextest     required "validate.sh: cargo nextest run"
register cc-toolchain required "build.rs, schedulers/Makefile, e9patch build"
register clang       required "compiles the BPF schedulers as userspace C"
register libelf      required "libbpf-sys: ELF parsing"
register zlib        required "libbpf-sys: compressed ELF"
register python3     required "stress.py smoke tests, benchmark scripts"
register venv-typecheck required "validate.sh: scripts/typecheck.sh (mypy --strict)"

register e9patch     validate  "validate.sh: e9-instrumented scheduler build + stress.py e9 mode"

register jq              coverage "coverage.sh: parses cargo --message-format=json"
register llvm-coverage   coverage "coverage.sh: llvm-profdata/llvm-cov + libclang_rt.profile"

register venv-benchmark optional "scripts/benchmark.py (pandas/plotly reports)"
register rt-app         optional "scxsim real-run / vm-run workloads"
register vng            optional "scxsim vm-run (virtme-ng)"
register bpftrace       optional "live-kernel structops tracing (--bpf-trace)"
register trace-processor optional "perfetto trace assertions in the Rust test suite"

# ---------------------------------------------------------------------------
# Checks. Each returns 0 when present (and calls detail), 1 when missing
# (having called fix with the exact action that installs it).
# ---------------------------------------------------------------------------

check_rust() {
    fix "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    command -v cargo >/dev/null 2>&1 || return 1
    command -v rustc >/dev/null 2>&1 || return 1
    detail "$(cargo --version) at $(command -v cargo)"
}

install_rust() {
    net curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
    sh /tmp/rustup-init.sh -y
    rm -f /tmp/rustup-init.sh
}

check_rustfmt() {
    fix "rustup component add rustfmt"
    cargo fmt --version >/dev/null 2>&1 || return 1
    detail "$(cargo fmt --version)"
}
install_rustfmt() { rustup component add rustfmt; }

check_clippy() {
    fix "rustup component add clippy"
    cargo clippy --version >/dev/null 2>&1 || return 1
    detail "$(cargo clippy --version)"
}
install_clippy() { rustup component add clippy; }

check_nextest() {
    fix "curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C \"\${CARGO_HOME:-\$HOME/.cargo}/bin\""
    cargo nextest --version >/dev/null 2>&1 || return 1
    detail "$(cargo nextest --version | head -1)"
}
install_nextest() {
    local bindir="${CARGO_HOME:-$HOME/.cargo}/bin"
    mkdir -p "$bindir"
    net curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$bindir"
}

# gcc/g++/make/... are needed by cc-rs, by schedulers/Makefile and by the
# e9patch build. There is no sane $HOME-local substitute, so a missing one
# is reported as a system-package action.
CC_TOOLS=(gcc g++ make ar ld strip xxd pkg-config nproc)
check_cc-toolchain() {
    local missing=()
    for t in "${CC_TOOLS[@]}"; do
        command -v "$t" >/dev/null 2>&1 || missing+=("$t")
    done
    if [ ${#missing[@]} -gt 0 ]; then
        fix "$(sys_hint 'gcc gcc-c++ make binutils vim-common pkgconf-pkg-config' \
                        'build-essential xxd pkg-config')   # missing: ${missing[*]}"
        return 1
    fi
    detail "gcc $(gcc -dumpversion), make $(make --version | head -1 | awk '{print $3}')"
}

check_clang() {
    local cc="${BPF_CLANG:-clang}"
    fix "./scripts/install_llvm_oss.sh   (or $(sys_hint 'clang llvm' 'clang llvm'))"
    command -v "$cc" >/dev/null 2>&1 || return 1
    detail "$("$cc" --version | head -1) at $(command -v "$cc")${BPF_CLANG:+ (BPF_CLANG)}"
}

check_libelf() {
    fix "$(sys_hint 'elfutils-libelf-devel' 'libelf-dev')"
    pkg-config --exists libelf 2>/dev/null || return 1
    detail "libelf $(pkg-config --modversion libelf)"
}

check_zlib() {
    fix "$(sys_hint 'zlib-devel' 'zlib1g-dev')"
    pkg-config --exists zlib 2>/dev/null || return 1
    detail "zlib $(pkg-config --modversion zlib)"
}

check_python3() {
    fix "$(sys_hint 'python3' 'python3 python3-venv')"
    command -v python3 >/dev/null 2>&1 || return 1
    detail "$(python3 --version) at $(command -v python3)"
}

# The venv is the ONE python environment for this repo: typecheck.sh runs
# .venv/bin/mypy and probes .venv/bin/pip, never a PATH mypy that might be
# a different interpreter with different stubs installed.
venv_has() {   # $1.. = pip package names
    [ -x "$VENV/bin/pip" ] || return 1
    local pkg
    for pkg in "$@"; do
        "$VENV/bin/pip" show "$pkg" >/dev/null 2>&1 || return 1
    done
}

venv_install() {   # $1.. = pip package names
    if [ ! -x "$VENV/bin/pip" ]; then
        echo "Creating venv at $VENV"
        python3 -m venv "$VENV"
    fi
    net "$VENV/bin/pip" install --upgrade "$@"
}

check_venv-typecheck() {
    fix "make install-deps   (python3 -m venv .venv && .venv/bin/pip install ${VENV_TYPECHECK_PKGS[*]})"
    [ -x "$VENV/bin/mypy" ] || return 1
    venv_has "${VENV_TYPECHECK_PKGS[@]}" || return 1
    detail "$("$VENV/bin/mypy" --version) in .venv (+ ${VENV_TYPECHECK_PKGS[*]})"
}
install_venv-typecheck() { venv_install "${VENV_TYPECHECK_PKGS[@]}"; }

check_venv-benchmark() {
    fix "make install-deps   (.venv/bin/pip install ${VENV_BENCHMARK_PKGS[*]})"
    venv_has "${VENV_BENCHMARK_PKGS[@]}" || return 1
    detail "${VENV_BENCHMARK_PKGS[*]} in .venv"
}
install_venv-benchmark() { venv_install "${VENV_BENCHMARK_PKGS[@]}"; }

check_e9patch() {
    fix "make install-e9patch"
    local e9="${E9TOOL:-$REPO_DIR/third_party/e9patch/e9tool}"
    if [ ! -x "$e9" ]; then
        e9=$(command -v e9tool 2>/dev/null || true)
    fi
    [ -n "$e9" ] && [ -x "$e9" ] || return 1
    detail "$e9"
}
install_e9patch() { "$REPO_DIR/scripts/install_e9patch.sh"; }

check_jq() {
    fix "$(sys_hint jq jq)"
    command -v jq >/dev/null 2>&1 || return 1
    detail "$(jq --version)"
}

# coverage.sh needs llvm-profdata, llvm-cov AND a clang whose runtime dir
# carries libclang_rt.profile*.a. The stock CentOS clang and the fb clang
# both fail the last part, so this check distinguishes three states:
# absent, installed-but-not-activated, and ready.
check_llvm-coverage() {
    local prefix="${LLVM_HOME:-$HOME/opt/llvm-18.1.8}"
    if [ -x "$prefix/bin/clang" ]; then
        fix "source $prefix/env.sh   # installed but not active in this shell"
    else
        fix "./scripts/install_llvm_oss.sh && source \$HOME/opt/llvm-18.1.8/env.sh"
    fi
    command -v llvm-profdata >/dev/null 2>&1 || return 1
    command -v llvm-cov >/dev/null 2>&1 || return 1
    local cc="${BPF_CLANG:-clang}" rt_dir
    command -v "$cc" >/dev/null 2>&1 || return 1
    rt_dir=$("$cc" --print-runtime-dir 2>/dev/null || true)
    [ -n "$rt_dir" ] || return 1
    ls "$rt_dir"/libclang_rt.profile*.a >/dev/null 2>&1 || return 1
    detail "llvm-cov $(command -v llvm-cov), profile rt in $rt_dir"
}
install_llvm-coverage() {
    "$REPO_DIR/scripts/install_llvm_oss.sh"
    echo "NOTE: run 'source \$HOME/opt/llvm-18.1.8/env.sh' before ./coverage.sh"
}

# Resolution order must match rtapp_bin() in src/bin/scxsim/real_run.rs.
check_rt-app() {
    fix "build from https://github.com/scheduler-tools/rt-app and put it on \$PATH (or set SCXSIM_RTAPP_BIN)"
    local bin="${SCXSIM_RTAPP_BIN:-}"
    [ -n "$bin" ] || bin=$(command -v rt-app 2>/dev/null || true)
    [ -n "$bin" ] || bin="$HOME/bin/rt-app"
    [ -x "$bin" ] || return 1
    detail "$bin"
}

check_vng() {
    fix "pipx install virtme-ng   (or: $(sys_hint virtme-ng virtme-ng))"
    command -v vng >/dev/null 2>&1 || return 1
    detail "$(command -v vng)"
}

check_bpftrace() {
    fix "$(sys_hint bpftrace bpftrace)"
    command -v bpftrace >/dev/null 2>&1 || return 1
    detail "$(command -v bpftrace)"
}

# Matches the lookup in crates/scx_simulator/tests/perfetto_pb.rs.
check_trace-processor() {
    fix "download trace_processor_shell from https://github.com/google/perfetto/releases into \$HOME/bin"
    local bin
    bin=$(command -v trace_processor_shell 2>/dev/null || true)
    [ -n "$bin" ] || bin="$HOME/bin/trace_processor_shell"
    [ -x "$bin" ] || return 1
    detail "$bin"
}

# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------
MODE="check"
STRICT=0
ALL=0
TIER_FILTER=""

usage() {
    sed -n '2,20p' "$0"
    exit "${1:-0}"
}

[ $# -gt 0 ] && case "$1" in
    check|install) MODE="$1"; shift ;;
    -h|--help)     usage ;;
esac
while [ $# -gt 0 ]; do
    case "$1" in
        --strict)    STRICT=1 ;;
        --all)       ALL=1 ;;
        --tier)      TIER_FILTER="${2:-}"; shift ;;
        -h|--help)   usage ;;
        *)           echo "unknown argument: $1" >&2; usage 1 ;;
    esac
    shift
done

selected() {   # $1 = dep id
    [ -z "$TIER_FILTER" ] || [ "${DEP_TIER[$1]}" = "$TIER_FILTER" ]
}

MISSING_REQUIRED=()
MISSING_OTHER=()
declare -A MISSING_FIX=()

echo "=== scx-sim dependency check ==="
for id in "${DEP_IDS[@]}"; do
    selected "$id" || continue
    DETAIL=""; FIX=""
    if "check_$id"; then
        printf '  [ok]      %-17s %s\n' "$id" "$DETAIL"
    else
        MISSING_FIX["$id"]="$FIX"
        if [ "${DEP_TIER[$id]}" = required ]; then
            MISSING_REQUIRED+=("$id")
            printf '  [MISSING] %-17s REQUIRED — %s\n' "$id" "${DEP_WHY[$id]}"
        else
            MISSING_OTHER+=("$id")
            printf '  [missing] %-17s %s — %s\n' "$id" "${DEP_TIER[$id]}" "${DEP_WHY[$id]}"
        fi
        printf '            %-17s fix: %s\n' "" "$FIX"
    fi
done

TOTAL_MISSING=$(( ${#MISSING_REQUIRED[@]} + ${#MISSING_OTHER[@]} ))

if [ "$MODE" = check ]; then
    echo ""
    if [ "$TOTAL_MISSING" -eq 0 ]; then
        echo "All checked dependencies present."
        exit 0
    fi
    echo "Missing: ${#MISSING_REQUIRED[@]} required, ${#MISSING_OTHER[@]} other."
    echo "Install everything that can be installed unattended with: make install-deps"
    if [ ${#MISSING_REQUIRED[@]} -gt 0 ]; then
        exit 1
    fi
    [ "$STRICT" -eq 1 ] && exit 1
    exit 0
fi

# --- install mode ---
if [ "$TOTAL_MISSING" -eq 0 ]; then
    echo ""
    echo "Nothing to install."
    exit 0
fi

MANUAL=()
FAILED=()
DEFERRED=()
for id in "${MISSING_REQUIRED[@]}" "${MISSING_OTHER[@]}"; do
    # The coverage toolchain is a ~1.3 GiB download / ~7 GiB install, so it is
    # opt-in rather than a surprise inside a plain `make install-deps`.
    if [ "${DEP_TIER[$id]}" = coverage ] && [ -z "$TIER_FILTER" ] && [ "$ALL" -eq 0 ]; then
        DEFERRED+=("$id")
        continue
    fi
    if ! declare -F "install_$id" >/dev/null; then
        MANUAL+=("$id")
        continue
    fi
    echo ""
    echo "=== Installing $id ==="
    if "install_$id"; then
        echo "  $id installed."
    else
        echo "  ERROR: install of $id failed." >&2
        FAILED+=("$id")
    fi
done

echo ""
echo "=== install-deps summary ==="
if [ ${#DEFERRED[@]} -gt 0 ]; then
    echo "Not installed here (large, opt-in) — run 'make install-deps-coverage':"
    for id in "${DEFERRED[@]}"; do
        printf '  %-17s %s\n' "$id" "${MISSING_FIX[$id]}"
    done
fi
if [ ${#MANUAL[@]} -gt 0 ]; then
    echo "Needs a manual action (system package or external download):"
    for id in "${MANUAL[@]}"; do
        printf '  %-17s %s\n' "$id" "${MISSING_FIX[$id]}"
    done
fi
if [ ${#FAILED[@]} -gt 0 ]; then
    echo "Failed to install:"
    for id in "${FAILED[@]}"; do
        printf '  %-17s %s\n' "$id" "${MISSING_FIX[$id]}"
    done
fi
if [ ${#MANUAL[@]} -eq 0 ] && [ ${#FAILED[@]} -eq 0 ] && [ ${#DEFERRED[@]} -eq 0 ]; then
    echo "All missing dependencies installed. Re-run 'make check-deps' to confirm."
    exit 0
fi
echo ""
echo "Re-run 'make check-deps' after handling the above."
exit 1
