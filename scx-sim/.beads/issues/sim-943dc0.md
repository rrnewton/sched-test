---
title: Investigate BPF instruction limit enforcement in simulator
status: open
priority: 2
issue_type: feature
created_at: 2026-02-28T14:10:04.689095843+00:00
updated_at: 2026-02-28T14:10:04.689095843+00:00
---

# Description

BPF programs have an instruction limit (1M verified insns, but runtime
execution is bounded by the verifier's path analysis). Post-JIT, this
translates to a bounded number of native instructions per structop
invocation.

Questions to investigate:
- How exactly does BPF count instructions (pre-JIT vs post-JIT)?
- What is the effective upper bound on native instructions per callback?
- Could we enforce an analogous bound in the simulator (e.g., via RBC)?
- Would this give us a natural upper bound on how long a structop can
  "run ahead" in the concurrent interleaving model?

This would complement the concurrent window design by providing a
principled bound on structop execution duration.
