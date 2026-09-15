---
title: 'gh-pages workflow: triggers on abandoned branch simulator.v6 AND its last 3 runs all failed'
status: open
priority: 2
issue_type: bug
created_at: 2026-08-12T23:48:49.556133858+00:00
updated_at: 2026-08-12T23:48:49.556133858+00:00
---

# Description

Two independent failures stacked, which is why neither was noticed.

1. STALE TRIGGER. gh-pages.yml fires on pushes to 'simulator.v6'. The development tip moved to 'integration' long ago, so the workflow has not fired since 2026-06-05. The scxsim guide has therefore not been rebuilt or published in over two months.

2. IT WAS ALREADY FAILING. Its last THREE runs, all on 2026-06-05, all have conclusion 'failure'. So even before the trigger went stale it was not publishing. Nobody saw, because a failing deploy of a docs site produces no signal anyone watches — no test goes red, no PR is blocked.

The workflow's own header comment names the likely cause: 'Pages source for this repo MUST be set to "GitHub Actions" (not the legacy branch-based source) for this workflow's deploy-pages step to succeed.' That is a repository SETTING, not code, so it cannot be fixed in a PR.

WHY THIS IS THE ARCHETYPE FOR THIS CLASS: fixing only the trigger (simulator.v6 -> integration) would make it fire again and fail again, converting a silent no-op into recurring red noise on every docs push. Both halves have to be fixed together, and the settings half needs someone with repo admin.

STEPS:
1. Owner: set Pages source to 'GitHub Actions' at repo settings, or via
   gh api repos/facebookexperimental/sched-test/pages -X POST.
2. Then change the trigger branch from simulator.v6 to integration.
3. Confirm one successful deploy before closing.

Until 1 is done, leaving the trigger stale is arguably the lesser evil, so this is filed rather than half-fixed.

# Acceptance Criteria

- Pages source configured for GitHub Actions.
- Trigger branch updated to the current dev tip.
- One green run with the guide actually published.
