---
title: test_lavd_cpu_capacity_scaling fails with cpumask violation
status: closed
priority: 1
issue_type: task
created_at: 2026-02-21T03:08:17.039791084+00:00
updated_at: 2026-03-03T19:30:05.679488852+00:00
closed_at: 2026-03-03T19:30:05.679488751+00:00
---

# Description

Task 4 has allowed_cpus=[CPU 3], but LAVD dispatches it to CPU 0 via SCX_DSQ_LOCAL_ON. Error: 'SCX_DSQ_LOCAL_ON cannot dispatch task 4 to CPU 0 (not in cpumask)'. Needs investigation.
