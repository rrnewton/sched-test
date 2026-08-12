---
title: Run the real BPF verifier as a scxsim CI gate (veristat / bpftool prog load)
status: open
priority: 1
issue_type: task
created_at: 2026-08-12T20:09:43.526691958+00:00
updated_at: 2026-08-12T20:09:43.526691958+00:00
---

# Description

scxsim never runs the BPF verifier. It matches RUNTIME semantics (see ai_docs/BPF_UB_FIDELITY_POLICY.md) but has no equivalent of the verifier's LOAD-TIME rejections: uninitialised register reads (R%d !read_ok, verifier.c:3338), out-of-bounds map/packet access (verifier.c:5222-5285), unbounded pointer arithmetic (verifier.c:12916), constant division by zero (verifier.c:14505), constant over-width shifts (verifier.c:14511). A program scxsim runs happily may be unloadable in production, and that is the dangerous direction.

Approximating the verifier in C flags is hopeless — its bounds reasoning is whole-program abstract interpretation over tracked register ranges, and ASan checks a strictly different property. The fix is to run the REAL verifier: compile each supported scheduler for -target bpf and either load it or run veristat, as a separate CI gate.

Proven feasible: scx_layered's util.bpf.c compiles clean for -target bpf -mcpu=v3 using the scx include set plus the libbpf-sys headers, and 'sudo bpftool prog load' gives full verifier diagnostics. veristat is not currently referenced anywhere in the scx submodule.

Blocker: the scxsim build system produces host .so files, not per-scheduler BPF objects, so this needs a new build path.

# Acceptance Criteria

A CI step compiles each supported scheduler for the BPF target and fails the build on verifier rejection, with the verifier log surfaced.
