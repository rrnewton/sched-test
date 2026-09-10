---
title: A task left on a layer DSQ while every CPU is idle is never dispatched (scx_layered)
status: open
priority: 2
issue_type: task
created_at: 2026-09-10T20:53:07.679991400+00:00
updated_at: 2026-09-10T20:53:07.679991400+00:00
---

# Description

Localized while investigating sim-6mheb. Currently MASKED: it only becomes reachable once p->tgid is set (sim-6mheb), because until then layered routes every task down its scheduler-daemon fast path and nothing lands on a layer DSQ.

REPRO (apply sim-6mheb's one-line p->tgid = pid first):
  4 CPUs, N run-once tasks of 20ms each, default single catch-all layer, duration 2000ms.
    N=3   -> 2/3 complete. The third sits runnable forever; still 2/3 at 3000ms.
    N=8   -> 8/8 complete.
    N=16  -> 16/16 complete.

So layer DSQs ARE consumed under load. What fails is the UNDERSUBSCRIBED case: probes show the two completing tasks got SCX_DSQ_LOCAL (select_cpu found an idle CPU) while the third was inserted on the layer DSQ, and once the other two finish there is no runnable work anywhere, every CPU idles, and nothing ever comes back to consume that DSQ.

Not a DSQ-id-zero problem: reproduced identically with the catch-all as layer 1 (DSQ 0x10000) instead of layer 0 (DSQ 0x0).

In the kernel a CPU with nothing to run calls balance_scx() -> ops.dispatch(), so an idling CPU would drain the DSQ. Suspect the engine either does not deliver ops.dispatch to a CPU that has already gone idle, or does not re-trigger dispatch when a user DSQ becomes non-empty with all CPUs idle. Whichever it is, this is engine/substrate (the kernel's job), not scheduler logic.

Worth checking whether other schedulers are exposed to the same shape and merely do not hit it because they dispatch to local DSQs more eagerly.

Cross-check available: layered's own antistall exists upstream to rescue exactly this, and with the production default antistall_sec=3 it did NOT rescue a 3000ms run — worth confirming whether that is correct timing or a second gap.
