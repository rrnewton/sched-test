---
title: Upstream scx_layered match_substr() reads uninitialised 'y' — breaks cgroup-contains matching in instrumented builds
status: open
priority: 1
issue_type: bug
created_at: 2026-08-12T19:17:55.425533398+00:00
updated_at: 2026-08-12T19:17:55.425533398+00:00
---

# Description

`scx/scheds/rust/scx_layered/src/bpf/util.bpf.c:150` declares `y` uninitialised and then reads it before it is ever assigned:

    int str_len, match_str_len, x, y;        /* y uninitialised */
    ...
    bpf_for(x, 0, MAX_PATH) {
            if (str_len - x < y)             /* reads y before ANY assignment */
                    break;
            bpf_for(y, 0, MAX_PATH) { ... }  /* only here does y get a value */
    }

On the first outer iteration `y` is read uninitialised — undefined behaviour in C. The BPF verifier would reject or zero it, but scxsim compiles this as userspace C, so `y` is whatever the register or stack slot happens to hold, which depends on codegen.

EFFECT: with `SCX_SIM_COVERAGE=1` the instrumentation changes register allocation, `y` holds garbage larger than `str_len`, the outer loop breaks on its first iteration, and `match_substr()` returns 'no match' for a string that does contain the substring. `MATCH_CGROUP_CONTAINS` silently stops working.

OBSERVED: `layered::cgroup_suffix_and_contains_match` (layered.rs:880) fails — cgroup 'xmidy' should match `CgroupContains("mid")` (layer 1) but lands in the catch-all (layer 2). Only the assertion needing a POSITIVE contains-match fails; the 'plain/' case expects no match and passes vacuously, and suffix matching is a different function and is unaffected.

EVIDENCE (measured at feat/layered-support tip e012423, clean detached checkout, OSS LLVM 18.1.8):
  coverage build      : 0/20 runs pass  (deterministic)
  non-coverage build  : 20/20 runs pass (deterministic)
  same commit, same machine, same shell — only the build config differs.
  Also fails at 1fe2cea, the commit that ADDED the test, so it is not a regression — the test has
  never passed under instrumentation.

VERIFIED FIX — one line, initialise y:

    -	int str_len, match_str_len, x, y;
    +	int str_len, match_str_len, x, y = 0;

  after patching: coverage build 20/20 on the failing test, layered suite 27/27,
  full workspace `cargo test --all` 1036 passed / 0 failed / 16 ignored (was 1035/1/16).

The patch was applied to the submodule working tree only to prove causation and then REVERTED; no submodule commit, no pin change.

CONTEXT: the same file already carries upstream commit ad1b85f3 'scx_layered: prevent offset calculation during prefix matching from being optimized out' — upstream has been bitten by codegen sensitivity here before. `match_substr()` was added by 9c237fd4 'layered: add cgrp contains matcher', which is the newest commit touching the file on both the pinned SHA 59c30bae and the locally-fetched origin/main, so the bug is still present upstream.

# Design

The fix belongs upstream in sched-ext/scx, not as a local patch — it is a genuine correctness bug there too (any compiler or flag change can flip it). Two steps:

1. Send `int y = 0` (or hoist the length check inside the inner loop) as a PR to sched-ext/scx. Owner's call per the push policy: scx origin is sacred, personal branches go to rrnewton/scx.
2. Until it lands upstream, scxsim's layered coverage builds will show this one red test. Either carry it as a known-failure with a pointer to this issue, or hold a local patch — but do NOT commit a submodule pin that is not an ancestor of upstream main.

Worth a wider check: this class of bug (uninitialised read that the BPF verifier would catch but userspace C does not) can hide anywhere scxsim compiles BPF as userspace C. Building the schedulers with -Wuninitialized / -Wmaybe-uninitialized, or a UBSan/MSan pass, would find the rest cheaply.

# Acceptance Criteria

- layered::cgroup_suffix_and_contains_match passes under SCX_SIM_COVERAGE=1 as well as a normal build.
- The fix is upstream in sched-ext/scx (or the local carry is documented with a link to the upstream PR).
- Ideally: schedulers built with -Wuninitialized so the next instance fails the build rather than one test in one build config.
