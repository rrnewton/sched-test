---
title: 'scx-sim: the publish set''s rust-version = 1.88 is measured once and not gated'
status: open
priority: 2
issue_type: task
labels:
- release
- cratesio
created_at: 2026-09-24T23:10:48.400663984+00:00
updated_at: 2026-09-24T23:10:48.400663984+00:00
---

# Description

The six crates.io publish-set crates (scxsim-build, scx_perf, scx_layered_growth, scx_layered_alloc, scx_simulator, scxsim-workload-ir) declare rust-version = "1.88". That floor was MEASURED on 2026-09-24, not guessed: a scratch consumer depending on the six packaged .crate files, with a fresh MSRV-aware lockfile (resolver 3), fails on 1.87.0 (let chains in the upstream scx_layered alloc.rs and layer_core_growth.rs that scx_layered_alloc / scx_layered_growth compile verbatim; stable from 1.88) and on 1.85.0 (the same plus u*::is_multiple_of, stable from 1.87), and on 1.88.0 builds in 56 s and reproduces the 1.97.1 run fingerprints for simple and lavd.

Nothing re-checks it. validate.sh runs on the pinned 1.97.1 only, so the declared floor goes stale silently the first time upstream scx_layered (pulled in by the nightly scx pin bump, which lands itself when green) or our own code uses a newer std API. The failure a user sees is loud (a compile error on their toolchain, not a wrong run), but it breaks a published promise, and cargo's MSRV-aware resolver cannot help: it picks dependency versions, not our source.

Fix: a CI stage that builds the publish set on the declared rust-version (cargo +1.88 check -p <each crate> with --locked against an MSRV-resolved lockfile, or cargo-msrv verify), wired so a pin bump that raises the floor goes red and forces an explicit rust-version bump.

# Acceptance Criteria

A pin bump or source change that needs a newer toolchain than rust-version fails a gate that runs before landing; the gate runs at the same feature sets the published crates are consumed at.
