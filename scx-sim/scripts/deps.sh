#!/bin/bash
# deps.sh — single source of truth for scx-sim's external dependencies.
#
#   ./scripts/deps.sh check      # report every dependency; exit 1 if a
#                                # REQUIRED one is missing (default mode)
#   ./scripts/deps.sh check --strict   # also exit 1 on missing OPTIONAL deps
#   ./scripts/deps.sh install    # install everything installable
#   ./scripts/deps.sh install <key> [<key>...]   # install just these
#   ./scripts/deps.sh list       # print the dependency keys
#
# Reached from the Makefile as `make check-deps` / `make install-deps`, and
# from validate.sh (which refuses to run with a REQUIRED dep missing).
#
# Design rules for this file:
#
# 1. NO MACHINE-SPECIFIC PATHS. Nothing here may assume a particular user,
#    home directory layout, or distro package set. Anything installed into
#    $HOME goes under $HOME/opt/<tool>-<version> plus an env.sh, following
#    the OSS LLVM install that already works on the dev box.
# 2. ONE ENVIRONMENT PER TOOLCHAIN. Python tooling is resolved from `.venv`
#    and only `.venv`. Mixing a PATH `mypy` with a `.venv/bin/pip` probe is
#    the exact bug that motivated this script.
# 3. NO SILENT SKIPS. Every dependency is either found (with its location
#    printed) or reported missing with a copy-pasteable install action.
#
# Toolchain policy (owner): prefer OSS tooling over the fb clang. Install
# order of preference is a global /nix store, then podman, then $HOME.
# There is no /nix on the current dev box, so the $HOME/opt path is what is
# implemented here. `dnf` commands are printed as hints but never run: chef
# can revert them and they can defeat a /nix install.

set -uo pipefail

cd "$(dirname "$0")/.."
SCXSIM_DIR="$(pwd)"
REPO_ROOT="$(cd .. && pwd)"

# Pinned OSS LLVM. 18.1.8 is the version already validated on the dev box:
# it is the newest release whose prebuilt tarball ships
# libclang_rt.profile.a, which SCX_SIM_COVERAGE=1 needs and which neither
# the CentOS clang nor the fb clang provides.
LLVM_VERSION="18.1.8"
LLVM_PREFIX="$HOME/opt/llvm-$LLVM_VERSION"
LLVM_TARBALL="clang+llvm-$LLVM_VERSION-x86_64-linux-gnu-ubuntu-18.04.tar.xz"
LLVM_URL="https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/$LLVM_TARBALL"

GH_VERSION="2.76.1"
GH_PREFIX="$HOME/opt/gh-$GH_VERSION"
GH_URL="https://github.com/cli/cli/releases/download/v$GH_VERSION/gh_${GH_VERSION}_linux_amd64.tar.gz"

VENV="$SCXSIM_DIR/.venv"
VENV_PIP="$VENV/bin/pip"
PY_TYPECHECK_PKGS=(mypy pandas-stubs)
PY_BENCHMARK_PKGS=(plotly pandas)

# Dependency keys, in report order.
REQUIRED_DEPS=(
    scx-submodule
    cc make binutils xxd pkg-config libelf zlib
    clang
    rustc cargo rustfmt clippy cargo-nextest
    python3 venv mypy pandas-stubs
)
OPTIONAL_DEPS=(
    compiler-rt-profile llvm-profdata llvm-cov
    e9tool
    gh
    plotly
    rt-app bpftrace trace-processor vng lldb
)

# --- helpers ---------------------------------------------------------------

# have <cmd> — echo the resolved path and succeed, or fail silently.
have() { command -v "$1" 2>/dev/null; }

# have_header <header.h> — succeed if the C preprocessor can find it.
have_header() {
    echo "#include <$1>" | "${CC:-cc}" -E - >/dev/null 2>&1
}

# The clang scx-sim actually compiles scheduler C with.
bpf_clang() { echo "${BPF_CLANG:-clang}"; }

