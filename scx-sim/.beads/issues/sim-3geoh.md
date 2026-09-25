---
title: Scheduler .so files carry their own bodies for 24 of the 55 host-provided names, so a missing host export silently binds a no-op
status: open
priority: 1
issue_type: bug
labels:
- cratesio
- no-stub
created_at: 2026-09-25T03:43:12.577520685+00:00
updated_at: 2026-09-25T03:43:12.577520685+00:00
---

# Description

Every bundled scheduler .so carries its own body for 24 of the 55 names the host is supposed to provide: the HostExport::own_definition class in scxsim-build's HOST_EXPORTS (scx_bpf_create_dsq, scx_bpf_dsq_nr_queued, scx_bpf_error_bstr, scx_bpf_kick_cpu, scx_bpf_put_cpumask, scx_bpf_task_cpu, bpf_task_from_pid, bpf_task_release, bpf_rcu_read_lock, bpf_rcu_read_unlock, scx_task_alloc, scx_task_data, scx_task_free, scx_atq_create_internal, scx_atq_insert, scx_atq_insert_vtime, scx_atq_nr_queued, scx_atq_peek, scx_atq_pop, bpf_cgroup_acquire, bpf_cgroup_ancestor, bpf_cgroup_from_id, bpf_cgroup_release, bpf_cpumask_test_cpu). Most are __weak bodies in scxtest/overrides.c that return 0 or NULL or do nothing; the cgroup four are weak bodies in csrc/sim_bpf_stubs.c; bpf_cpumask_test_cpu is a strong duplicate there.

The host's real implementation runs only if the host binary exports the name. If it does not, dlopen succeeds and the .so's own no-op runs, with no error anywhere. That is the No-Stub failure, and it is silent. Measured at integration 24d864c6 with the release-candidate scratch consumer linked through a selective export list: leaving out scx_task_data makes lavd schedule zero tasks while exiting Normal at 1 s and at 5 s; at 40 s it reports ErrorStall, blamed on the scheduler. The weak scx_bpf_error_bstr body is worse in kind: if it binds, every scx_bpf_error() the scheduler raises is discarded and the run continues.

The load-time probe (unexported_symbols in unsafe_impl/ffi.rs, LoadError::HostSymbolsNotExported) now refuses such a host before dlopen, but only for the names in HOST_EXPORTS, which are derived from the bundled six. sim-4b77c tracks the gap for an embedder's own .so. These bodies are what turn that gap into silent stub-binding instead of a load failure.

Fix: delete the .so-side bodies for names the host always provides, so each reference is a strong undefined symbol and a missing export fails RTLD_NOW with 'undefined symbol: <name>' (the dlopen_fails class, which is loud today). Keep the probe: it names the fix (scxsim_build::emit_host_link_args()) where the dlopen error does not. Then move the names to HostExport::dlopen_fails and let tests/symbol_export.rs confirm the reclassification.

Related bodies to decide in the same pass:
- Five of the 24 have no call sites in any bundled .so (0 relocations): scx_atq_insert, scx_atq_insert_vtime, scx_atq_peek, bpf_rcu_read_lock, bpf_rcu_read_unlock.
- csrc/sim_bpf_stubs.c defines scx_bpf_cpu_rq and scx_bpf_locked_rq as strong functions returning NULL. They are reached only when a scheduler's scx_bpf_cpu_curr path is unresolved, but they are kfunc no-ops by construction.
- scxtest/overrides.c also defines '__weak unsigned long CONFIG_NR_CPUS = 1024'.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
