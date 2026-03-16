---
title: 'validate.sh blocked: nextest hangs on 2 preemptive interleave tests'
status: closed
priority: 0
issue_type: bug
created_at: 2026-03-14T11:15:11.243863547+00:00
updated_at: 2026-03-14T11:17:07.688708778+00:00
closed_at: 2026-03-14T11:17:07.688708708+00:00
---

# Description

validate.sh cannot complete because cargo nextest run --workspace includes two tests (test_preemptive_custom_timeslice and test_preemptive_pmu_determinism) that hang indefinitely. This blocks the CI/validation workflow. Either the tests need a timeout, need to be marked #[ignore], or validate.sh needs a nextest filter to exclude them. This is a release blocker because developers cannot run the validation script.
