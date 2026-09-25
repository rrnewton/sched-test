---
title: scxtest/overrides.c gives every .so weak scx_minheap_* bodies that report success, hiding an elided lib/minheap.bpf.c
status: open
priority: 1
issue_type: bug
labels:
- cratesio
- no-stub
created_at: 2026-09-25T03:43:12.581093639+00:00
updated_at: 2026-09-25T03:43:12.581093639+00:00
---

# Description

scxtest/overrides.c gives every scheduler .so __weak bodies for scx_minheap_pop (returns 0, which is success, without writing *helem), scx_minheap_insert (returns 0 and drops the element) and scx_minheap_alloc (returns NULL). The host provides no scx_minheap_* at all, and the names are not in HOST_EXPORTS, so the load-time probe never checks them. All six bundled .so files export the three names but contain no relocation against them and no call to them.

Upstream uses the API: lib/minheap.bpf.c defines scx_minheap_insert, scx_minheap_pop and scx_minheap_alloc_internal; lib/dhq.bpf.c and scx_p2dq (main.bpf.c, types.h) call it. An embedder that builds such a scheduler through scxsim_build::build_schedulers and forgets to compile lib/minheap.bpf.c into its wrapper links cleanly against these bodies. The scheduler then runs with every heap empty: pop reports success and hands back uninitialized memory. That is the No-Stub 'elided library' failure, silent. Without the weak bodies the same mistake is an 'undefined symbol: scx_minheap_pop' dlopen failure.

The bodies have also drifted from upstream: the weak scx_minheap_alloc(u32) matches no upstream symbol at pin 413031d4, because upstream allocates through scx_minheap_alloc_internal(size_t).

Fix: delete the three bodies. A scheduler that needs a heap compiles the real lib/minheap.bpf.c, the same way lavd compiles lib/cgroup_bw.bpf.c.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
