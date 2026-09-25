---
title: 'crates.io publish: owner decisions (scx_ prefix names, 1.0.0 vs 0.1.0, public surface, licence, what blocks the first publish)'
status: open
priority: 1
issue_type: task
labels:
- cratesio
created_at: 2026-09-25T03:43:12.625623193+00:00
updated_at: 2026-09-25T04:50:46.010577974+00:00
---

# Description

The crates.io release candidate (branch feat/scxsim-cratesio-release-candidate) leaves five decisions to the owner. The release write-up is scx-sim/ai_docs/SCXSIM_CRATESIO_RELEASE_CANDIDATE_20260924.md. Treat a publish as permanent: a published version can be yanked, but yanking does not free the name and the version number cannot be reused.

1. Names, and the scx_ prefix.
   - All six names are unregistered in both spellings (crates.io sparse index 404 for each, 200 for the controls scx_utils, libbpf-rs and scx_rustland_core; checked 2026-09-25T03:38Z).
   - A crates.io search for 'scx' returns 75 crates, 32 of them starting scx_ or scx-. Nine declare repository github.com/sched-ext/scx. 21 declare none, among them upstream's scheduler crates scx_layered, scx_lavd, scx_cosmos, scx_tickless and scx_bpfland. Two declare btraven00/scx. scx_utils is owned by htejun, ByteLab-David, davemarchevsky, arighi, likewhatevs, JakeHillion and rrnewton. scx_bpfland, scx_cosmos, scx_cake and scx_arena each list htejun, arighi and rrnewton among their owners.
   - So scx_simulator, scx_perf, scx_layered_alloc and scx_layered_growth would be published from rrnewton/sched-test into a namespace sched-ext's maintainers publish in. scx_layered_alloc and scx_layered_growth would sit beside upstream's scx_layered and read as parts of it. They are adapters that compile upstream scx_layered's alloc.rs and layer_core_growth.rs. scx_perf reads as a sched-ext crate, and it is a PMU branch counter derived from Reverie.
   - Options: (a) keep the scx_ names with the sched-ext co-owners' agreement; (b) use the scxsim- prefix that scxsim-build and scxsim-workload-ir already use. Under (b), `[lib] name = "scx_simulator"` keeps every `use scx_simulator::...` path working.
   - Recommendation: (b) for scx_perf, scx_layered_alloc and scx_layered_growth. Only scx_simulator depends on them, so the rename costs nothing. Settle scx_simulator's own name with the co-owners.

2. scx_simulator 1.0.0 or 0.1.0. The branch carries 1.0.0 only because integration already did: the version arrived with #18 (ee906b05, 'scxsim 1.0 Release Candidate', 2026-05-04), an internal milestone, and the release-candidate task did not ask for it. The other five crates are new and start at 0.1.0. Recommendation: 0.1.0 for scx_simulator too.
   - sim-c8949e ('1.0 Release Candidate validation', P0) is still open with 22 unchecked items.
   - The fixes for several release-candidate findings change the public API or its behaviour: sim-4xdj4 (safe functions become `unsafe fn` or lose their re-export), sim-cp5c0 (a public KernelConfig field), sim-0qpv9 (the library takes SIM_LOCK or refuses a second load), sim-yrtox (the stdout report leaves Simulator::run), sim-dneoq (libbpf-sys features). Under 1.0.0 each of those is a 2.0.0. Under 0.1.0 they go into 0.2.0 without breaking a stability promise.

3. The size of the public surface. The crate root re-exports 162 names, besides the 25-name `prelude` that the embed contract documents. Whatever is public at the first release is what semver then protects. Two root re-exports, `unified_alloc` and `LayerDemand`, are items of upstream scx_layered's alloc.rs, compiled by scx_layered_alloc, so an upstream change to them becomes a breaking change of scx_simulator. Recommendation: before publishing, reduce the root to the prelude plus what an embedder needs, and make the rest pub(crate) or put it in a module documented as unstable.

4. Licensing. scx_simulator's expression is a conjunction that includes GPL-2.0-only, so any program that links it takes on GPL-2.0-only for the combined work. ktstr is GPL-2.0-only, so ktstr is compatible. A consumer that must ship its binaries under permissive-only terms cannot depend on it. sim-kqri1 lists the gaps to fix first: a missing (LGPL-2.1-only OR BSD-2-Clause) term, and scxtest/ files with no SPDX line or origin note.

5. What must be fixed before the first publish. Recommendation:
   - Unsound or silently wrong for an in-process consumer: sim-4xdj4, sim-0qpv9, sim-3geoh, sim-kuhrw, sim-6vhbq, sim-4b77c, sim-ld1ro.
   - Breaking to change after publication: sim-cp5c0, sim-yrtox, sim-dneoq, sim-jnh26, sim-fmijm, and item 3 above.
   - Licensing: sim-kqri1.
   - Everything else filed under the cratesio label can follow a release. Two phrases need a yes or no: 'the fbk kernel' in scxtest/kern_types.h and '(devserver toolchains)' in schedulers/lavd/wrapper.c. Neither is secret. See sim-yc6z3, item 5.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
