#!/usr/bin/env bash
# TIER 2 of the nightly scx pin bump: turn an escalation issue into a woken agent.
#
# WHY THIS IS A SEPARATE SCRIPT AND NOT A WORKFLOW STEP
# ------------------------------------------------------
# The obvious design — have the nightly workflow call an agent inline when the
# bump goes red — is the design that was already tried, and it produced 41 runs
# and zero successes over five weeks. `anthropics/claude-code-action@v1` requires
# the Claude Code GitHub App to be INSTALLED on the repository, and on an
# org-managed repo that is not something the workflow can arrange for itself:
#
#     App token exchange failed: 401 Unauthorized -
#     Claude Code is not installed on this repository.
#
# An API key does not satisfy it; the key was present on every one of those runs.
#
# So the escalation is split in two, and the halves are independent:
#
#   1. The workflow files a GitHub ISSUE labelled `scx-pin-bump`. Durable,
#      visible where people already look, and it does not care whether any agent
#      runner exists anywhere.
#   2. THIS script, run on a machine that actually has an agent runtime, turns
#      open issues into woken agents.
#
# If (2) never runs, (1) is still a standing, visible signal — which is strictly
# better than the previous state, where the only signal was a red X on a mirror
# nobody watches. The failure mode of this design is "a human notices a day
# late", not "eight weeks of silent drift".
#
# WHY agentcloud AND NOT A SCHEDULER
# -----------------------------------
# agentcloud (`agentcloudctl`) is a session orchestrator — create / spawn / run /
# send / notify / wait. It has no cron verb and cannot schedule anything, so it
# is the wrong tool for tier 1 and the right one for tier 2. Scheduling stays in
# GitHub Actions, where it is already proven to fire.
#
# USAGE
#   scripts/wake_pin_bump_agent.sh [--dry-run] [--repo OWNER/NAME]
#
# Intended to be run from a machine with an agent runtime, either by hand when
# someone sees the red check, or from a local timer. It is idempotent per issue:
# an issue already marked as dispatched is skipped, so re-running is safe.
set -o errexit
set -o nounset
set -o pipefail

REPO="${SCX_PIN_BUMP_REPO:-facebookexperimental/sched-test}"
DRY_RUN=0
DISPATCHED_MARKER="agent-dispatched"

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --repo) REPO="$2"; shift 2 ;;
        -h|--help) sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# GitHub access on this host may require the proxy wrapper; use it when present
# so this works both on a dev box behind fwdproxy and on a plain machine.
GH=(gh)
if command -v with-proxy >/dev/null 2>&1; then
    GH=(with-proxy gh)
fi

if ! command -v gh >/dev/null 2>&1; then
    echo "ERROR: gh is not installed; cannot read escalation issues." >&2
    exit 2
fi

echo "=== scx pin bump: escalation sweep on $REPO ==="

ISSUES=$("${GH[@]}" issue list --repo "$REPO" --state open --label scx-pin-bump \
             --json number,title,labels --jq \
             ".[] | select([.labels[].name] | index(\"$DISPATCHED_MARKER\") | not) | .number" \
         || true)

if [ -z "$ISSUES" ]; then
    echo "No undispatched scx-pin-bump issues. Nothing to wake."
    exit 0
fi

if ! command -v agentcloudctl >/dev/null 2>&1; then
    # Be explicit that the issues are still there and still need a human. A
    # missing agent runtime must not read as "handled".
    echo "WARNING: agentcloudctl is not installed on this host, so no agent can be woken here." >&2
    echo "         These issues still need attention:" >&2
    for n in $ISSUES; do echo "           $REPO#$n" >&2; done
    exit 1
fi

for NUM in $ISSUES; do
    TITLE=$("${GH[@]}" issue view "$NUM" --repo "$REPO" --json title --jq .title)
    URL=$("${GH[@]}" issue view "$NUM" --repo "$REPO" --json url --jq .url)
    echo "--- waking an agent for #$NUM: $TITLE"

    PROMPT="You are picking up an automated scx submodule pin bump that failed.

Issue: $URL
Title: $TITLE

Read the issue first — it names the stage that failed, the current and target
pins, and the branch where the mechanical bump's work was preserved.

Rules that bind you here, from the harness CLAUDE.md:

- Work in your OWN worktree created through worktrees/wrkslots. Never mutate the
  primary checkout sched-test1/, and register the slot (commit + push the parent
  registry) before your first source commit.
- The scx pin legitimately carries local patches. The invariant is NOT that the
  pin is an ancestor of upstream main — that is false every day. It is that the
  pin's BASE shares history with upstream, and that every commit the pin carries
  on top of that base is one we put there deliberately. Verify with:
      BASE=\$(git -C scx merge-base HEAD origin/main)
      git -C scx log --oneline \"\$BASE\"..HEAD
  Never resolve a pin problem with a bare 'git checkout origin/main' in the
  submodule — it silently deletes those patches.
- Use scripts/scx_pin_bump.sh to do the bump rather than doing it by hand; it
  verifies carried patches by subject and count and refuses instead of dropping
  one. Its --target flag walks the bump in stages when a single jump conflicts.
- Run the WHOLE of scx-sim/validate.sh, not a subset, at the feature sets it
  uses. A partial green does not license landing.
- You own this until it LANDS. Push to BOTH origin and mirror, open the PR
  against integration, and land it yourself. Close the issue only once the bump
  is on integration, not when a fix is merely written.

Fix the failure, land the bump, then close $URL with a summary of what upstream
change broke us and what you did about it."

    if [ "$DRY_RUN" -eq 1 ]; then
        echo "    DRY RUN — would create an agentcloud session with this prompt:"
        echo "$PROMPT" | sed 's/^/      /'
        continue
    fi

    if SESSION=$(agentcloudctl create \
                    --prompt "$PROMPT" 2>/dev/null); then
        echo "    session: $SESSION"
        # Label the issue so the next sweep does not wake a second agent onto
        # the same failure. Do this only after the session actually exists.
        "${GH[@]}" label create "$DISPATCHED_MARKER" --repo "$REPO" \
            --description "An agent has been woken for this issue" \
            --color 0E8A16 2>/dev/null || true
        "${GH[@]}" issue edit "$NUM" --repo "$REPO" --add-label "$DISPATCHED_MARKER" >/dev/null
        "${GH[@]}" issue comment "$NUM" --repo "$REPO" \
            --body "[orc, claude-opus-5] Agent session \`$SESSION\` woken for this failure by \`scripts/wake_pin_bump_agent.sh\`."
        echo "    issue #$NUM marked $DISPATCHED_MARKER"
    else
        echo "    ERROR: could not create an agent session for #$NUM; leaving it undispatched." >&2
    fi
done

echo "=== sweep complete ==="
