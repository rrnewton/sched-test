
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

Issue Tracking
----------------------------------------

Each sub-project has its own `.beads/` directory for local issue tracking with
minibeads (`mb`). When working in a sub-project, `mb` automatically uses that
project's `.beads/` directory.

**NEVER edit `.beads/` files directly.** Always use the `mb` CLI to create,
update, and close issues. Direct edits produce corrupt issue files (wrong
filename format, missing metadata) that break the tracker.
