---
title: 'scxsim: model cosmos GPU-affinity subsystem (gpu_enabled + gpu_pid_map) to cover can_use_node/pick_cpu_on_gpu_node NUMA gating'
status: open
priority: 3
issue_type: task
created_at: 2026-07-23T03:32:32.189359476+00:00
updated_at: 2026-07-23T03:32:32.189359476+00:00
---

# Description

cosmos can_use_node() (per-node cpumask restriction, main.bpf.c ~474) is reachable ONLY from pick_cpu_on_gpu_node() (~506), which short-circuits at 'target_node = gpu_node_by_pid(p->pid); if (target_node < 0 ...) return'. With no GPU-registered task, gpu_node_by_pid() returns -1 and can_use_node() is never evaluated (verified: pick_cpu_on_gpu_node runs but can_use_node=0% even with numa_enabled + restricted allowed_cpus, test_numa_restricted_affinity). To cover can_use_node the sim needs to model the GPU-affinity path: gpu_enabled=true + a task registered in gpu_pid_map with a target node != its current node (mirroring scx_cosmos --gpu... options / NVML). Needs a wrapper knob to populate gpu_pid_map + register the map. Filed during tg write-cosmos-tests (COVERAGE_AUDIT sec 4.3 hint was incomplete: can_use_node is GPU-gated, not reachable via plain restricted affinity).
