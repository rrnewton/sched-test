---
title: libscx_lavd.so has an RWE LOAD segment (two __ksym data definitions make .ksyms WAX), so an MDWE process cannot load it
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.598287095+00:00
updated_at: 2026-09-25T03:43:12.598287095+00:00
---

# Description

libscx_lavd.so has a writable and executable (RWE) LOAD segment. A process that forbids W+X mappings cannot load it: under prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN), which is the mechanism behind systemd's MemoryDenyWriteExecute=yes, dlopen fails with 'failed to map segment from shared object'. The other five bundled .so files have the usual RE / R / RW layout and load there.

Measured at integration 24d864c6 with the release-candidate scratch consumer (full link contract), run under a small wrapper that sets PR_SET_MDWE and then execs it:
- lavd: load error, 'libscx_lavd.so: failed to map segment from shared object'.
- simple: loads, exits Normal, fingerprint identical to the run without MDWE.

readelf -lW libscx_lavd.so shows the third LOAD segment as RWE, holding .data.rel.ro .dynamic .got .got.plt .data .ksyms .maps license .bss.

Cause: .ksyms. lavd's sources declare cpufreq_cpu_data and hw_pressure as __ksym externs, and schedulers/lavd/wrapper.c defines them (a struct cpufreq_policy pointer and an unsigned long). A definition inherits the section attribute of the visible declaration, so both land in .ksyms. The same section already holds code, because function definitions of other __ksym names land there too: bpf_cgroup_ancestor and bpf_cgroup_from_id from lavd's wrapper, and the weak bpf_iter_scx_dsq_* bodies present in all six. In the other five .so files .ksyms holds only functions and is AX, so it goes into the RE segment. In lavd the two data objects make it WAX, the linker places it with the writable data, and that whole segment becomes executable. As a result lavd's .data and .bss are executable in every process that loads it.

binutils 2.39 and later warn about an RWX LOAD segment by default. This host links with 2.35.2, so the build is silent here, but a consumer on a newer toolchain will see the warning.

Fix, either of:
- Define the two objects in a translation unit that does not see their __ksym declarations. They are data the sim provides, not kfuncs.
- For the sim build, redefine __ksym to nothing after bpf_helpers.h, so no definition inherits the attribute. That also takes the function bodies out of .ksyms, and the duplicate-kfunc cleanup (sim-mlv09) removes lavd's two wrapper functions anyway.

And add a build check that fails on any LOAD segment with both W and X, for example in tests/symbol_export.rs or in scxsim-build right after the link step, so the next such object cannot ship.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
