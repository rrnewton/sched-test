---
title: 'scxsim-workload-ir: YieldNotRepresentable refusal is obsolete and its message is false (scx_simulator::Phase has Yield)'
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T00:19:31.020336917+00:00
updated_at: 2026-09-25T00:19:31.020336917+00:00
---

# Description

ingest.rs to_scenario refuses Phase::Yield with IngestError::YieldNotRepresentable. Its Display text says "scx_simulator::Phase is Run|Sleep|Wake with no yield ... Closing this means adding a Yield phase to the simulator."

scx_simulator::Phase::Yield exists; the crate's own test code notes this at the SimPhase::Yield arm. So the message sends a user to add something that already exists. The module doc ("Phase is Run | Sleep | Wake") is stale too.

Decide before scxsim-workload-ir 0.1.0 is published, because the variant is public API:
- lower Phase::Yield to scx_simulator::Phase::Yield, if the semantics match (check what the IR's yield means against the engine's Yield); or
- keep the refusal and correct the message to say why a yield cannot be lowered.
