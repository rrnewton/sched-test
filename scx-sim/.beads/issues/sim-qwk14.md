---
title: 'scxsim lavd: bpf_ringbuf_reserve() is NULL, so the introspec ring buffer is permanently full'
status: open
priority: 3
issue_type: task
labels:
- no-stub
- lavd
depends_on:
  sim-30aa7: related
  sim-7d77c: related
created_at: 2026-09-25T05:18:55.269257412+00:00
updated_at: 2026-09-25T05:21:26.487168071+00:00
---

# Description

DEFECT

`schedulers/lavd/wrapper.c` defines `bpf_ringbuf_reserve` as `((void *)0)` and `bpf_ringbuf_submit` as empty.

PRODUCTION

lavd's `introspec.bpf.c` `submit_task_ctx` reserves a message in the `introspec_msg` ring buffer, fills it, and submits it to the userspace monitor.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

Every reserve fails. `submit_task_ctx` returns -ENOMEM and the sample is dropped. The code that fills the message is never reached.

CONSEQUENCE

No scheduling state depends on this, so it is EQUIVALENT-TODAY. But the simulator behaves like a ring that is always full, which production reaches only under overload. The message-filling code never runs, so a fault in it cannot be found. This breaks the No-Stub rule, because it is a no-op shim.

FIX DIRECTION

Model the BPF ring buffer: a bounded buffer that `bpf_ringbuf_reserve`, `bpf_ringbuf_submit` and `bpf_ringbuf_discard` operate on, which the harness can drain or leave full. Then delete the wrapper overrides.

ACCEPTANCE

- A lavd test with introspection enabled drains `introspec_msg` and checks a message's fields against the task.
- A second test fills the ring and sees -ENOMEM.
- Until this is fixed, the overrides carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row K06 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
