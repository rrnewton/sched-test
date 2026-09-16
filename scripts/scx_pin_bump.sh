#!/usr/bin/env bash
# Mechanically advance the `scx` submodule pin, replaying any local patches the
# pin carries onto the new upstream. No agent, no model, no tokens.
#
# WHY THIS IS A SCRIPT AND NOT A WORKFLOW STEP
# --------------------------------------------
# The bump logic used to live inline in `.github/workflows/sync-upstream.yml`,
# where it could only ever be exercised by waiting for 04:10 UTC and reading a
# log. That is the worst possible place for the one step whose failure mode is
# SILENTLY DELETING OUR PATCHES. As a script it runs identically on a laptop, in
# CI, and under an agent that has been woken to fix a failed bump, and the
# refusal paths below can be tested directly (see `--simulate-lost-patch`).
#
# THE INVARIANT, STATED CORRECTLY
# --------------------------------
# The pin is *supposed* to sit ahead of upstream. It legitimately carries local
# commits — today exactly one, `lib/cgroup_bw: add scxsim targeted yield hooks`.
# So the naive invariant
#
#     the pin must be an ancestor of upstream main        # WRONG, false daily
#
# is false every single day, and a job encoding it either fires constantly or
# gets "fixed" by deleting the patches. The true invariant is
#
#     the pin's BASE (merge-base with upstream) must be an ancestor of upstream
#
# which is near-tautological on its own, so it is NOT the gate either. THE GATE
# IS: list what the pin carries on top of its base, and confirm every one of
# those commits survives the bump. That is what this script enforces, by subject
# AND by count, and it REFUSES rather than proceeding when they do not match.
#
# A bare `git checkout origin/main` in the submodule silently discards every
# carried patch. That is not a hypothetical: it is the shape this script exists
# to make impossible.
#
# EXIT CODES
#   0   bumped; submodule is at the new pin and staged in the superproject
#   10  already up to date; nothing staged, nothing to do (NOT an error)
#   1   REFUSED: rebase conflict, or a carried patch would be lost
#   2   cannot run (missing submodule, bad ref, git failure)
set -o errexit
set -o nounset
set -o pipefail

UPSTREAM_REF="${SCX_UPSTREAM_REF:-origin/main}"
SUBMODULE="scx"
TARGET=""
DO_FETCH=0
SIMULATE_LOST_PATCH=0

usage() {
    sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'USAGE'

usage: scripts/scx_pin_bump.sh [options]

  --fetch                 fetch the submodule's upstream remote first (CI passes this)
  --upstream REF          upstream ref to bump toward (default: origin/main)
  --target SHA            bump to this exact commit instead of the upstream tip.
                          This is the STAGED-WALK knob: when the pin is far
                          behind, step it through intermediate upstream commits
                          so a failure names a small range instead of hundreds.
  --simulate-lost-patch   self-test: force the lost-patch refusal path and exit 1
                          without touching the real pin. Proves the guard fires.
  -h, --help              this text
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --fetch) DO_FETCH=1; shift ;;
        --upstream) UPSTREAM_REF="$2"; shift 2 ;;
        --target) TARGET="$2"; shift 2 ;;
        --simulate-lost-patch) SIMULATE_LOST_PATCH=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# Emit a GitHub annotation in Actions, plain text everywhere else. Same
# convention as scripts/check_scx_pin_staleness.py, deliberately: these two are
# read together when a bump goes wrong.
annotate() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::$1::$2"
    else
        echo "$(echo "$1" | tr '[:lower:]' '[:upper:]'): $2"
    fi
}

# Publish a key=value to $GITHUB_OUTPUT when in Actions. The workflow needs
# these to decide whether to land, and the values must come from the script
# rather than being re-derived in YAML, where they would drift.
emit() {
    echo "$1=$2"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        echo "$1=$2" >> "$GITHUB_OUTPUT"
    fi
}

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

if [ ! -e "$SUBMODULE/.git" ]; then
    annotate error "$SUBMODULE submodule is not checked out at $REPO_ROOT/$SUBMODULE"
    exit 2
fi

cd "$SUBMODULE"

if [ "$DO_FETCH" -eq 1 ]; then
    REMOTE="${UPSTREAM_REF%%/*}"
    git fetch --quiet "$REMOTE" || {
        annotate error "could not fetch $REMOTE in $SUBMODULE"
        exit 2
    }
fi

PIN=$(git rev-parse HEAD)
if ! NEW=$(git rev-parse --verify "${TARGET:-$UPSTREAM_REF}^{commit}" 2>/dev/null); then
    annotate error "cannot resolve ${TARGET:-$UPSTREAM_REF} in $SUBMODULE"
    exit 2
fi

BASE=$(git merge-base "$PIN" "$NEW" 2>/dev/null || true)
if [ -z "$BASE" ]; then
    # This is the P0 the staleness alarm also watches for. Syncing later cannot
    # fix it; the history the pin descends from is gone upstream.
    annotate error "P0: pin ${PIN:0:12} shares NO history with ${TARGET:-$UPSTREAM_REF}. Upstream rebased or dropped the commit it descends from."
    exit 2
fi

