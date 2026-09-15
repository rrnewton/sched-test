---
title: 'repm: ''repm run'' does not produce combined_results.csv'
status: open
priority: 2
issue_type: bug
labels:
- post-1.0
created_at: 2026-04-27T10:57:23.379736163+00:00
updated_at: 2026-04-27T10:57:23.379736163+00:00
---

# Description

Source: rcpipe rc-test-full-pipeline (2026-04-27).

Symptom: After 'repm run', no combined_results.csv is produced. Downstream 'repm analyze' cannot find per-rep CSVs to compare. Manual aggregation needed.

Action: locate the run-completion path; emit combined_results.csv at run end. Schema should be inferable from analyze's reader.

Verify: 'repm run ...' followed by 'repm analyze ...' on the same run completes without manual file marshaling.

Classification: post-1.0-candidate.
Migrated from tg bug-repm-no-combined-results-csv.
