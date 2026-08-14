---
title: scxsim lacks tp_btf/sched_switch substrate; lavd_sched_switch compiled but never invoked
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T13:55:27.843543653+00:00
updated_at: 2026-08-12T13:55:27.843543653+00:00
---

# Description

Upstream 14f12ba2 ('scx_lavd: replace deprecated ops.cpu_acquire/release with a sched_switch hook', in the 59c30ba -> c630d994 sync) gives LAVD two tiers for draining local-DSQ tasks stranded by higher-class preemption:

  kernel >= 6.19: ops.cpu_release is NULLED OUT by userspace Rust and a
                  SEC("?tp_btf/sched_switch") hook (lavd_sched_switch) calls
                  scx_bpf_reenqueue_local_from_anywhere() on fair -> RT/DL.
  kernel <  6.19: ops.cpu_release retained as the legacy drain.

Tier selection lives in USERSPACE Rust (scx_lavd/src/main.rs:725, ksym_exists("scx_bpf_reenqueue_local___v2")), not in BPF, so scxsim -- which compiles only the BPF side -- does not participate in it.

Current scxsim state after the sync (verified by nm on the built libscx_lavd.so):
  - lavd_cpu_release  ... PRESENT and reachable (scxsim models ops.cpu_release; safe/scenario.rs HigherPriorityClass drives release_at_ns/acquire_at_ns)
  - lavd_sched_switch ... PRESENT as a symbol but NEVER INVOKED -- scxsim has no tp_btf tracepoint substrate to deliver sched_switch
  - lavd_cpu_acquire  ... GONE (upstream deleted it; was present in the pre-sync build). This one is FINE and needs no work: cpu_acquire is an optional callback resolved with try_get!, so an absent symbol makes scxsim skip the call, exactly mirroring an unset struct_ops slot in the kernel.

Consequence: scxsim always exercises the LEGACY (< 6.19) tier. On a modern kernel, production LAVD runs the sched_switch tier instead, so scxsim and production take DIFFERENT code paths for the same stranded-task drain. Both paths call the same reenqueue primitive, so behavior should be equivalent today -- but this is a Principle 1 (match-production) divergence and is exactly the kind of gap that hides a bug when the two paths later diverge.

Per the No-Stub Rule this is a scxsim INFRASTRUCTURE task, not a license to stub: build a minimal tp_btf tracepoint substrate that can deliver sched_switch (prev, next, prev_state) so lavd_sched_switch actually executes, then gate which tier runs on a simulated kernel version.

Secondary gap found while investigating: no test drives the HigherPriorityClass release/acquire path for lavd at all (only panic_recovery.rs:411 checks acquire/release ORDERING validation). So the legacy tier scxsim does run is itself uncovered.

Filed during tg catchup-sync-upstream-scx.
