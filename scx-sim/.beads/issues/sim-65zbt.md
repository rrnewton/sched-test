---
title: Rodata is written at the definition's declared width; nothing checks it against the symbol's size
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T00:19:31.014181941+00:00
updated_at: 2026-09-25T00:19:31.014181941+00:00
---

# Description

DynamicScheduler::try_apply_rodata (called by try_load_with_definition) resolves each rodata global with dlsym, which yields an address and no size, and writes the ConfigValue's width via write_{bool,u8,u32,u64}_global (NumCpus writes u32). Nothing compares that width with the ELF symbol's st_size.

A declared width WIDER than the global writes past it into whatever the linker placed next. A NARROWER one writes the low bytes only and leaves the rest. Neither is reported.

Measured on libscx_lavd.so (debug build, branch feat/scxsim-cratesio-release-candidate):
- verbose (size 1) is followed directly by per_cpu_dsq (size 1) at +1. So ("verbose", ConfigValue::U32(_)) also overwrites per_cpu_dsq.
- no_use_em (size 1) is followed by padding. The same mistake there is harmless in today's layout.

Whether a given mistake corrupts anything depends on link layout and on the neighbour's value when the write happens. So this is latent, not a demonstrated miscompare.

Why it matters for publishing: try_load_with_definition is a safe fn, and SchedulerDefinition is data the embedder writes. ktstr's declare_scheduler! carries no build info, so ktstr would write these definitions by hand.

Fix: check each write against st_size, and on a mismatch return a new LoadError variant naming the global, its declared width and its actual size. LoadError is #[non_exhaustive], so the new variant is semver-compatible after 1.0.0. Two ways to get st_size:
- dladdr1(addr, RTLD_DL_SYMENT). glibc only. The libc crate binds dladdr1 but not the RTLD_DL_SYMENT constant (value 1).
- Read .dynsym from the .so at load time with the object crate, today a dev-dependency (tests/symbol_export.rs).
