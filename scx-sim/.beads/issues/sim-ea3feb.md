---
title: 'scxsim: TaskCompleted double-emitted when a task''s final phase is Sleep'
status: open
priority: 3
issue_type: bug
created_at: 2026-07-23T04:09:57.405767838+00:00
updated_at: 2026-07-23T04:09:57.405767838+00:00
---

# Description

A finite task (RepeatMode::Once or Count(k)) whose LAST phase is Phase::Sleep records TWO TraceKind::TaskCompleted events at the identical timestamp; a task whose last phase is Phase::Run records exactly one. Repro (tg test-concurrent-task-lifecycle): a Count(k) task with phases [Run 2ms, Sleep 1ms] emits 2 completions for every k in {1,2,3,4} (both at the same time_ns); the same task with phases [Run 2ms] (Run-only) emits 1. Root cause: the terminal Sleep-phase completion is recorded on two paths — the sleep/timer completion path and the phase-advance (advance_phase()->Exited) path in engine.rs (see handle_task_phase_complete ~3750 and the Wake/advance path ~3994 which sets state=Exited + records TaskCompleted). Impact: purely a trace-event duplication (scheduling behavior unaffected; both events share a timestamp), but it corrupts any analysis that COUNTS TaskCompleted events (e.g. completion accounting, throughput metrics). InitTask/Enable/ExitTask are unaffected (exactly one per task). Fix: record TaskCompleted exactly once at the single point a task transitions to Exited, regardless of whether the terminal phase is Run or Sleep. Workaround used in task_lifecycle_concurrent.rs: dedup completions by pid (distinct_completed) and only assert exact counts for tasks that terminate on a Run phase.
