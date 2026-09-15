---
title: LAVD stalls with complex suspend/resume workloads under cooperative interleaving
status: open
priority: 2
issue_type: bug
created_at: 2026-03-14T17:50:42.934019223+00:00
updated_at: 2026-03-14T17:50:42.934019223+00:00
---

# Description

Random workload stress test found LAVD stalls with complex multi-task suspend/resume patterns under cooperative interleaving. The stall occurs with 8 tasks using mixed phases (run/sleep/wake with multiple resume targets) and high rbc-ns (50). The fixed workloads rarely trigger this. Example: 8 tasks with interleaving suspend/resume chains, rbc-ns=50, 4 CPUs, ErrorStall on Pid(4) after 2s. The workload JSON is included in the finding report. Found by: stress.py --random-workloads, seed 924582410.
