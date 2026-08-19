---
title: SIGFPE handler cannot fix compiler reasoning built on division UB
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T20:10:09.939986733+00:00
updated_at: 2026-08-12T20:10:09.939986733+00:00
---

# Description

csrc/sim_sigfpe.c now reproduces the verifier's chk_and_{div,mod,sdiv,smod} semantics exactly (12/12 in csrc/tests/sim_bpf_ub_semantics_test.c). But it is a PARTIAL mitigation by construction: it fixes the value in RAX/RDX, and the compiler is entitled to optimise around the division on the assumption that it never traps, before any signal exists.

Measured at -O2 (clang 22):
  uint32_t q = ua32 / uz32;   /* uz32 volatile, zero */
  q prints as 0 (handler correct), but 'q == 0' evaluates FALSE.
Disassembly: clang rewrote 'q == 0' into 'divisor > dividend' (cmp %esi,%edi / seta %cl), valid only if the divisor is nonzero. The comparison never reads EAX.

Consequences to work through:
1. schedulers/cosmos/config.mk carries a sed patch inserting an 'interval ? ... : 0' guard at one cosmos division site. That is a symptom of this same problem, applied per-site. Decide whether to keep it, generalise it, or drop it once probe mode has been run.
2. Run SCX_SIM_UB_PROBE=1 across the full test suite to find how many division sites actually reach a zero divisor. Nobody has measured this. If the count is zero the residual risk is theoretical; if not, each site needs a source guard.
3. Consider whether source-level guards should be generated rather than hand-sed'd.

# Acceptance Criteria

SCX_SIM_UB_PROBE=1 has been run over the full suite and the reachable divide-by-zero sites are enumerated, with a decision recorded for each and for the cosmos sed patch.