# Location of libclang_rt.profile.a for the effective clang, if any.
#
# Neither the CentOS clang nor the fb clang ships one, so the usual outcome
# on a Meta dev box is "not found via BPF_CLANG, but present in the OSS LLVM
# we installed under $HOME". That is an ACTIVATION problem, not a missing
# install, and the two must not be reported the same way.
profile_runtime_lib() {
    local cc rt
    cc="$(bpf_clang)"
    if command -v "$cc" >/dev/null 2>&1; then
        rt="$("$cc" --print-runtime-dir 2>/dev/null)"
        # clang prints a human sentence, not a path, when there is none.
        if [ -d "${rt:-}" ] && ls "$rt"/libclang_rt.profile*.a >/dev/null 2>&1; then
            ls "$rt"/libclang_rt.profile*.a | head -1
            return 0
        fi
    fi
    if ls "$LLVM_PREFIX"/lib/clang/*/lib/*/libclang_rt.profile*.a >/dev/null 2>&1; then
        echo "present in $LLVM_PREFIX but BPF_CLANG=$cc does not see it." >&2
        echo "Activate the OSS toolchain:  source $LLVM_PREFIX/env.sh" >&2
    fi
    return 1
}

# Fetch a URL to a file. Retries through with-proxy, which is how the dev
# box reaches github.com.
fetch() {
    local url="$1" out="$2"
    if curl -fsSL --retry 2 -o "$out" "$url"; then return 0; fi
    if have with-proxy >/dev/null; then
        echo "  direct fetch failed; retrying via with-proxy ..." >&2
        with-proxy curl -fsSL --retry 2 -o "$out" "$url" && return 0
    fi
    return 1
}

# --- dependency table ------------------------------------------------------
#
# dep <key> <verb>, where verb is one of:
#   why      one line naming what breaks without it
#   hint     a copy-pasteable command that installs it
#   check    echo where it was found; exit 0 found / 1 missing
#   install  do the install; exit 0 ok / 1 failed / 3 no automatic install
#
# Everything known about a dependency lives in its single case branch.

