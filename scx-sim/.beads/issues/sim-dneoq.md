---
title: scx_simulator turns libbpf-sys's vendored-libbpf and static-libbpf features back on for every embedder, including ktstr's docs.rs build
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.609738633+00:00
updated_at: 2026-09-25T03:43:12.609738633+00:00
---

# Description

scx_simulator depends on libbpf-sys with default features, and uses it only for headers. Feature unification therefore turns libbpf-sys's vendored-libbpf and static-libbpf features back on for every embedder, including one that has deliberately turned them off.

scx_simulator/Cargo.toml has libbpf-sys = "1.6.1", default features on. Its comment says the headers are the only use: build.rs compiles the scheduler C against them, and no libbpf_sys symbol is referenced. It also says why the default features stay on: without vendored-libbpf, DEP_BPF_INCLUDE names a directory that libbpf-sys never fills.

ktstr sets libbpf-sys = { version = "1.6", default-features = false }, and its [package.metadata.docs.rs] builds with no-default-features. Its Cargo.toml comments explain that docs.rs cannot compile the vendored libbpf C stack (no flex/bison, no network). The release-candidate scratch consumer copies that setup. At integration 24d864c6, cargo tree -e features -i libbpf-sys on it shows:

    libbpf-sys feature "default"
    └── scx_simulator v1.0.0
    libbpf-sys feature "static-libbpf"
    └── libbpf-sys feature "vendored-libbpf"
        └── libbpf-sys feature "default" (*)

So once ktstr depends on scx_simulator, ktstr's docs.rs build compiles vendored libbpf again. Not measured on docs.rs itself. The same applies to any build of ktstr with default features off. scxsim-workload-ir's ingest feature depends on scx_simulator, so it carries the same effect.

Fix, one of:
- Vendor the handful of libbpf headers the build uses (bpf_helpers.h and the headers it includes, LGPL-2.1 OR BSD-2-Clause) into scx_simulator and drop the libbpf-sys dependency. This also removes the links = "bpf" one-version constraint that the Cargo.toml comment works around.
- Or keep libbpf-sys but find the headers without vendored-libbpf (for example from the libbpf-sys source directory), so the dependency can be default-features = false.

Either way, an embedder can then keep its own libbpf-sys feature choice. Until then the release notes should say that depending on scx_simulator enables vendored libbpf, and that an embedder with a docs.rs setup like ktstr's must make the dependency optional and leave it out of the docs.rs feature set.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
