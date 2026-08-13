
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

CI: a `schedule:` on an integration workflow will NEVER run
----------------------------------------

**GitHub resolves `on: schedule:` from the DEFAULT BRANCH only.** The default
branch is `main`, and `main` carries exactly one workflow of its own (`ci.yml`).
Every real workflow — `simulator.yml`, `gh-pages.yml`, `scxsim-examples.yml`,
`scxsim-quickstart.yml`, `sync-upstream.yml`, `scx-pin-staleness.yml` — lives on
`integration`.

So **adding `on: schedule:` to a workflow on `integration` silently does
nothing.** The workflow file looks correct, the cron expression is valid, GitHub
reports no error, and it never fires. Verified against the API: **zero scheduled
runs have ever occurred on this repository.** `sync-upstream.yml` carried a
nightly cron for months and never ran here once — which is the underlying reason
upstream scx syncs stopped for seven weeks.

**What to do instead:** add your workflow's filename to the matrix in
`.github/workflows/scheduled-dispatch.yml` **on `main`**. That file is a cron
carrier: it fires from the default branch and dispatches the real workflows on
`integration`. Your workflow must also declare `workflow_dispatch:` for the
dispatch to reach it.

Two things worth knowing about that arrangement:

- The dispatcher is deliberately inert — a list of filenames and a dispatch
  call, no logic. `main` is not a branch we develop on, so anything clever there
  would rot unseen.
- Prefer giving a scheduled workflow a `push:` trigger as well, where that makes
  sense. `scx-pin-staleness.yml` runs on both, so the cron and the push trigger
  are independent paths: if the dispatcher breaks, pushes still catch the
  problem; if pushes stop — which is exactly when drift accumulates unnoticed —
  the cron still fires.

Push- and pull_request-triggered workflows are **unaffected**: for those GitHub
uses the workflow file from the pushed ref, so they work correctly on
`integration`. Only `schedule:` resolves from the default branch.

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
