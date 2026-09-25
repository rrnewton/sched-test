---
title: 'with_source_patches is a silent no-op for tickless, lavd and layered: only cosmos''s wrapper includes the patched copy'
status: open
priority: 1
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T00:29:32.455648039+00:00
updated_at: 2026-09-25T00:29:32.455648039+00:00
---

# Description

build_schedulers applies SchedulerDefinition::source_patches by writing a patched copy of <scx_root>/scheds/rust/scx_<name>/src/bpf/main.bpf.c into its output dir as <name>_main_patched.c and prepending -I<out>. The patch reaches the compiler only if the scheduler wrapper.c includes <NAME_main_patched.c> with an angle include. Only cosmos/wrapper.c does. tickless, lavd and layered include "main.bpf.c", which resolves to the unpatched upstream file through -I<scx_root>/scheds/rust/scx_<name>/src/bpf.

Measured on branch feat/scxsim-cratesio-release-candidate (scratch consumer built against the worktree sources, rustc 1.97.1, SimBuildInputs::build_bundled). Each scheduler got one extra patch that inserts "#error SOURCE_PATCH_APPLIED" before the line char _license[] SEC("license") = "GPL";
- cosmos: the build fails at the #error, in cosmos_main_patched.c. The patch was applied.
- tickless, lavd, layered: the find-assert passes and <name>_main_patched.c contains the #error. libscx_<name>.so still builds, from the unpatched source. Silent.
- mitosis, simple: the build panics with "read .../scheds/rust/scx_<name>/src/bpf/main.bpf.c: No such file or directory". mitosis compiles mitosis.bpf.c and simple has local source. Loud, but the message does not say the scheduler cannot take patches.

Why it matters for publishing: with_source_patches is part of the documented onboarding chain (SchedulerDefinition::new names it "for a patched scheduler"). The embedder-facing docs on SchedulerDefinition::source_patches and with_source_patches do not mention the wrapper requirement; only the SchedulerManifest field doc does. The existing drift guard (the find-assert) passes, so an embedder has every reason to believe the patch was applied. cosmos has its patch because BPF integer divide-by-zero yields 0 while native C raises SIGFPE. The same kind of fix on lavd would be dropped without a word, and the .so would run the unpatched logic.

Fix, either of:
(a) build_schedulers refuses when source_patches is non-empty and the wrapper text does not include <NAME_main_patched.c>. The refusal names the include to add. Cheap and loud.
(b) Make patching independent of the wrapper: write the patched copy as <out>/<name>_patched/main.bpf.c and prepend that directory to -I. A quoted "main.bpf.c" from wrapper.c then finds it first, because the staged wrapper directory holds no main.bpf.c. The patched file's own quoted includes still resolve through the scx_bpf_dir -I. cosmos could then drop its special include.
In both cases, give mitosis and simple a message that names the real reason.

Acceptance: a test builds a bundled scheduler other than cosmos with a patch that plants #error. Under (b) the build must fail at the #error; under (a) it must fail with the refusal.
