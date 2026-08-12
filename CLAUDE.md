
Multi-Project Repository
========================================

This is a multi-project repository with multiple worktrees (`sched-test1/`,
`sched-test2/`, `sched-test3/`, etc.). Each worktree contains several
sub-projects.

Project-Specific Instructions
----------------------------------------

Each sub-project has its own `CLAUDE.md` with development guidelines. When
working in a sub-project, follow its `CLAUDE.md`:

- **scx-sim/**: Scheduler simulator — see `scx-sim/CLAUDE.md`

Shipping: you own your PR until it lands
----------------------------------------

Owner policy, standing (2026-08-12). **There should be no languishing work.**

    work -> commit as you go -> rename off `agent/*` -> push (origin AND
    mirror) -> open PR against `integration` -> (optional reviewer pass)
    -> LAND IT -> PUSH THE MIRROR AGAIN

**Merging a PR on GitHub updates ORIGIN ONLY.** `rrnewton/sched-test` is not a
GitHub-native mirror, so the merge commit does not propagate: the moment your
PR merges, `mirror/integration` is behind by exactly that commit, silently.
Pushing both remotes earlier does not cover it — that was your branch, this is
the merge. After landing:

    with-proxy git fetch origin integration
    with-proxy git push mirror FETCH_HEAD:refs/heads/integration

Then compare the two SHAs with `git ls-remote` rather than assuming. Details
and the worked example are in `scx-sim/CLAUDE.md`.

Never leave uncommitted changes locally, and before going idle or finishing,
confirm in your final note that the worktree is clean, the work is pushed, and
the PR is open or landed. Pause for the owner only to land something RED, to
rewrite shared history, or to change the scx submodule pin.

Full text, including the push-target rules this depends on, is in the harness
`CLAUDE.md` (the parent workspace above this checkout) and in
`scx-sim/CLAUDE.md`.

Issue Tracking
----------------------------------------

Each sub-project has its own `.beads/` directory for local issue tracking with
minibeads (`mb`). When working in a sub-project, `mb` automatically uses that
project's `.beads/` directory.

**NEVER edit `.beads/` files directly.** Always use the `mb` CLI to create,
update, and close issues. Direct edits produce corrupt issue files (wrong
filename format, missing metadata) that break the tracker.