dep() {
    local key="$1" verb="$2"
    case "$key" in

    scx-submodule)
        case "$verb" in
        why)   echo "scheduler .so build (scheds/include, lib/scxtest, vmlinux headers)" ;;
        hint)  echo "git -C '$REPO_ROOT' submodule update --init --recursive" ;;
        check) [ -f "$REPO_ROOT/scheds/include/lib/cgroup.h" ] \
                   && echo "$REPO_ROOT/scheds" ;;
        install) git -C "$REPO_ROOT" submodule update --init --recursive ;;
        esac ;;

    cc)
        case "$verb" in
        why)   echo "C toolchain used by cargo build scripts and e9patch" ;;
        hint)  echo "sudo dnf install gcc gcc-c++   # Debian: build-essential" ;;
        check) have cc && have g++ >/dev/null ;;
        install) return 3 ;;
        esac ;;

    make)
        case "$verb" in
        why)   echo "schedulers/Makefile, docs/guide/Makefile" ;;
        hint)  echo "sudo dnf install make   # Debian: build-essential" ;;
        check) have make ;;
        install) return 3 ;;
        esac ;;

    binutils)
        case "$verb" in
        why)   echo "ar/ld/strip — linking scheduler .so files and e9patch" ;;
        hint)  echo "sudo dnf install binutils   # Debian: binutils" ;;
        check) have ar >/dev/null && have ld >/dev/null && have strip ;;
        install) return 3 ;;
        esac ;;

    xxd)
        case "$verb" in
        why)   echo "hex dumps during the scheduler build and e9patch build" ;;
        hint)  echo "sudo dnf install vim-common   # Debian: xxd" ;;
        check) have xxd ;;
        install) return 3 ;;
        esac ;;

    pkg-config)
        case "$verb" in
        why)   echo "libbpf/libelf discovery during cargo build" ;;
        hint)  echo "sudo dnf install pkgconf-pkg-config   # Debian: pkg-config" ;;
        check) have pkg-config ;;
        install) return 3 ;;
        esac ;;

    libelf)
        case "$verb" in
        why)   echo "libbpf-sys (BPF object parsing)" ;;
        hint)  echo "sudo dnf install elfutils-libelf-devel   # Debian: libelf-dev" ;;
        check) have_header libelf.h && echo "libelf.h" ;;
        install) return 3 ;;
        esac ;;

    zlib)
        case "$verb" in
        why)   echo "libbpf-sys (compressed ELF) and e9patch" ;;
        hint)  echo "sudo dnf install zlib-devel   # Debian: zlib1g-dev" ;;
        check) have_header zlib.h && echo "zlib.h" ;;
        install) return 3 ;;
        esac ;;

    clang)
        case "$verb" in
        why)   echo "compiles the BPF scheduler C as userspace code (BPF_CLANG)" ;;
        hint)  echo "make install-deps   # OSS LLVM $LLVM_VERSION into $LLVM_PREFIX" ;;
        check) have "$(bpf_clang)" ;;
        install) install_oss_llvm ;;
        esac ;;

    rustc|cargo)
        case "$verb" in
        why)   echo "the primary build system" ;;
        hint)  echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" ;;
        check) have "$key" ;;
        install) return 3 ;;
        esac ;;

    rustfmt)
        case "$verb" in
        why)   echo "validate.sh: cargo fmt --all -- --check" ;;
        hint)  echo "rustup component add rustfmt" ;;
        check) cargo fmt --version >/dev/null 2>&1 && cargo fmt --version ;;
        install) rustup component add rustfmt ;;
        esac ;;

    clippy)
        case "$verb" in
        why)   echo "validate.sh: cargo clippy --all -- -D warnings" ;;
        hint)  echo "rustup component add clippy" ;;
        check) cargo clippy --version >/dev/null 2>&1 && cargo clippy --version ;;
        install) rustup component add clippy ;;
        esac ;;

    cargo-nextest)
        case "$verb" in
        why)   echo "validate.sh: cargo nextest run --workspace" ;;
        hint)  echo "cargo install cargo-nextest --locked" ;;
        check) have cargo-nextest ;;
        install) cargo install cargo-nextest --locked ;;
        esac ;;

    python3)
        case "$verb" in
        why)   echo "bug_finding/stress.py, scripts/*.py, benchmark tooling" ;;
        hint)  echo "sudo dnf install python3   # Debian: python3 python3-venv" ;;
        check) have python3 ;;
        install) return 3 ;;
        esac ;;

    venv)
        # The venv is REQUIRED, not a nicety: the system pip on Meta dev
        # boxes refuses every install ("direct installs are not allowed on
        # the Production system paths"), so a venv is the only portable
        # place to put mypy and its stubs.
        case "$verb" in
        why)   echo "the one Python environment all Python tooling resolves from" ;;
        hint)  echo "python3 -m venv '$VENV'" ;;
        check) [ -x "$VENV/bin/python3" ] && echo "$VENV" ;;
        install) python3 -m venv "$VENV" ;;
        esac ;;

    mypy)
        case "$verb" in
        why)   echo "validate.sh: scripts/typecheck.sh runs mypy --strict" ;;
        hint)  echo "'$VENV_PIP' install mypy" ;;
        check) [ -x "$VENV/bin/mypy" ] && echo "$VENV/bin/mypy" ;;
        install) venv_pip_install mypy ;;
        esac ;;

    pandas-stubs)
        case "$verb" in
        why)   echo "mypy --strict on scripts/benchmark.py (which imports pandas)" ;;
        hint)  echo "'$VENV_PIP' install pandas-stubs" ;;
        check) [ -x "$VENV_PIP" ] && "$VENV_PIP" show pandas-stubs >/dev/null 2>&1 \
                   && echo "$VENV (pip show pandas-stubs)" ;;
        install) venv_pip_install pandas-stubs ;;
        esac ;;

    compiler-rt-profile)
        case "$verb" in
        why)   echo "coverage.sh (SCX_SIM_COVERAGE=1 needs libclang_rt.profile.a)" ;;
        hint)  echo "make install-deps && source '$LLVM_PREFIX/env.sh'" ;;
        check) profile_runtime_lib ;;
        install) install_oss_llvm ;;
        esac ;;

    llvm-profdata|llvm-cov)
        case "$verb" in
        why)   echo "coverage.sh (merging and rendering coverage data)" ;;
        hint)  echo "make install-deps && source '$LLVM_PREFIX/env.sh'" ;;
        check) have "$key" ;;
        install) install_oss_llvm ;;
        esac ;;

    e9tool)
        case "$verb" in
        why)   echo "validate.sh e9-instrumented scheduler build; stress.py e9patch mode" ;;
        hint)  echo "make install-e9patch" ;;
        check) [ -x "$SCXSIM_DIR/third_party/e9patch/e9tool" ] \
                   && echo "$SCXSIM_DIR/third_party/e9patch/e9tool" \
                   || have e9tool ;;
        install) "$SCXSIM_DIR/scripts/install_e9patch.sh" ;;
        esac ;;

    gh)
        # gh is not needed to build or test, but a BROKEN gh is worse than
        # an absent one: git's github.com credential helper on this box is
        # `!gh auth git-credential`, so a dangling gh symlink silently
        # breaks git authentication against github.com. check_gh reports
        # the dangling case explicitly.
        case "$verb" in
        why)   echo "GitHub CLI; also backs git's github.com credential helper" ;;
        hint)  echo "make install-deps   # gh $GH_VERSION into $GH_PREFIX" ;;
        check) check_gh ;;
        install) install_gh ;;
        esac ;;

    plotly)
        case "$verb" in
        why)   echo "benchmark/sweep HTML dashboards (make benchmark)" ;;
        hint)  echo "'$VENV_PIP' install ${PY_BENCHMARK_PKGS[*]}" ;;
        check) [ -x "$VENV_PIP" ] && "$VENV_PIP" show plotly >/dev/null 2>&1 \
                   && echo "$VENV (pip show plotly)" ;;
        install) venv_pip_install "${PY_BENCHMARK_PKGS[@]}" ;;
        esac ;;

    rt-app)
        case "$verb" in
        why)   echo "scxsim real/VM runs (scripts/run_real.sh, scxsim vm-run)" ;;
        hint)  echo "build from https://github.com/scheduler-tools/rt-app, then set SCXSIM_RTAPP_BIN or install to \$HOME/bin/rt-app" ;;
        # Must mirror rtapp_bin() in crates/scx_simulator/src/bin/scxsim/real_run.rs
        # EXACTLY: $SCXSIM_RTAPP_BIN, else $HOME/bin/rt-app. Deliberately does NOT
        # accept rt-app merely being on PATH — real_run.rs does not look there, so
        # reporting [ok] for a PATH-only rt-app would be the checker asserting
        # something the code will not do, which is the whole failure class this
        # script exists to prevent. If real_run.rs gains PATH lookup, add it here
        # in the same order.
        check) { [ -x "${SCXSIM_RTAPP_BIN:-}" ] && echo "$SCXSIM_RTAPP_BIN"; } \
                   || { [ -x "$HOME/bin/rt-app" ] && echo "$HOME/bin/rt-app"; } ;;
        install) return 3 ;;
        esac ;;

    bpftrace)
        case "$verb" in
        why)   echo "live-kernel tracing for the scxsim-vs-kernel call diff" ;;
        hint)  echo "sudo dnf install bpftrace" ;;
        check) have bpftrace ;;
        install) return 3 ;;
        esac ;;

    trace-processor)
        case "$verb" in
        why)   echo "tests/perfetto_pb.rs trace_processor ingestion test (skipped without it)" ;;
        hint)  echo "download trace_processor_shell from https://perfetto.dev and put it on PATH or in ~/bin" ;;
        check) have trace_processor_shell \
                   || { [ -x "$HOME/bin/trace_processor_shell" ] && echo "$HOME/bin/trace_processor_shell"; } ;;
        install) return 3 ;;
        esac ;;

    vng)
        case "$verb" in
        why)   echo "scxsim vm-run (virtme-ng VM launches)" ;;
        hint)  echo "pipx install virtme-ng   # see https://github.com/arighi/virtme-ng" ;;
        check) have vng ;;
        install) return 3 ;;
        esac ;;

    lldb)
        case "$verb" in
        why)   echo "lldb_debug/ worked examples" ;;
        hint)  echo "sudo dnf install lldb" ;;
        check) have lldb ;;
        install) return 3 ;;
        esac ;;

    *)
        echo "deps.sh: unknown dependency key '$key'" >&2
        return 2 ;;
    esac
}