# THE GATE. Everything the pin carries on top of the shared base. Empty is
# normal and fine; a non-empty list is ALSO normal — what matters is that this
# exact list survives the bump.
mapfile -t CARRIED_SUBJECTS < <(git log --reverse --format=%s "$BASE".."$PIN")
CARRIED=${#CARRIED_SUBJECTS[@]}

BEHIND=$(git rev-list --count "$BASE".."$NEW")

echo "=== scx pin bump ==="
echo "  submodule      : $SUBMODULE"
echo "  current pin    : ${PIN:0:12}"
echo "  shared base    : ${BASE:0:12}"
echo "  bumping toward : ${TARGET:-$UPSTREAM_REF} = ${NEW:0:12}"
echo "  upstream ahead : $BEHIND commit(s)"
echo "  carried locally: $CARRIED commit(s)"
for s in ${CARRIED_SUBJECTS+"${CARRIED_SUBJECTS[@]}"}; do
    echo "      carried: $s"
done
echo "===================="

emit old_pin "$PIN"
emit old_pin_short "${PIN:0:12}"
emit new_pin "$NEW"
emit new_pin_short "${NEW:0:12}"
emit carried_count "$CARRIED"
emit upstream_commits "$BEHIND"

if [ "$PIN" = "$NEW" ]; then
    echo "pin already at ${NEW:0:12}; nothing to do."
    emit bumped false
    exit 10
fi

if [ "$BEHIND" -eq 0 ] && [ "$SIMULATE_LOST_PATCH" -eq 0 ]; then
    echo "no new upstream commits beyond the shared base; nothing to do."
    emit bumped false
    exit 10
fi

# Work on a detached HEAD so a failure can never leave a branch in the submodule
# pointing somewhere surprising.
git checkout --quiet --detach "$PIN"

if [ "$CARRIED" -eq 0 ]; then
    echo "pin carries no local commits; fast-forwarding to ${NEW:0:12}."
    git checkout --quiet --detach "$NEW"
else
    echo "replaying $CARRIED local commit(s) onto ${NEW:0:12}..."
    # A rebase identity must exist or the rebase dies with `fatal: empty ident
    # name`. Set it only if absent, and only for this submodule, so we neither
    # clobber a real identity nor depend on one having been configured globally.
    if ! git config user.email >/dev/null 2>&1; then
        git config user.email "scx-pin-bump@localhost"
        git config user.name "scx pin bump (mechanical)"
    fi
    if ! git rebase --onto "$NEW" "$BASE" "$PIN" >/tmp/scx-rebase.$$ 2>&1; then
        cat /tmp/scx-rebase.$$ || true
        git rebase --abort >/dev/null 2>&1 || true
        git checkout --quiet --detach "$PIN"
        rm -f /tmp/scx-rebase.$$
        annotate error "REFUSING TO BUMP: the $CARRIED local scx commit(s) do not rebase cleanly onto ${NEW:0:12}. The pin is unchanged. A human or an agent must resolve the conflict; nothing was discarded."
        emit bumped false
        emit refused conflict
        exit 1
    fi
    rm -f /tmp/scx-rebase.$$
fi

# ---------------------------------------------------------------------------
# VERIFY THE PATCHES SURVIVED. This is the whole point of the script.
#
# Count alone is not enough: a rebase silently DROPS a commit that becomes empty
# because the same change landed upstream. That is sometimes legitimate, but it
# must never happen unnoticed, because the alternative reading — we just deleted
# a local scheduler patch — is indistinguishable without looking. So compare
# subjects, in order, and refuse on any mismatch.
# ---------------------------------------------------------------------------
mapfile -t REPLAYED_SUBJECTS < <(git log --reverse --format=%s "$NEW"..HEAD)
REPLAYED=${#REPLAYED_SUBJECTS[@]}

if [ "$SIMULATE_LOST_PATCH" -eq 1 ]; then
    # Self-test hook: pretend a patch vanished, so the refusal path below is
    # exercised for real rather than trusted. Restores the pin on the way out.
    REPLAYED_SUBJECTS=("deliberately-corrupted-by-simulate-lost-patch")
    REPLAYED=$(( CARRIED > 0 ? CARRIED - 1 : 1 ))
fi

lost_patch_refusal() {
    git checkout --quiet --detach "$PIN"
    annotate error "REFUSING TO BUMP: expected $CARRIED local commit(s) after replay, found $REPLAYED. $1 The pin is unchanged and NOTHING was discarded — resolve by hand."
    echo "  expected:"
    for s in ${CARRIED_SUBJECTS+"${CARRIED_SUBJECTS[@]}"}; do echo "    - $s"; done
    echo "  found:"
    for s in ${REPLAYED_SUBJECTS+"${REPLAYED_SUBJECTS[@]}"}; do echo "    - $s"; done
    emit bumped false
    emit refused lost-patch
    exit 1
}

if [ "$REPLAYED" -ne "$CARRIED" ]; then
    lost_patch_refusal "A carried patch was dropped, most likely because it became empty against the new upstream."
fi

i=0
while [ "$i" -lt "$CARRIED" ]; do
    if [ "${CARRIED_SUBJECTS[$i]}" != "${REPLAYED_SUBJECTS[$i]}" ]; then
        lost_patch_refusal "Replayed commit $((i + 1)) is '${REPLAYED_SUBJECTS[$i]}' but should be '${CARRIED_SUBJECTS[$i]}'."
    fi
    i=$((i + 1))
done

BUMPED=$(git rev-parse HEAD)
NEW_BASE=$(git merge-base "$BUMPED" "$NEW")
if [ "$NEW_BASE" != "$NEW" ]; then
    lost_patch_refusal "After replay the pin's base is ${NEW_BASE:0:12}, not the intended ${NEW:0:12}."
fi

cd "$REPO_ROOT"
git add "$SUBMODULE"

echo
echo "=== bump complete ==="
echo "  pin: ${PIN:0:12} -> ${BUMPED:0:12}"
echo "  absorbed $BEHIND upstream commit(s); replayed $CARRIED local commit(s) intact:"
for s in ${CARRIED_SUBJECTS+"${CARRIED_SUBJECTS[@]}"}; do echo "      $s"; done
echo "  submodule staged in the superproject; NOT committed."
echo "====================="

emit bumped true
emit bumped_pin "$BUMPED"
emit bumped_pin_short "${BUMPED:0:12}"
exit 0
