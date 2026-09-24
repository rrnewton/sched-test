---
title: 'scxsim lavd: lib topology is a single-LLC stub (topo_cpu_to_llc_id() == 0) that lavd''s own idle-CPU selection now consults'
status: open
priority: 1
issue_type: bug
labels:
- no-stub
- lavd
- topology
created_at: 2026-09-24T20:02:39.859610108+00:00
updated_at: 2026-09-24T20:02:39.859610108+00:00
---

# Description

The lavd wrapper (schedulers/lavd/wrapper.c, the 'Topology stubs: single-LLC simulator' block) defines upstream lib/topology's topo_cpu_to_llc_id() as `return 0` and nr_topo_nodes[] as all-ones, instead of compiling in scx/lib/topology.bpf.c. It was added in Phase 2 (67b2445c) for cgroup_bw's per-LLC backlog walks, when the cpu-bw-stall reproducer was single-LLC.

Since scx 7b0b432b9 ("scx_lavd: extend overflow set on bursty wake-ups", in the 81738161 pin bump) lavd's OWN scheduling logic calls it: idle.bpf.c find_cpu_for_ovrflw_extend() anchors the overflow extension to prev_cpu's LLC and skips every candidate whose topo_cpu_to_llc_id() differs. Under the stub every CPU is in LLC 0, so after lavd_setup_multi_domain() (cpdom llc_id = d per domain) the scheduler may extend its overflow set with a CPU in a different LLC, which production lavd never does. Two topology views inside one scheduler now disagree (cpdom_ctxs vs lib topology), and neither is derived from the sim's own Topology model (safe/topology.rs llc_id).

Production populates the lib topology from userspace: scx_arena::ArenaLib runs the arena_init / arena_topology_init / arena_alloc_mask / arena_topology_node_init syscall programs (scx/lib/arena.bpf.c), which call topo_init() for every machine/node/LLC/core/CPU node of the host topology.

Fix (substrate, per the No-Stub rule): compile scx/lib/topology.bpf.c (and the bitmap/arena pieces it needs) into the lavd .so, and have the sim play ArenaLib's role, replaying that syscall sequence from the scenario's Topology before lavd_init. Then derive lavd's cpdom_ctxs from the same Topology so the two views cannot disagree. Delete the stub and its DANGER TODO.

Found by the implicit-declaration sweep during the scx pin bump (task manual-scx-pin-bump-to-latest): topo_cpu_to_llc_id is declared by lib/topology.h only under __BPF__, so idle.bpf.c's new call compiled as an implicit declaration.
