---
title: 'Licensing: scx_simulator''s license field lacks the (LGPL-2.1-only OR BSD-2-Clause) term, and scxtest/ is unmarked GPL-2.0-derived code'
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.617206957+00:00
updated_at: 2026-09-25T03:43:12.617206957+00:00
---

# Description

scx_simulator's license field does not cover two upstream headers it ships. Its scxtest/ directory is GPL-2.0 code derived from upstream scx and says so nowhere. The other five crates' fields match the SPDX lines and LICENSE files they ship.

The license fields, as packaged from the release-candidate branch (integration 24d864c6):
- scx_simulator, scx_layered_alloc, scx_layered_growth: BSD-3-Clause AND GPL-2.0-only. alloc and growth ship upstream's alloc.rs and layer_core_growth.rs under vendor/, with upstream's GPL-2.0 LICENSE beside them.
- scx_perf: BSD-3-Clause AND BSD-2-Clause, with LICENSE-REVERIE. The Cargo.toml comment says BSD-2-Clause covers the code taken from Reverie and BSD-3-Clause covers this repo's changes to it.
- scxsim-build, scxsim-workload-ir: BSD-3-Clause.

1. The SPDX lines in scx_simulator's 272 packaged .c/.h/.rs files: 105 GPL-2.0, 4 (GPL-2.0-only OR BSD-2-Clause), 2 (LGPL-2.1 OR BSD-2-Clause), and 161 with no SPDX line. The two dual-licensed headers are vendor/scx/scheds/include/lib/alloc/bpf_helpers_local.h and vendor/scx/scheds/include/scx/task_local_data.bpf.h. Neither of their options is in the expression. bpf_helpers_local.h is also compiled in, because lib/sdt_task.bpf.c, lib/sdt_alloc.bpf.c and lib/sdt_cgroup.bpf.c include it. The four (GPL-2.0-only OR BSD-2-Clause) arena headers are already covered by the GPL-2.0-only term.

2. scxtest/ holds 9 files derived from upstream scx lib/scxtest, which upstream deleted at 54e1ea429 (2026-06-04, 'Remove scxtest BPF unit testing infrastructure'). scx_test.c and scx_test.h are identical to upstream's last version. The other seven have been extended here, for example overrides.c from 185 to 306 lines and scx_test_map.c from 288 to 575. The code is compiled in: scx_simulator's build.rs compiles scx_test.c, scx_test_map.c and scx_test_cpumask.c, and scxsim-build compiles overrides.c into every scheduler .so.
   - Upstream's copies had no SPDX line and fell under the scx repo's GPL-2.0 LICENSE. Ours have no SPDX line, no copyright notice and no note of their origin.
   - They sit outside vendor/, so the only licence text beside them is the crate's BSD-3-Clause LICENSE.
   - The package expression includes GPL-2.0-only, so it is not wrong. But a reader cannot tell which files the GPL term covers. sim-cxpef leaves open whether scxtest is in scope for its header sweep. For a published crate it has to be.

3. scx_perf's src/lib.rs has no copyright or licence header, while LICENSE-REVERIE opens with Reverie's own sentence 'Copyright notices are include in each source file'. Nothing else about scx_perf's licensing looks wrong. Whether to relicense the Reverie-derived code is not raised here.

Fix before publishing:
- Add the missing term to scx_simulator's field, e.g. 'BSD-3-Clause AND GPL-2.0-only AND (LGPL-2.1-only OR BSD-2-Clause)'. LGPL-2.1 is the deprecated SPDX spelling of LGPL-2.1-only.
- Give each scxtest/ file an SPDX line (GPL-2.0-only) and a one-line origin note naming upstream lib/scxtest at 54e1ea429^. Or move the directory under vendor/, next to upstream's LICENSE.
- Give scx_perf's src/lib.rs a header naming its Reverie origin.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
