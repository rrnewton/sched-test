---
title: rt-app ingest ignores 'delay', so start_time_ns only round-trips one way
status: open
priority: 3
issue_type: task
created_at: 2026-09-11T14:31:57.665202243+00:00
updated_at: 2026-09-11T14:31:57.665202243+00:00
---

# Description

Found while making scenario_to_rtapp_json stop dropping fields.

The exporter now emits rt-app's 'delay' (microseconds before a task's first loop) from TaskDef::start_time_ns, so a scenario with staggered task starts staggers on hardware too. The INGEST side does not read it: 'delay' is in TASK_PHASE_KEYS, which means parse_events skips it as a non-event, and nothing else looks at it. So load_rtapp() gives every task start_time_ns == 0 regardless.

Consequence: a spec written with 'delay' simulates as if every task started at t=0, and a Scenario -> rt-app -> Scenario round trip loses the stagger.

NOT fixed with the exporter change, deliberately. Parsing 'delay' changes the meaning of every existing spec that carries one — those tasks would start later than they do today — which is a timing change that wants its own look at what depends on it, not a rider on a workload-language PR.

TO CLOSE: parse 'delay' (usec) into TaskDef::start_time_ns in parse_task, and check nothing in workloads/, bug_finding/stress.py or the experiment fixtures depends on the current start-at-zero behaviour. Note rt-app applies 'delay' once before the first loop iteration, not per iteration.
