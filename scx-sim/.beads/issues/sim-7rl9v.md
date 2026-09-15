---
title: scx_layered match_substr() reads y before assignment (util.bpf.c:181)
status: open
priority: 1
issue_type: bug
created_at: 2026-08-12T20:09:57.995718517+00:00
updated_at: 2026-08-12T20:09:57.995718517+00:00
---

# Description

scx/scheds/rust/scx_layered/src/bpf/util.bpf.c:181 compares 'if (str_len - x < y)' in the outer bpf_for, but y is only assigned by the inner bpf_for at :184. On the first outer iteration y is unassigned.

This is an UPSTREAM scx bug, not a scxsim bug. Recorded here because it is the motivating instance for ai_docs/BPF_UB_FIDELITY_POLICY.md and because the two targets behave differently:

- BPF target (scx@59c30ba, -mcpu=v3): clang allocates y to r9, which at the compare holds the LIVE str_buf map pointer (spilled at insn 318). Disassembly shows 'if w1 s< w9' at insn 340. The verifier is satisfied because r9 is initialised, so the program loads — y silently takes the low 32 bits of a map-value pointer.
- x86-64 (scxsim): y picks up host stack/register residue, which CHANGES WITH BUILD FLAGS. Under coverage instrumentation the garbage made MATCH_CGROUP_CONTAINS silently return 'no match'.

-Wconditional-uninitialized flags it at util.bpf.c:181:21. Neither -Wall nor -Wuninitialized does (verified, clang 22).

Whether upstream layered's cgroup substring matching is actually misbehaving in production is NOT established here and needs its own investigation — the value is a pointer's low bits, so the comparison outcome depends on the map allocation address.

# Acceptance Criteria

Fix upstreamed to sched-ext/scx (initialise y before the outer loop), or a documented explanation of why the existing behaviour is intended.
