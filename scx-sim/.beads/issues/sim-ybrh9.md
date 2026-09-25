---
title: Gate the standalone scheduler .so build behind the standalone feature
status: open
priority: 2
issue_type: task
created_at: 2026-09-24T18:44:44.890187065+00:00
updated_at: 2026-09-24T18:44:44.890187065+00:00
---

# Description

Embedders that build scx_simulator with default-features = false still pay for build_schedulers compiling all six bundled scheduler .so files (simple, tickless, cosmos, mitosis, lavd, layered), because build.rs runs it unconditionally. It cannot simply be gated on CARGO_FEATURE_STANDALONE today, because non-standalone code still reads the compile-time SCHEDULER_SO_DIR: DynamicScheduler::layered / layered_with_topology / layered_for_topology (unsafe_impl/ffi.rs) are not cfg(feature = "standalone"), and the scxsim bin's scheduler lookup and list_schedulers use env!("SCHEDULER_SO_DIR") under every feature set. Gating the build alone would turn those into runtime missing-.so failures (No Silent Failures). Fix order: (1) cfg-gate the layered convenience constructors like simple/lavd/etc (layered_for_topology callers that need an embedder path get a load_with_definition-based equivalent); (2) make the scxsim bin require the standalone feature (required-features) or resolve .so dirs another way; (3) only then gate build_schedulers + the SCHEDULER_SO_DIR rustc-env on CARGO_FEATURE_STANDALONE. Also fix validate.sh's stale comment on the --no-default-features build, which claims the crate only uses load_with_definition in that configuration. Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
