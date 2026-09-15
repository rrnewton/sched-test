#!/bin/sh
# Install the scx-sim pre-commit hook into the enclosing git repo.
#
# Symlinks <repo-root>/.git/hooks/pre-commit -> ../../scx-sim/scripts/git-hooks/pre-commit
# so the tracked source-of-truth at scx-sim/scripts/git-hooks/pre-commit
# is what actually runs.
#
# Hook scope: enforces cargo fmt + clippy on commits that stage files
# under scx-sim/. See the hook header for details, env knobs, and the
# SKIP_SCXSIM_HOOK=1 escape hatch.
#
# Re-run this script after a fresh clone / new worktree, OR whenever
# someone re-creates `.git/hooks/pre-commit` by hand.

set -eu

repo_root=$(git rev-parse --show-toplevel)

# In a worktree, .git is a FILE (gitdir pointer), not a directory; the
# actual hooks live under the common git dir shared across worktrees.
# `git rev-parse --git-path hooks` resolves to the right path either way.
hooks_dir=$(git rev-parse --git-path hooks)
target="$hooks_dir/pre-commit"

# scx-sim/ relative to repo root: confirm the source file exists where
# we expect it.
src_rel="scx-sim/scripts/git-hooks/pre-commit"
src_abs="$repo_root/$src_rel"
if [ ! -f "$src_abs" ]; then
    echo "install.sh: source hook missing at $src_abs" >&2
    echo "install.sh: run this script from a checkout that has scx-sim/." >&2
    exit 1
fi

mkdir -p "$hooks_dir"

# Install as a COPY rather than a symlink so the hook is self-contained
# across worktrees (worktrees share the common .git/hooks/, but the
# checkout that holds the source file may be archived later). After
# pulling updates to scx-sim/scripts/git-hooks/pre-commit, re-run this
# script to refresh the installed copy.
#
# If a pre-existing pre-commit is present and DIFFERS from our source,
# warn but proceed — the assumption is that re-running install.sh is
# the canonical way to refresh / overwrite. Custom hooks should live
# OUTSIDE of pre-commit (e.g. as a hook-runner that calls the scxsim
# hook explicitly). Adapt this script if your repo evolves to need a
# multi-hook orchestrator.
if [ -f "$target" ] && ! cmp -s "$src_abs" "$target"; then
    echo "install.sh: WARNING — overwriting existing differing pre-commit at $target" >&2
fi

cp "$src_abs" "$target"
chmod +x "$target"

echo "install.sh: installed pre-commit hook at $target"
echo "install.sh:   copied from $src_rel"
echo ""
echo "Verify with:"
echo "  ls -l $target"
echo ""
echo "The hook runs cargo fmt --check (always) + cargo clippy --no-deps"
echo "(when .rs/Cargo.{toml,lock} files under scx-sim/ are staged)."
echo "Bypass for emergencies: SKIP_SCXSIM_HOOK=1 git commit ..."
echo ""
echo "Note: re-run install.sh after pulling updates to"
echo "scx-sim/scripts/git-hooks/pre-commit to refresh the installed copy."
