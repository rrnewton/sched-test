---
name: sync-upstream
description: Update scx submodule to latest upstream main and fix simulator C wrappers to compile and pass tests
---

# SCX Upstream Sync Skill

You are tasked with syncing the scx simulator with the upstream sched-ext/scx
repository. The `scx` git submodule tracks upstream scheduler source code. When
upstream changes, the C wrapper files in `scx-sim/schedulers/` may need updates
to compile and pass tests.

## Context

- **scx/**: Git submodule pointing to `sched-ext/scx` upstream
- **scx-sim/schedulers/**: C wrapper files that compile upstream BPF schedulers as userspace C
- **scx-sim/csrc/**: Shared simulator C infrastructure (sim_wrapper.h, stubs, etc.)
- **lib/scxtest/**: BPF test wrapper library
- **scx-sim/validate.sh**: Validation script (fmt, clippy, nextest, doc-tests, stress smoke)
- **scheds/include/ -> scx/scheds/include**: Symlink to upstream headers
- **scheds/rust/ -> scx/scheds/rust**: Symlink to upstream scheduler Rust code
- **scheds/vmlinux/ -> scx/scheds/vmlinux**: Symlink to vmlinux.h

## Architecture Overview

The simulator compiles BPF scheduler code (`.bpf.c` files) as regular userspace C
by using a header-guard trick:

1. `sim_wrapper.h` includes `common.bpf.h` (sets its header guard)
2. It then overrides BPF macros (BPF_STRUCT_OPS, kfuncs, helpers) with C equivalents
3. Each scheduler's `wrapper.c` includes `sim_wrapper.h` first, then the upstream `.bpf.c`
4. The scheduler code compiles as C functions callable from the Rust simulator

### Supported Schedulers

Each scheduler directory under `scx-sim/schedulers/` contains:
- `wrapper.c`: The main wrapper (scheduler-specific overrides + `#include` of upstream source)
- `config.mk` (optional): Extra CFLAGS, include paths, source patching rules

Current schedulers: **simple**, **cosmos**, **lavd**, **mitosis**, **tickless**

### Common Breakage Patterns

When upstream changes, these are the typical failures:

1. **New kfuncs**: Upstream adds a new `extern __ksym` kfunc that doesn't exist in the simulator.
   Fix: Add a stub function (either in the wrapper.c or in `sim_bpf_stubs.c`).

2. **New BPF helpers**: Upstream uses a BPF helper not overridden in `sim_wrapper.h`.
   Fix: Add a `#undef` + `#define` override in `sim_wrapper.h` or the scheduler wrapper.

3. **New struct fields**: Upstream adds fields to `task_ctx`, `cpu_ctx`, or other structs.
   Fix: Usually no action needed (structs are compiled from source). But if the wrapper
   accesses those fields (probes, setup functions), update accordingly.

4. **New global variables**: Upstream adds `const volatile` globals that need initialization.
   Fix: Add initialization in the scheduler's `*_setup()` function.

5. **New BPF maps**: Upstream adds new maps that need registration.
   Fix: Add `scx_test_map_register()` calls in the `*_register_maps()` function.

6. **Division by zero**: BPF division by zero returns 0; C crashes with SIGFPE.
   Fix: Add guards in `config.mk` sed patches or in wrapper.c overrides.

7. **New source files**: Upstream splits code into new `.bpf.c` files.
   Fix: Add `#include` for the new file in the wrapper.c.

8. **Removed/renamed symbols**: Upstream renames or removes globals/functions.
   Fix: Update wrapper references accordingly.

9. **New compat macros**: Upstream adds new `__COMPAT_*` wrappers in `compat.bpf.h`.
   Fix: Add corresponding `#undef` in `sim_wrapper.h`.

10. **New SCX_* enum values**: Upstream adds new enum constants in enums.autogen.bpf.h.
    Fix: Add `#undef` in `sim_wrapper.h` to use the real vmlinux.h values.

## Prerequisites

Before starting, verify:
1. You are on a branch appropriate for this sync
2. The scx submodule has been updated to the target upstream commit
3. The working directory is the repository root

## Sync Procedure

### Phase 1: Analyze Upstream Changes

1. **Identify the commit range:**
   ```bash
   cd scx
   git log --oneline OLD_COMMIT..NEW_COMMIT
   ```

2. **Focus on changes affecting our wrappers:**
   ```bash
   # Changes to common infrastructure
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/include/ scheds/vmlinux/

   # Changes to each supported scheduler's BPF code
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/rust/scx_cosmos/src/bpf/
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/rust/scx_lavd/src/bpf/
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/rust/scx_mitosis/src/bpf/
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/rust/scx_tickless/src/bpf/

   # Simple uses a local copy -- check upstream for comparison
   git diff OLD_COMMIT..NEW_COMMIT -- scheds/c/scx_simple.bpf.c
   ```

3. **Categorize changes by impact:**
   - **Harmless**: Internal logic changes, new scheduler features that don't affect compilation
   - **Needs wrapper update**: New kfuncs, helpers, globals, struct changes
   - **Needs infrastructure update**: Changes to common.bpf.h, compat.bpf.h, enums

### Phase 2: Build and Fix

1. **Try building first -- many upstream changes are harmless:**
   ```bash
   cd scx-sim && cargo build --release 2>&1
   ```

2. **For each compilation error, identify the root cause:**
   - Read the error message carefully
   - Find the upstream change that caused it
   - Apply the minimal fix (see Common Breakage Patterns above)

3. **Iterate: fix one error, rebuild, fix the next.** Compilation errors often
   cascade, so fixing the first one may resolve several others.

4. **For each scheduler that fails to compile:**
   - Check if the scheduler's upstream source files changed
   - Check if shared infrastructure (common.bpf.h, compat.bpf.h) changed
   - Apply scheduler-specific fixes in its `wrapper.c` or `config.mk`

### Phase 3: Fix Tests

1. **Run the full validation:**
   ```bash
   cd scx-sim && bash validate.sh
   ```

2. **If tests fail:**
   - Read test output carefully
   - Determine if the failure is from a wrapper bug or a behavioral change
   - Fix wrapper issues; for behavioral changes, update test expectations

3. **If the Rust code needs updates** (e.g., new FFI functions, changed symbols):
   - Check `scx-sim/crates/scx_simulator/src/ffi.rs` for symbol loading
   - Update symbol names or add new ones as needed

### Phase 4: Final Validation

```bash
cd scx-sim && bash validate.sh
```

ALL checks must pass:
- `cargo fmt --check`
- `cargo clippy --all -- -D warnings`
- `cargo nextest run --workspace`
- `cargo test --workspace --doc`
- `stress.py` smoke tests

### Phase 5: Commit and Report

**CRITICAL**: Your final commit message MUST include the status for CI detection.

#### On Success

```bash
git add -A
git commit -m "$(cat <<'EOF'
Sync scx submodule to upstream <short-hash>

Status: SUCCESS

## Summary
- Upstream commits: N
- Schedulers updated: list
- Infrastructure changes: list
- All tests passing
EOF
)"
```

#### On Failure

If you cannot resolve all issues:

1. **Commit your progress** with fixes so far
2. **Create WHY_THIS_BRANCH_IS_BROKEN.md** in the repository root:

```markdown
# Why This Branch Is Broken

**Date**: YYYY-MM-DD
**Upstream commit**: <full-hash>
**Previous commit**: <full-hash>

## Blocking Issues

### Issue 1: <title>
- **Scheduler**: <name>
- **Error**: <compilation/test error>
- **Root cause**: <what changed upstream>
- **Attempted fix**: <what was tried>
- **Suggested fix**: <what a human should do>

## Partial Progress

- [x] Submodule updated
- [x] simple: compiles
- [ ] cosmos: blocked on <issue>
- [x] lavd: compiles
- [ ] tests: N/M passing
```

3. **Final commit:**
```bash
git add -A
git commit -m "$(cat <<'EOF'
Sync scx submodule to upstream <short-hash>

Status: FAILURE
Reason: <brief explanation>

## Summary
- Upstream commits: N
- Blocking issues: list
- See WHY_THIS_BRANCH_IS_BROKEN.md for details
EOF
)"
```

## Error Handling

- **Build failures**: Fix compilation errors iteratively. Each fix should be minimal.
- **Test failures**: Debug carefully. The simulator must maintain kernel fidelity.
- **New dependencies**: If upstream adds new build dependencies, document them.
- **Unclear changes**: When in doubt about an upstream change's impact, add a
  conservative stub (return 0 / no-op) rather than guessing at complex behavior.

## Important Guidelines

- **Minimal changes**: Only modify what's necessary to compile and pass tests.
  Don't refactor, don't clean up, don't improve. Just fix the breakage.
- **One scheduler at a time**: Fix each scheduler independently. If one is
  intractable, document the failure and move on to the next.
- **Preserve existing behavior**: The wrappers should continue to work the
  same way. New upstream features don't need to be fully simulated -- stubs
  are acceptable.
- **Check sim_wrapper.h first**: Many fixes belong in the shared infrastructure
  rather than individual wrappers.
- **Commit incrementally**: Don't accumulate too many changes in one commit.
  Commit after fixing each scheduler.

## Output Format

At the end of your work, print a summary to stdout:

```
## Sync Summary

**Status**: SUCCESS | FAILURE | PARTIAL

**Upstream commits processed**: N
**Previous submodule commit**: <hash>
**New submodule commit**: <hash>

**Scheduler status**:
- simple: OK | BROKEN (reason)
- cosmos: OK | BROKEN (reason)
- lavd: OK | BROKEN (reason)
- mitosis: OK | BROKEN (reason)
- tickless: OK | BROKEN (reason)

**Test results**: N passed, M failed

**Issues encountered**:
- Issue 1 (resolved/unresolved)

**Files modified**:
- path/to/file1
- path/to/file2
```