# --- checks that need more than one line ------------------------------------

# gh has three states, not two: present, absent, or present-but-dangling.
# The dangling case is the dangerous one — `command -v gh` still fails, but
# the reason ("the symlink points at a directory that no longer exists") is
# invisible unless we say so.
check_gh() {
    local resolved
    resolved="$(command -v gh 2>/dev/null)"
    if [ -n "$resolved" ] && [ -x "$resolved" ]; then
        echo "$resolved"
        return 0
    fi
    local d
    for d in "$HOME/bin/gh" "$HOME/.local/bin/gh" "$GH_PREFIX/bin/gh"; do
        if [ -L "$d" ] && [ ! -e "$d" ]; then
            echo "DANGLING SYMLINK: $d -> $(readlink "$d")" >&2
            return 1
        fi
    done
    return 1
}

# --- installers -------------------------------------------------------------

venv_pip_install() {
    dep venv check >/dev/null || dep venv install || return 1
    "$VENV_PIP" install --upgrade "$@"
}

# OSS LLVM into $HOME/opt/llvm-<ver>, with an env.sh to activate it.
#
# This is the shape every $HOME-installed toolchain in this repo should
# follow: versioned prefix, nothing installed system-wide, no chef-owned
# path touched, one `source .../env.sh` to activate. It therefore survives
# a chef run, unlike a `dnf install`.
install_oss_llvm() {
    if [ -x "$LLVM_PREFIX/bin/clang" ]; then
        echo "  OSS LLVM already present at $LLVM_PREFIX"
    else
        local tmp
        tmp="$(mktemp -d)" || return 1
        echo "  downloading $LLVM_URL ..."
        if ! fetch "$LLVM_URL" "$tmp/$LLVM_TARBALL"; then
            rm -rf "$tmp"
            echo "  ERROR: could not download OSS LLVM $LLVM_VERSION" >&2
            return 1
        fi
        echo "  extracting into $LLVM_PREFIX ..."
        mkdir -p "$LLVM_PREFIX" || { rm -rf "$tmp"; return 1; }
        tar -xf "$tmp/$LLVM_TARBALL" -C "$LLVM_PREFIX" --strip-components=1 \
            || { rm -rf "$tmp"; return 1; }
        rm -rf "$tmp"
    fi

    # The ubuntu-18.04 release build links against libtinfo.so.5; distros
    # newer than that only ship .so.6. A private compat dir on
    # LD_LIBRARY_PATH bridges the two without touching /usr/lib64.
    mkdir -p "$LLVM_PREFIX/compat"
    if [ ! -e "$LLVM_PREFIX/compat/libtinfo.so.5" ]; then
        local tinfo
        tinfo="$(ls /usr/lib64/libtinfo.so.6 /usr/lib/x86_64-linux-gnu/libtinfo.so.6 2>/dev/null | head -1)"
        if [ -z "$tinfo" ]; then
            echo "  ERROR: no libtinfo.so.6 found; $LLVM_PREFIX/bin/clang will not start." >&2
            echo "         Install ncurses libs, then re-run: make install-deps" >&2
            return 1
        fi
        ln -sf "$tinfo" "$LLVM_PREFIX/compat/libtinfo.so.5"
    fi

    cat > "$LLVM_PREFIX/env.sh" <<EOF
# Source this to put the OSS LLVM $LLVM_VERSION toolchain on PATH.
#   source $LLVM_PREFIX/env.sh
# Installed by scx-sim/scripts/deps.sh. Neither the CentOS clang nor the fb
# clang ships libclang_rt.profile.a, which scx-sim's SCX_SIM_COVERAGE=1
# build needs.
export LLVM_HOME="$LLVM_PREFIX"
# The ubuntu-18.04 release build wants libtinfo.so.5; newer distros only
# have .so.6, so the private compat dir goes first.
export LD_LIBRARY_PATH="\$LLVM_HOME/compat\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
export PATH="\$LLVM_HOME/bin:\$PATH"
export BPF_CLANG="\$LLVM_HOME/bin/clang"
EOF

    echo "  OSS LLVM $LLVM_VERSION ready. Activate with:"
    echo "      source $LLVM_PREFIX/env.sh"

    # Installed, but this shell still points at the distro clang. Report
    # that as "needs activation" (rc 4) rather than letting the caller's
    # post-install re-check call it an install failure.
    dep compiler-rt-profile check >/dev/null 2>&1 || return 4
}

