---
title: sim_atq.c hand-ports upstream lib/atq with no differential test; decide its No-Stub status
status: open
priority: 2
issue_type: task
labels:
- cratesio
- no-stub
created_at: 2026-09-25T03:43:12.590034287+00:00
updated_at: 2026-09-25T03:43:12.590034287+00:00
---

# Description

scx_simulator does not compile upstream lib/atq.bpf.c. It links csrc/sim_atq.c instead: a C port of the whole scx_atq_* API over a glibc-malloc array sorted by vtime. Upstream is an arena rbtree (rb_create(RB_NOALLOC, RB_DUPLICATE)) behind scx_alloc. lavd's compiled-in lib/cgroup_bw.bpf.c calls this API on every throttled-task path.

The port is careful. It models SCX_ATQ_DEAD, the -EALREADY and -ECANCELED claim results, FIFO sequence keys and holdcnt. The No-Stub Rule still names 'BPF helper / library code that the scheduler calls into' and forbids interface-only reimplementations that approximate the BPF semantics. Whether a hand port of library code counts as substrate or as a reimplementation has not been decided in writing. Meanwhile it is kept in sync by hand: the pin bump to 413031d4 changed lib/atq.bpf.c, lib/rbtree.bpf.c and lib/sdt_task.bpf.c, and sim_atq.c was not touched. That was harmless for the port's logic but left its offset defaults stale (sim-6vhbq). No test compares the port with upstream, for example on ordering among equal vtimes or on FIFO behaviour after errors.

Decide one of:
(a) compile the real lib/atq.bpf.c and lib/rbtree.bpf.c into the host over the sim arena (sim-d9988 tracks full arena support), or
(b) record an explicit exemption in scx-sim/CLAUDE.md and add a differential test that drives upstream atq.bpf.c (compiled as host C against the sim arena) and sim_atq.c with the same operation sequences and requires identical pop order and return codes.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
