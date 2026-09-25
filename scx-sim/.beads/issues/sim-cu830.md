---
title: 'rustdoc: scx_simulator''s public docs have 20 errors (12 links to private items, 7 unresolved, 1 bad HTML tag) and no gate runs cargo doc'
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.592620360+00:00
updated_at: 2026-09-25T03:43:12.592620360+00:00
---

# Description

scx_simulator's public docs have 12 links to private items, 7 unresolved links and one unclosed HTML tag. Nothing in the gate runs rustdoc, so nothing catches them.

Measured on the crates.io release-candidate branch (integration 24d864c6), from scx-sim/:

    RUSTDOCFLAGS="-D warnings" cargo +1.97.1 doc --no-deps -p scx_simulator -p scxsim-build -p scx_perf -p scx_layered_alloc -p scx_layered_growth -p scxsim-workload-ir

The other five publish crates document cleanly (the release-candidate branch fixed theirs). scx_simulator fails with 20 errors:

Links from public docs to private items (rustdoc::private_intra_doc_links), 12. docs.rs documents public items only, so each of these points at a page that is never generated:
- CgroupInfo, CgroupRegistry -> CgroupAlloc
- destroy_by_name, free_raw -> free_cgroup_raw
- xnuma_threshold, ForkPlacement -> crate::layered_xnuma
- the e9patch module -> E9PatchBackend, E9PatchReplayBackend, crate::preempt::E9_SHARED_ADDR
- E9PatchFns, E9_RIP_SHARED_ADDR -> crate::preempt::E9_SHARED_ADDR
- E9RipShared -> crate::preempt::E9SharedRbc

Unresolved links (rustdoc::broken_intra_doc_links), 7. These render as literal bracketed text:
- crate::layered_config (safe/layered.rs)
- LayerConfigOptions::from_probes (layered_config.rs)
- dst (layered_xnuma.rs)
- ScenarioBuilder and LayerMatch (unsafe_impl/ffi.rs)
- LayeredProbes::lstat_id and LayeredProbes::gstat_id (layered_probes.rs)

Invalid HTML (rustdoc::invalid_html_tags), 1. The prepare_css_iter doc in safe/cgroup.rs writes Arc<Mutex> outside backticks. A browser parses <Mutex> as an unknown element, so the word disappears from the rendered page.

Neither validate.sh nor any workflow under .github/workflows runs cargo doc.

Fix: fix the 20, then add the command above to validate.sh (and so to CI). Without the gate, the next docs.rs build is the first place a regression shows up.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
