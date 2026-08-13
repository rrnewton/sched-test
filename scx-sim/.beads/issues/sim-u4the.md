---
title: 'scxsim: layered SMT siblings are published but their effect on placement is untested'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T15:52:23.308423701+00:00
updated_at: 2026-08-12T15:52:23.308423701+00:00
---

# Description

tests/layered.rs::smt_siblings_are_published_only_when_smt_is_on asserts that
__sibling_cpu[] holds the right partner with SMT on and -1 with SMT off. That
is a publication check only. No test shows SMT actually changing a placement
decision, so the SMT half of "real LLC/NUMA/SMT topology" is compiles-and-runs
rather than behaviourally proven — unlike LLC, which
llc_topology_drives_dsq_selection covers with a flat-topology control arm.

What a real test needs: layered's sibling logic lives in try_preempt_cpu()
(the `nr_excl_layers && layer->excl && sibling_cpu(cand) >= 0` branch, which
bumps LSTAT_EXCL_COLLISION) and in sib_keep_idle() (bumps GSTAT_EXCL_IDLE).
Both require at least one EXCLUSIVE layer, so a test must:
  1. configure a layer with .with_exclusive(true) so nr_excl_layers > 0,
  2. contend an SMT sibling pair so the collision path is reachable,
  3. assert LSTAT_EXCL_COLLISION or GSTAT_EXCL_IDLE is non-zero,
  4. and -- the part that makes it non-vacuous -- assert the SAME workload
     with threads_per_core=1 leaves the counter at zero.

Both counters are already exposed: probes.layer_stat(id, LayerStat::..) and
probes.global_stat(GlobalStat::ExclIdle). LayerStat needs an ExclCollision
variant added (LSTAT_EXCL_COLLISION = 19).

Estimated ~0.5 agent-day. Not a blocker for the Tier-2 claim; recorded so the
claim's exact shape is auditable. Raised by the Tier-2 self-audit in tg
layered-support-implement.
