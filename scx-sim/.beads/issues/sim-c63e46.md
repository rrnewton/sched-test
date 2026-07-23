---
title: 'scxsim: model cosmos GPU-affinity subsystem (gpu_enabled + gpu_pid_map) to cover can_use_node/pick_cpu_on_gpu_node NUMA gating'
status: open
priority: 3
issue_type: task
created_at: 2026-07-23T03:32:32.189359476+00:00
updated_at: 2026-07-23T19:00:46.053952731+00:00
---

# Description

cosmos can_use_node() (per-node cpumask restriction, main.bpf.c ~474) is reachable ONLY from pick_cpu_on_gpu_node() (~506), which short-circuits at 'target_node = gpu_node_by_pid(p->pid); if (target_node < 0 ...) return'. With no GPU-registered task, gpu_node_by_pid() returns -1 and can_use_node() is never evaluated (verified: pick_cpu_on_gpu_node runs but can_use_node=0% even with numa_enabled + restricted allowed_cpus, test_numa_restricted_affinity). To cover can_use_node the sim needs to model the GPU-affinity path: gpu_enabled=true + a task registered in gpu_pid_map with a target node != its current node (mirroring scx_cosmos --gpu... options / NVML). Needs a wrapper knob to populate gpu_pid_map + register the map. Filed during tg write-cosmos-tests (COVERAGE_AUDIT sec 4.3 hint was incomplete: can_use_node is GPU-gated, not reachable via plain restricted affinity).

# Notes

Partially resolved on integration: added cosmos_add_gpu_task(pid,node) wrapper helper (lazily registers gpu_pid_map) + Rust binding, so a NUMA GPU-mapped task now reaches can_use_node() via cosmos_select_cpu's GPU branch. can_use_node coverage 0->78.57% regions (function now executed) via new test_gpu_node_affinity (cosmos.rs). REMAINING: the GPU-dispatch tail (pick_cpu_on_gpu_node line ~517 -> __COMPAT_scx_bpf_pick_idle_cpu_node) and can_use_node's return-true path are still uncovered because the sim does not model scx_bpf_pick_idle_cpu_node (NULL weak ksym -> would crash); the test deliberately keeps can_use_node returning false (restricted, non-intersecting affinity). Follow-up: implement scx_bpf_pick_idle_cpu_node kfunc to cover the GPU dispatch. NOTE: registration is lazy to avoid perturbing exact two-run determinism (test_cosmos_domain_determinism).
