---
title: OSS copyright headers are absent from ~90% of source files
status: open
priority: 2
issue_type: task
created_at: 2026-08-19T22:34:42.923460014+00:00
updated_at: 2026-08-19T22:34:42.923460014+00:00
---

# Description

Surfaced while draining the PR queue (tg `drain-sched-test-prs-linear-integration`), closing PR #25 `opensource-compliance`.

## The measurement

Sampling 251 tracked non-submodule `.c` / `.h` / `.rs` files at integration d1c776c and checking the first 8 lines of each for a `Copyright` line:

    sampled=251  missing_copyright=226

So roughly 90% of source files carry no copyright header.

## Why this is being filed rather than fixed here

PR #25 (`opensource-compliance` -> `simulator.v5`) would have added headers to 136 files. It was closed as obsolete and that call stands: its base `simulator.v5` is a retired lineage that is not an ancestor of `simulator.v6` or `integration`, it had been CONFLICTING since 2026-06-05, and its two other deliverables are already in the tree (root `LICENSE`, and `scx_simulator` at version 1.0.0). Its April file list no longer matches the tree, so resurrecting the branch would be a 426-file rebase off a dead base to obtain something a script can regenerate correctly against the current tree.

PR #51 `fix/oss-compliance-warnings` did land and covered LICENSE, README, the rand advisory and 27 files of headers — so this is the remaining tail, not the whole job.

## What closing this looks like

Generate the headers against the CURRENT tree rather than porting April's list. Decide first:

- which paths are in scope — `scx/` is a submodule and is out; `scx-sim/schedulers/*/wrapper.c` includes vendored upstream scx source as a single translation unit and may want different treatment
- whether generated files and vendored `csrc`/`scxtest` (vendored in 74dc43f) are in scope
- whether a CI check should keep it from regressing, which is the only thing that stops this recurring

Worth pairing with that last point: a gate is cheaper than a second sweep.
