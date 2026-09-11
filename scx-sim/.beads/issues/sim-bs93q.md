---
title: rt-app ingest instance naming diverges from stock rt-app (global index, and single instances are suffixed)
status: open
priority: 2
issue_type: task
created_at: 2026-09-11T14:22:35.476786992+00:00
updated_at: 2026-09-11T14:22:35.476786992+00:00
---

# Description

MEASURED, not read. Ran ~/bin/rt-app (built from checkouts/rt-app@9eedd75) on a two-task / three-thread spec and read /proc/<pid>/task/*/comm:

  frontendd-0        <- declared "instance": 1, STILL suffixed
  iothreadpool-1     <- FIRST instance of the second task, index 1 not 0
  iothreadpool-2

rt-app's thread_data_set_unique_name() is snprintf("%s-%d", name, tdata->ind) and tdata->ind is a GLOBAL thread index (data->ind = index, rt-app_parse_config.c). Our safe/rtapp.rs parse_task() instead uses the bare name when instance == 1 and a PER-TASK index otherwise.

WHY IT MATTERS. p->comm is what MATCH_COMM_PREFIX reads, and the rt-app front end exists so the same JSON means the same thing in simulation and on hardware. Prefix rules over the base name are unaffected, which is why this went unnoticed; an exact rule, or a prefix reaching past the base name (e.g. CommPrefix("iothreadpool-0")), matches in sim and misses for real.

PINNED, NOT FIXED, by rtapp::tests::known_gap_instance_naming_diverges_from_stock_rtapp. Fixing it touches three things at once and is its own change: (1) the naming itself, (2) name_to_pid, which maps both the bare name and the suffixed form and is what resume / thread_of / parent resolve through, (3) scenario_to_rtapp_json, which emits task names as rt-app keys and would then need to emit names rt-app will itself re-suffix.

Watch out for the interaction with the irq_gen pid accounting (separate issue) — both passes over tasks_obj must agree on how many pid slots each entry consumes, and a global thread index makes that agreement load-bearing rather than incidental.
