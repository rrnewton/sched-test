---
title: 'scxsim-workload-ir: mark IngestError and LoweringError #[non_exhaustive] before 0.1.0 is published'
status: open
priority: 2
issue_type: task
labels:
- cratesio
created_at: 2026-09-25T00:19:31.023131157+00:00
updated_at: 2026-09-25T00:19:31.023131157+00:00
---

# Description

Both are exhaustive public enums, so every variant added after publication is a breaking change and needs a 0.2.0. scx_simulator's LoadError is already #[non_exhaustive].

IngestError has 7 variants today. One of them, YieldNotRepresentable, is already slated to change (see the Yield issue). Mark both enums before the first publish; afterwards, doing so is itself a breaking change.
