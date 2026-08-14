---
title: bug_finding/README.md + docs cite ./bug_finding/stress.sh which does not exist (renamed to stress.py)
status: open
priority: 3
issue_type: task
created_at: 2026-08-12T14:13:01.250454200+00:00
updated_at: 2026-08-12T14:13:01.250454200+00:00
---

# Description

Found during `investigate-14-skipped-tests`.

bug_finding/README.md references `./bug_finding/stress.sh` in 4 places (lines ~21,24,27,30: bare, --jobs 4, --once, --verbose). That file does not exist anywhere in the repo. The actual tool is `bug_finding/stress.py`.

Impact: this stale pointer is what kept stress_random_lavd / _mitosis / _simple orphaned. Their doc comments said 'run via ./bug_finding/stress.sh', that runner was gone, stress.py does NOT run those tests (it drives the scxsim binary directly), and nothing in the 6 CI workflows / validate.sh / Makefile runs 'cargo test --ignored'. So three passing tests were executed by nobody.

The test-side half is already fixed (the three #[ignore]s were removed and the doc comments corrected — they now run in the default suite, verified 10/10 stable). This bead covers the remaining doc rot: update README.md to cite stress.py with its real flags.
