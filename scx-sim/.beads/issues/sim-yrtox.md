---
title: Simulator::run prints an end-of-run report to stdout on every run, with no way for a library caller to turn it off
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.604299342+00:00
updated_at: 2026-09-25T03:43:12.604299342+00:00
---

# Description

Simulator::run prints an end-of-run report to stdout on every run, with no way for a library caller to turn it off. For an in-process consumer such as ktstr, this is library output mixed into the embedder's own stdout.

The run path ends by calling print_simulation_summary, then preempt::print_structop_summary, then print_preemption_stats (safe/engine.rs). All three use println!. print_simulation_summary is also called on the init_task failure path. In the release-candidate scratch consumer at integration 24d864c6, a 1 s, 4-CPU tickless run wrote 17 lines to stdout before the consumer's own output:
- 'Simulation complete:' and five stat lines.
- 'Sched_ext structop summary:', a table header, and one row per CPU plus a total row, so the report grows with the CPU count.

print_preemption_stats adds four more lines when RBC preemption is active. stderr was empty. The consumer had to put its machine-readable result on the last line of stdout so that it could be found after the report.

The information is useful, but it belongs to the CLI. The scxsim binary can keep printing it, while the library returns it.

Fix: move the three calls out of Simulator::run into the CLI (src/bin/scxsim). Trace::summary() already carries part of the data. Add the rest to Trace (the per-CPU structop and kfunc counts, and the preemption stats). If the library still wants a log line, emit it at tracing debug level.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
