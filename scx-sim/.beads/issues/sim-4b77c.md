---
title: 'Host-export probe covers only the bundled schedulers'' symbols: an embedder-built .so can bind its own fallbacks silently'
status: open
priority: 1
issue_type: bug
labels:
- cratesio
- no-stub
created_at: 2026-09-25T01:20:12.001902626+00:00
updated_at: 2026-09-25T01:20:12.001902626+00:00
---

# Description

HOST_EXPORTS is the list of symbols the six bundled scheduler .so files resolve from their host; tests/symbol_export.rs derives it from those six and fails on drift. The load-time probe (unexported_symbols in ffi.rs, called first in DynamicScheduler::try_load_with_definition) checks exactly that list, before dlopen, and never reads the .so being loaded.

An embedder that compiles its own wrapper.c through scxsim_build::build_schedulers links the same scxtest/overrides.c and csrc/sim_bpf_stubs.c weak fallbacks, and its scheduler may reference host symbols the bundled six never do (a kfunc behind __ksym __weak, say). If the embedder's binary does not export such a name, the .so binds its own fallback or NULL and runs, silently: the No-Stub failure the probe exists to stop, and the probe passes because the name is not on its list. A strong reference outside the list still fails dlopen, loudly.

tests/symbol_export.rs states the gap in its header ('Not covered: ... a scheduler .so an embedder builds itself'). No test builds an out-of-tree wrapper.c at all; embed_harness and ktstr-scenario-replay build bundled ones through SimBuildInputs::build_bundled.

Fix direction: derive the check from the .so being loaded. Either read its dynamic symbols and relocations (what symbol_export.rs already does with the object crate) at build time in build_schedulers, refusing a .so whose silently-binding names are not all in HOST_EXPORTS, or do the same at load time before dlopen. Until then ai_docs/ktstr_scxsim_embed_contract.md lists it under the guards' gaps.
