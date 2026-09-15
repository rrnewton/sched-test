---
title: 'sync-upstream.yml nightly cron has NEVER fired — schedule: only runs from the default branch'
status: open
priority: 2
issue_type: bug
created_at: 2026-08-12T23:48:49.552543207+00:00
updated_at: 2026-08-12T23:48:49.552543207+00:00
---

# Description

gh run list for sync-upstream.yml returns NO RUNS EVER. Not 'failing', not 'skipped' — it has literally never executed since it was written.

MECHANISM: GitHub fires 'schedule:' (cron) triggers ONLY from the repository's DEFAULT branch. The default branch is 'main', and main's .github/workflows/ contains exactly one file:

    git ls-tree -r --name-only origin/main -- .github/workflows/
    .github/workflows/ci.yml

sync-upstream.yml lives on integration (and feature branches), never on main. Its '0 4 * * *' cron therefore has no branch to fire from. workflow_dispatch still works, which is presumably why nobody noticed — a manual run behaves normally.

This is the purest instance of the class: a check that produces NO signal at all, not even a red X, because it never starts.

SAME EXPOSURE FOR EVERY OTHER WORKFLOW: main has only ci.yml, so simulator.yml, gh-pages.yml, scxsim-examples.yml, scxsim-quickstart.yml and scx-pin-staleness.yml are also absent there. Those are push/pull_request-triggered, which DO fire from the branch under test, so they work — but any 'schedule:' added to any of them in future will silently never run for the same reason.

FIX OPTIONS:
1. Land the workflow set on main. Cleanest, and makes future cron work, but main is protected and this needs the owner.
2. Move the sync to an external scheduler.
3. Accept manual-only and DELETE the cron block, so the file does not advertise a nightly sync that cannot happen. Cheapest honest option — a schedule that cannot fire is a false claim in the file.

Recommend 3 now and 1 when someone is touching main anyway. Do not leave the cron in place unexplained: the file currently reads as though upstream sync is automated nightly, and it is not.

# Acceptance Criteria

- Either the cron fires (workflow present on the default branch) or the cron block is removed and the file states that sync is manual.
- A note in the workflow explaining the default-branch rule, so the next person does not re-add a cron that cannot run.
