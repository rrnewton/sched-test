---
title: Some kfuncs have two definitions per process, and ELF binding or inlining picks the one that runs (lavd's cgroup kfuncs, bpf_cpumask_test_cpu)
status: open
priority: 2
issue_type: bug
labels:
- cratesio
- no-stub
created_at: 2026-09-25T03:43:12.587451237+00:00
updated_at: 2026-09-25T03:43:12.601361596+00:00
---

# Description

Some kfuncs have more than one definition in a process. Which body runs is decided by ELF symbol binding, or by clang inlining at compile time, and not by any code that says which one is meant.

1. bpf_cgroup_from_id and bpf_cgroup_ancestor for lavd. schedulers/lavd/wrapper.c defines both as strong functions in the same translation unit as lavd's main.bpf.c and lib/cgroup_bw.bpf.c. clang inlines them into their callers: accounting_timerfn, replenish_timerfn, scx_cgroup_bw_dump and others call sim_get_root_cgroup / sim_cgroup_lookup_by_id directly, and libscx_lavd.so has no relocation against either name. So the host's exported versions in unsafe_impl/kfuncs.rs never run for lavd, whatever the link flags. LD_DEBUG=bindings on the release-candidate consumer (integration 24d864c6, full link contract) shows 38 HOST_EXPORTS names bound from libscx_lavd.so to the executable, and neither cgroup name is among them.
   - The from_id pair agrees: both delegate to sim_cgroup_lookup_by_id.
   - The ancestor pair does not. lavd's copy models a flat hierarchy (the root at level 0, NULL at every other level). The host's calls sim_cgroup_lookup_ancestor for any level.
   - At pin 413031d4 the divergence is latent. lib/cgroup_bw.bpf.c's parent walk now reads cgrp->ancestors[] through cbw_cgroup_ancestor (BPF_CORE_READ, and the sim populates that array; see SIM_CGROUP_ALLOC_SIZE in csrc/sim_task.c). Its one remaining bpf_cgroup_ancestor call asks for level 0, where the two copies agree. Any future caller that asks for a deeper level under lavd gets NULL although the host can answer.
   - sim-4e4c0a (nested cgroup CPU-bw throttle not enforced) blames 'the bpf_cgroup_ancestor NULL stub (csrc/sim_bpf_stubs.c:323)' for parent_id=0. That path no longer exists upstream and the line reference is stale. Its symptom needs re-measuring at the new pin.
   - The wrapper bodies inherit the __ksym section attribute and land in .ksyms. On their own they would leave it AX, as in the other five .so files. The same wrapper also defines two __ksym data objects, cpufreq_cpu_data and hw_pressure, and those make lavd's .ksyms WAX, which gives libscx_lavd.so an RWE LOAD segment (sim-c7eca).

2. bpf_cpumask_test_cpu: csrc/sim_bpf_stubs.c compiles a strong definition into every scheduler .so, and the host has its own in scxtest/scx_test_cpumask.c. Under the full contract the .so's PLT reference binds to the host's copy (measured in the same LD_DEBUG run). The .so copy runs only when the host does not export the name.

3. csrc/sim_bpf_stubs.c also compiles a weak NULL-returning bpf_cgroup_from_id and bpf_cgroup_ancestor into every .so. They run for any non-lavd scheduler whose host does not export the names.

Fix: keep one implementation per kfunc, the host's. Delete lavd's wrapper copies, so lavd reaches kfuncs.rs through HOST_EXPORTS, and confirm lavd's fingerprints are unchanged (they should be, given the level-0-only use). Delete the sim_bpf_stubs.c duplicates together with the own_definition fallback removal.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
