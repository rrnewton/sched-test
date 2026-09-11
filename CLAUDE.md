
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

Check your toolchain before you trust a local validate
----------------------------------------

**One command, from anywhere in the repo:**

    rustup show active-toolchain

It must say `1.97.1 ... (overridden by .../rust-toolchain.toml)`. If it says
`stable (default)` or names any other version, **your local validate is not CI's
validate** and a green result means nothing. The usual cause is a branch that
predates the pin (`53f2e1f`, landed 2026-08-12 in #77) and therefore has no
`rust-toolchain.toml`: rustup then silently falls back to your machine default.
Rebase onto `integration` and re-check.

### Why this rule exists

For roughly three weeks — **2026-07-23 to 2026-08-12** — "I validated locally"
and "CI passes" silently meant different things. CI's clippy was already
**1.97.0** on 2026-07-23 (run `29969561399` cites `rust-clippy/rust-1.97.0`),
while no toolchain was pinned, so each agent validated against whatever their
box happened to have — 1.96 on at least one. Lints CI had been rejecting for
weeks were invisible locally.

That is not a lint problem, it is a **licence problem**. The standing clearance
to land without waiting for GH CI is conditional on validating locally at the
current tip, and that condition assumes local and CI are the same gate. For that
window they were not.

**The divergence is closed** for any checkout containing the pin: local and CI
both resolve `1.97.1`, verified on both sides. It is **not** closed for a branch
that predates it — those still fall back to the machine default, so the first
thing to do on a stale branch is rebase, not diagnose.

### What it looks like when you hit it

`validate.sh` exits **101 on clippy, before a single test runs**. It reads like a
test failure and is not one. Recognise it by the stage rather than the exit
code: the last thing printed is `=== Running cargo clippy ===`.

Do **not** read this as "the toolchain moved and broke my branch". CI would have
rejected the same code at any point since 2026-07-23. The pin did not create the
failure; it made it visible before you push, which is the improvement.

### Did anything land broken during the window?

Cheap to check and worth knowing: **no surviving violations.** 37 merges and 117
commits landed in that window, and `cargo clippy --all-targets --workspace -D
warnings` on the current tip under 1.97.1 is **clean, exit 0**. So nothing landed
that both violated 1.97 and is still violating it. Whether an individual merge
was momentarily red and fixed later is not answerable without re-running clippy
at each of the 37 merge points, and the tip being clean makes that mostly
academic.

CI: scheduled workflows run against integration
----------------------------------------

**GitHub resolves `on: schedule:` from the DEFAULT BRANCH only.** The default
branch is `main`, while the development tip remains `integration` between
imports to main.

`.github/workflows/scheduled-dispatch.yml` is the sole nightly trigger for
`sync-upstream.yml` and `scx-pin-staleness.yml`. It runs on main and dispatches
both workflows explicitly on integration, keeping the nightly checks aligned
with the development tip. Each workflow also supports `workflow_dispatch:`.

**For a new nightly job:** add its filename to the carrier's matrix and declare
`workflow_dispatch:` in the job's workflow. The carrier change must reach main
before it can run on a schedule. Do not add a second `schedule:` to a workflow
already in the matrix: that would be dormant on integration and start a
duplicate run when the workflow is imported to main.

Two things worth knowing about that arrangement:

- The dispatcher is deliberately inert — a list of filenames and a dispatch
  call, no job logic.
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