install_gh() {
    if [ ! -x "$GH_PREFIX/bin/gh" ]; then
        local tmp
        tmp="$(mktemp -d)" || return 1
        echo "  downloading $GH_URL ..."
        if ! fetch "$GH_URL" "$tmp/gh.tar.gz"; then
            rm -rf "$tmp"
            echo "  ERROR: could not download gh $GH_VERSION" >&2
            return 1
        fi
        mkdir -p "$GH_PREFIX" || { rm -rf "$tmp"; return 1; }
        tar -xf "$tmp/gh.tar.gz" -C "$GH_PREFIX" --strip-components=1 \
            || { rm -rf "$tmp"; return 1; }
        rm -rf "$tmp"
    fi
    # Repoint ~/bin/gh if it is absent or dangling. An existing, working
    # gh elsewhere on PATH is left alone.
    mkdir -p "$HOME/bin"
    if [ ! -e "$HOME/bin/gh" ]; then
        ln -sfn "$GH_PREFIX/bin/gh" "$HOME/bin/gh"
        echo "  linked $HOME/bin/gh -> $GH_PREFIX/bin/gh"
    fi
    "$GH_PREFIX/bin/gh" --version | head -1
}

# --- report ----------------------------------------------------------------

STRICT=0
MISSING_REQUIRED=()
MISSING_OPTIONAL=()

