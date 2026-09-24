---
title: schedulers/Makefile cannot build lavd (ravg.bpf.c not found) and has drifted from scxsim-build
status: open
priority: 2
issue_type: bug
labels:
- build
- makefile
created_at: 2026-09-24T20:13:19.899015219+00:00
updated_at: 2026-09-24T20:13:19.899015219+00:00
---

# Description

The standalone scheduler Makefile (schedulers/Makefile, used by make -C schedulers, the e9 and UB-probe targets) no longer builds libscx_lavd.so: 'ravg.bpf.c' file not found. lavd's wrapper includes "ravg.bpf.c" resolved via -I<scx_root>/lib, which scxsim-build adds to every TU (base_includes = crate include set + <scx_root>/lib) but the Makefile never did. Broken since 8e422cd9 (2026-06-26), so it predates the 81738161 scx bump; the other five schedulers still build through it.

The two build paths have drifted in other ways too:
- the Makefile does not compile sim_dsq_iter_glue.c, one of scxsim-build's full TUs, so even with the include fixed its .so files would lack that glue;
- -Wconditional-uninitialized is Makefile-only.

Found during the scx pin bump (task manual-scx-pin-bump-to-latest), when -Werror=implicit-function-declaration was added to both paths and the Makefile was exercised to confirm it.

Fix: derive the Makefile's include set, TU list and CFLAGS from one source shared with crates/scxsim-build/src/lib.rs (or have the Makefile call the cargo build), so the two cannot drift again; add a validate.sh stage that builds every scheduler through the Makefile path so the next break is not silent for three months.
