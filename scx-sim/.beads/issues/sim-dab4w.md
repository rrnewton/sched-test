---
title: scx_simulator's build script emits 6 C warnings; two come from headers that disagree about a macro, so include order picks the value
status: open
priority: 3
issue_type: task
labels:
- cratesio
created_at: 2026-09-25T03:43:12.607053509+00:00
updated_at: 2026-09-25T03:43:12.607053509+00:00
---

# Description

Building scx_simulator gives 6 compiler warnings from its build script: 3 distinct warnings, each in csrc/sim_task.c and csrc/sim_cgroup.c. Cargo shows them as 52 'warning: scx_simulator@1.0.0:' lines. Two of the three are headers that disagree about a macro, so the value in effect depends on include order.

- __builtin_preserve_field_info: scxtest/overrides.h defines it as 1 (fields exist), and csrc/sim_wrapper.h redefines it as 0 (fields absent, with a comment explaining why 0 is intended). In these two TUs sim_wrapper.h comes last, so the value is 0. Any TU that includes them in the other order gets 1, and bpf_core_field_exists() then answers the opposite way.
- __kconfig: sim_wrapper.h #undefs it and defines it empty. Its comment says weak __kconfig declarations in the .kconfig section resolve to address 0 in a -nostdlib .so and SIGSEGV. sim_wrapper.h then includes scx/common.bpf.h, and libbpf's bpf_helpers.h redefines __kconfig as __attribute__((section(".kconfig"))). So after that include the override is gone for the rest of the TU.
- -Wsign-compare in upstream scx/cid.bpf.h (bpf_arena_for(cpu, 0, nr_cpu_ids), s32 vs u32). This is vendored upstream code.

No behavioural effect today, as far as measured: neither sim_task.c nor sim_cgroup.c uses bpf_core_field_exists, bpf_core_type_exists, __kconfig or a CONFIG_ symbol, and neither object, nor any of the six bundled .so files, has a .kconfig section. Scheduler TUs built by scxsim-build were not checked for the same redefinitions, because its compiler output is not shown on success.

After publishing, crates.io consumers will not see these: cargo shows build-script warnings only for path dependencies, unless the build fails.

Fix: decide the one intended value of each macro and state it in one header, with #undef before the #define so the order cannot flip it. Re-assert __kconfig after the common.bpf.h include, or make sure nothing after it relies on the empty form. Silence -Wsign-compare for vendored headers, or fix it upstream.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
