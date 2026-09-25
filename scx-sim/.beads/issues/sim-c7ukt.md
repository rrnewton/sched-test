---
title: 'Host-export probe checks presence, not identity: a same-named export elsewhere in an embedder''s process takes the binding (fat-host interposition)'
status: open
priority: 2
issue_type: bug
labels:
- cratesio
- no-stub
created_at: 2026-09-25T01:20:22.030338356+00:00
updated_at: 2026-09-25T01:20:22.030338356+00:00
---

# Description

The probe (unexported_symbols in ffi.rs) asks only whether dlsym(RTLD_DEFAULT, name) is non-NULL. It does not check that the definition it found is scx_simulator's.

A .so loaded RTLD_NOW|RTLD_LOCAL binds each dynamic relocation to the first definition in the global scope (the executable, then the libraries it started with and any RTLD_GLOBAL loads) before its own. emit_host_link_args() passes -rdynamic, which exports every global the binary links: 193,080 dynamic symbols in the debug scratch consumer of the crates.io RC, against 126 with a selective list. So any name a scheduler .so defines interposably, or references, that the embedder's binary also exports for its own reasons binds to that copy. For a HOST_EXPORTS name the probe then passes on the wrong definition; for any other name nothing checks.

In-tree instance today: glibc takes the bindings of the .so's own calloc/free/memcpy/memset (all six) and strncmp (lavd, layered), bypassing sim_deterministic_mem.c (sim-o3kct, known_gap_libc_takes_the_so_memory_functions). tests/symbol_export.rs checks the sim's own test binary only and lists this as not covered ('a same-named export an embedder's binary carries for its own reasons').

NOT measured for a real embedder. ktstr links far more than the scratch consumer did, so the candidate set is larger.

Fix direction: in the probe, dladdr each definition found and require it to come from the object that carries scx_simulator's static libs (the executable). For names outside HOST_EXPORTS that a .so defines interposably, compare dlsym(handle) with dlsym(RTLD_DEFAULT) after dlopen and refuse an unintended host binding. Alternatively build the .so so that its own copies bind locally (-Bsymbolic, or hidden visibility for everything that is not a host export); that also changes sim-o3kct's behaviour, so the two need deciding together.