report_group() {
    local label="$1"; shift
    local key loc rc
    echo "$label"
    for key in "$@"; do
        loc="$(dep "$key" check 2>/dev/null)"; rc=$?
        if [ $rc -eq 0 ]; then
            printf '  [ok]      %-22s %s\n' "$key" "${loc:-found}"
        else
            if [ "$label" = "REQUIRED" ]; then
                MISSING_REQUIRED+=("$key")
                printf '  [MISSING] %-22s needed by: %s\n' "$key" "$(dep "$key" why)"
            else
                MISSING_OPTIONAL+=("$key")
                printf '  [absent]  %-22s needed by: %s\n' "$key" "$(dep "$key" why)"
            fi
            # Re-run the check with stderr visible so extra diagnosis
            # (e.g. gh's dangling-symlink message) reaches the user.
            dep "$key" check 2>&1 >/dev/null | sed 's/^/            /'
            printf '            install: %s\n' "$(dep "$key" hint)"
        fi
    done
}

cmd_check() {
    echo "=== scx-sim dependency check ==="
    echo "    repo: $SCXSIM_DIR"
    echo "    clang in use (BPF_CLANG): $(bpf_clang)"
    echo ""
    report_group REQUIRED "${REQUIRED_DEPS[@]}"
    echo ""
    report_group OPTIONAL "${OPTIONAL_DEPS[@]}"
    echo ""

    local rc=0
    if [ ${#MISSING_REQUIRED[@]} -gt 0 ]; then
        echo "FAIL: ${#MISSING_REQUIRED[@]} required dependency/ies missing: ${MISSING_REQUIRED[*]}"
        echo "      Install them all with:  make install-deps"
        echo "      Or install one:         ./scripts/deps.sh install <key>"
        rc=1
    else
        echo "OK: all required dependencies present."
    fi
    if [ ${#MISSING_OPTIONAL[@]} -gt 0 ]; then
        echo "NOTE: ${#MISSING_OPTIONAL[@]} optional dependency/ies missing: ${MISSING_OPTIONAL[*]}"
        echo "      Checks that need them will be reported as SKIPPED, not silently passed."
        [ "$STRICT" = 1 ] && rc=1
    fi
    return $rc
}

cmd_install() {
    local keys=("$@")
    if [ ${#keys[@]} -eq 0 ]; then
        keys=("${REQUIRED_DEPS[@]}" "${OPTIONAL_DEPS[@]}")
    fi
    local key rc failed=() manual=() activate=()
    for key in "${keys[@]}"; do
        if dep "$key" check >/dev/null 2>&1; then
            printf '[ok]      %-22s already present\n' "$key"
            continue
        fi
        printf '[install] %-22s %s\n' "$key" "$(dep "$key" why)"
        dep "$key" install; rc=$?
        if [ $rc -eq 3 ]; then
            manual+=("$key")
            printf '  no automatic install; run:  %s\n' "$(dep "$key" hint)"
        elif [ $rc -eq 4 ]; then
            # Installed, but it only takes effect once the shell is
            # pointed at it. Not a failure — but not silently "done" either.
            activate+=("$key")
        elif [ $rc -ne 0 ]; then
            failed+=("$key")
            printf '  FAILED. Manual action:      %s\n' "$(dep "$key" hint)"
        elif ! dep "$key" check >/dev/null 2>&1; then
            failed+=("$key")
            printf '  install reported success but %s is still not detected.\n' "$key"
            printf '  Manual action:              %s\n' "$(dep "$key" hint)"
        else
            printf '  installed: %s\n' "$(dep "$key" check 2>/dev/null)"
        fi
    done

    echo ""
    if [ ${#activate[@]} -gt 0 ]; then
        echo "Installed, but needs activation in your shell (${activate[*]}):"
        echo "      source $LLVM_PREFIX/env.sh"
        echo "  Add that line to your shell rc to make it stick."
    fi
    if [ ${#manual[@]} -gt 0 ]; then
        echo "Needs a manual step (system packages / third-party installers):"
        for key in "${manual[@]}"; do
            printf '  %-22s %s\n' "$key" "$(dep "$key" hint)"
        done
    fi
    if [ ${#failed[@]} -gt 0 ]; then
        echo "FAILED to install: ${failed[*]}"
        return 1
    fi
    echo "install-deps done. Re-check with: make check-deps"
    return 0
}

# --- entry point ------------------------------------------------------------

MODE="${1:-check}"
shift || true
ARGS=()
for a in "$@"; do
    case "$a" in
        --strict) STRICT=1 ;;
        *) ARGS+=("$a") ;;
    esac
done

case "$MODE" in
    check)   cmd_check ;;
    install) cmd_install ${ARGS[@]+"${ARGS[@]}"} ;;
    list)    printf '%s\n' "${REQUIRED_DEPS[@]}" "${OPTIONAL_DEPS[@]}" ;;
    -h|--help)
        sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *)
        echo "usage: $0 {check [--strict] | install [<key>...] | list}" >&2
        exit 2 ;;
esac
