---
title: 'scxsim-build: extra_local_include looks vestigial; verify the .so bytes are unchanged without it, then decide before publishing 1.0'
status: open
priority: 3
issue_type: task
labels:
- embed
- cratesio
created_at: 2026-09-24T23:53:10.629385610+00:00
updated_at: 2026-09-24T23:53:10.629385610+00:00
---

# Description

SchedulerManifest/SchedulerDefinition::extra_local_include (builder: with_extra_local_include) adds -I <schedulers>/<name>, the scheduler's own wrapper directory. lavd and cosmos set it. Today those directories hold only config.mk and wrapper.c. A quoted include already searches the includer's directory. A patched source (cosmos_main_patched.c) is generated into OUT_DIR and found through the -I<OUT_DIR> that build_schedulers prepends. So the flag appears to resolve nothing. It dates from when the patched source was written next to the wrapper.

It is part of the public scxsim-build API in the crates.io RC. Removing it after publishing is a semver break.

To verify: build lavd and cosmos with extra_local_include=false and compare the libscx_lavd.so / libscx_cosmos.so sha256 against the current build. If they are byte-identical, remove the field and builder (or deprecate them) before the first publish. If they differ, find which include resolves through that directory and document it.
