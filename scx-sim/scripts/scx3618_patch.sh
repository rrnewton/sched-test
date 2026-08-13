#!/usr/bin/env bash
# Apply / revert the scx#3618 patch that is carried out-of-pin under
# scx-sim/patches/scx3618/. See that directory's PROVENANCE.md for why the
# change is a patch file rather than a submodule pin bump.
#
# This exists so a before/after measurement has a build state a reader can
# reconstruct. Applying the patch by hand into the submodule working tree is
# what produced the withdrawn conclusion on PR sched-test#104.
set -euo pipefail

cd "$(dirname "$0")/../.."          # sched-test root
SCX=scx
PIN=59c30baee7d7a70f32f3983cfb3fe2f383a144b0
PATCH=scx-sim/patches/scx3618/scx3618-vs-pin-59c30bae.patch
FILES=(lib/cgroup_bw.bpf.c scheds/include/lib/cgroup.h scheds/rust/scx_lavd/src/bpf/main.bpf.c)
MARKER=scx_cgroup_bw_pressure

die() { echo "scx3618_patch: $*" >&2; exit 1; }

[ -f "$PATCH" ] || die "patch not found at $PATCH"

head=$(git -C "$SCX" rev-parse HEAD)
[ "$head" = "$PIN" ] || die "scx HEAD is $head, expected pin $PIN.
The carried patch is generated against that exact pin and is not valid
elsewhere. Regenerate it (see patches/scx3618/PROVENANCE.md) rather than
forcing it on."

# Applied-ness is decided by the marker symbol in the source, not by whether
# the tree is dirty: an unrelated edit would otherwise read as "patched".
#
# The `|| applied=0` is load-bearing. grep exits 1 when the marker is absent —
# the CLEAN case — and under `set -e` with `pipefail` that aborts the script
# with no output at all, which reads exactly like success to any caller that
# does not check the status. Do not remove it.
applied=$(grep -l "$MARKER" "${FILES[@]/#/$SCX/}" 2>/dev/null | wc -l) || applied=0

case "${1:-status}" in
  status)
    if [ "$applied" -gt 0 ]; then echo "source: PATCHED (scx#3618)"; else echo "source: clean at pin"; fi
    echo "scx HEAD:  $head"
    echo "modified:  $(git -C "$SCX" status --porcelain | wc -l) file(s)"
    echo "gitlink:   $(git ls-tree HEAD scx | awk '{print $3}')"
    ;;
  apply)
    [ "$applied" -eq 0 ] || die "already applied; revert first"
    dirty=$(git -C "$SCX" status --porcelain | wc -l)
    [ "$dirty" -eq 0 ] || die "scx tree has $dirty modified file(s); refusing to apply onto a dirty tree"
    git -C "$SCX" apply "../$PATCH"
    echo "applied. REBUILD before measuring, then confirm with:"
    echo "  scx-sim/scripts/verify_scx3618_build_state.sh"
    ;;
  revert)
    git -C "$SCX" restore --source=HEAD --staged --worktree "${FILES[@]}"
    echo "reverted to pin. REBUILD before measuring."
    ;;
  *)
    die "usage: $0 {status|apply|revert}"
    ;;
esac
