---
title: 'scxsim: glibc takes the .so''s own calloc/free/memcpy/memset/strncmp bindings, bypassing sim_deterministic_mem.c'
status: open
priority: 1
issue_type: bug
depends_on:
  sim-70abc8: related
created_at: 2026-09-24T20:51:54.599944171+00:00
updated_at: 2026-09-24T20:51:54.599944171+00:00
---

# Description

csrc/sim_deterministic_mem.c is compiled into every scheduler .so to replace glibc's
memory functions with fixed-branch versions (and calloc/free with the sim_arena bump
allocator), so glibc's alignment- and heap-state-dependent branches stay out of the PMU
RBC count (motivated by sim-70abc8). Its header comment says the linker "uses these
instead of resolving from the main binary's glibc". For the calls that matter, that is
false.

Measured (feat/scxsim-cratesio-release-candidate, based on integration 13543138):

- Every bundled .so (cosmos, lavd, mitosis, layered, simple, tickless) reaches calloc,
  free, memcpy and memset through R_X86_64_JUMP_SLOT relocations; lavd and layered also
  strncmp (`readelf -rW libscx_<name>.so`).
- The definitions have default visibility, and a dlopen'd .so resolves relocations in
  the global scope first -- the host and the libraries it started with, glibc included.
- `LD_DEBUG=bindings` on the symbol_export test binary: libscx_simple.so binds calloc,
  free, memcpy and memset to /lib64/libc.so.6.

So for every PLT call the scheduler runs glibc's implementations, not the deterministic
ones. malloc/realloc/memmove/memcmp/strcmp/strlen are defined too but not relocated, so
only direct calls (if any) reach them.

Impact: the determinism mechanism is silently not in effect for those calls. glibc's
memcpy/memset branch on size and alignment, and calloc/free on heap state; with ASLR
the addresses differ between runs. That is a plausible contributor to sim-70abc8's
small (1-8 branch) RBC deltas, NOT a demonstrated one: measure before claiming it.
Not a No-Stub issue (no scheduler logic is replaced).

Same mechanism as the host-export silent bindings (scxsim_build::HOST_EXPORTS,
LoadError::HostSymbolsNotExported), with glibc instead of the host binary as the
interposer.

Fix sketch: give the sim_deterministic_mem.c definitions hidden visibility, so the
static link binds every reference in the .so (including compiler-generated memcpy/memset
from other TUs) to them directly and the .so exports none of them. Before landing:
calloc then draws from the 32 MiB arena instead of glibc's heap, so check arena
headroom (see sim-ytru8) and that nothing frees a glibc pointer through the arena or
vice versa. Re-measure with LD_DEBUG=bindings.

Acceptance: `known_gap_libc_takes_the_so_memory_functions` in
crates/scx_simulator/tests/symbol_export.rs goes red; invert it per the known-gap
convention (assert no .so definition binds to another object). LD_DEBUG=bindings shows
no libscx_*.so binding of these names to libc.
