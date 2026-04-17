
scx_simulator: Development Guidelines
=======================================================

This document contains the development guidelines and instructions for the project. This guide OVERRIDES any default behaviors and MUST be followed exactly.

If you become stuck with an issue you cannot debug, you can file an issue for it and leave it to work on other topics. Of course, the tests should be always passing before each commit and achieve reasonably good code coverage as described below.

Coding conventions
========================================

You HATE duplicated code. You follow DRY religiously and seek clean abstractions where functions are short, and complexity is factored into helpers, traits, and centralized infrastructure that is shared as much as possible. You hate duplication so much that you would rather centralize repetitive code EVEN if it means the interface to the shared functionality becomes fairly complex (e.g. the shared logic uses callbacks with complex types for the pieces that vary between use cases).

You dislike function definitions that are longer than necesasry, and in particular those over 150 lines long. These would be better factored into a clear spine of calls to helper functions/macros that are factored out.

You also dislike long files. Whenever a file grows longer than 1500 lines you propose ideas for breaking it into separate modules.

PREFER STRONG TYPES. Do not use "u32" or "String" where you can have a more specific type or at least a type alias. "String" makes it very unclear which values are legal. We want explicit Enums to lock down the possibilities for our state, and we want separate types for numerical IDs and distinct, non-overlapping uses of basic integers.

Delete trailing spaces. Don't leave empty lines that consist only of whitespace. (Double newline is fine.)

Adhere to high-performance Rust patterns (unboxing, minimizing allocation, etc). In particular, adhere to the below programming patterns / avoid anti-patterns, which generally fall under the principle of "zero copy":

- Avoid the pattern of returning allocated collections when they are not necessary (e.g. return an iterator).
- Avoid clone: instead take a temporary reference to the object and manage lifetimes appropriately.
- Avoid collect: instead take an iterator with references to the original collection without copying.

Read OPTIMIZATION.md for more details.

Python Code
----------------------------------------
All Python code must be strictly typed with type annotations on all
function signatures (parameters and return types). Use `mypy --strict`
or `pyright` for type checking. This is enforced by `validate.sh`.

NEVER commit files containing your local username, home directory paths, or other
machine-specific absolute paths. Use relative paths, `~`, environment variables,
or generic placeholders like `<REPO_ROOT>` instead.

Documentation and Analysis
========================================

When creating analysis documents, specifications, or other AI-generated documentation, place them in the `ai_docs/` directory. This keeps the top-level clean and makes it clear which documents are AI-generated analysis (and may become outdated) versus core project documentation.

Cache Reproducer Methodology
----------------------------------------
See `CACHE_REPRODUCER.md` for the authoritative methodology document covering:
- Valid mode × scheduler matrix (rtapp_sim × EEVDF is IMPOSSIBLE)
- Calibration parameters and production reference values
- Dependent variable metrics (E2E latency, scheduling latency, IRQ exposure)
- Statistical requirements (N≥3 reps, randomized order, warmup exclusion)
- Data provenance rules (every number must cite source file and computation)

Workflow: Commits and Version Control
================================================================================

Clean Start: Before beginning work on a task
--------------------------------------------

Make sure we start in a clean state. Check that we have no uncommitted changes in our working copy. Perform `git pull origin <BRANCH>` to make sure we are starting with the latest version on our branch. Check that `./validate.sh` passes in our starting state.

Pre-Commit: checks before committing to git
--------------------------------------------

Run `./validate.sh` and ensure that it passes or fix any problems before committing.

Also include a `Test Results Summary` section in every commit message that summarizes how many tests passed of what kind.

If you validate some changes with a new manual or temporary test, that test should be added to either the unit tests or integration tests and it should be called consistently from `./validate.sh`.

NEVER add binary files or large serialized artifacts to version control without explicit permission. Always carefully review what you are adding with `git add`, and update `.gitignore` as needed.

Pre-submit or push: also validate
---------------------------------

Even if we didn't COMMIT new code, but are just rebasing or restacking changes or merging, we should still run `./validate.sh` to make sure we are in a good state before submitting or pushing.

Testing Required Before Push: preferrably agent-driven, manual if necessary
---------------------------------------------------------------------------

**NEVER push code that has only coded but never tested and actually run.**

Try hard to test code YOURSELF (as the agent), installing dependencies
as needed, using VMs, debuggers, or other tools at your disposal.
If you get completely blocked, sak for help with manual testing, but don't
just commit code ignoring testing.

If you cannot test due to sandboxing, permissions, or missing dependencies:
1. **Do NOT push the code**
2. Ask the user to run the manual test command
3. Wait for confirmation that it works before pushing

Amending commits
----------------------------------------

It is fine to amend the most recent commit (git commit --amend) as long as it has NOT been pushed to the remote yet. If the commit has already been pushed, create a new commit instead.

Branches and pushing
----------------------------------------

The `main` branch is protected. Never push directly to main. Only push to feature branches after validation. Don't force push unless you're asked to or ask permission.

Issue Tracking
========================================

We use minibeads (`mb`) for local issue tracking. Run `mb quickstart` to learn
the commands. Use `mb ready` to find the next issue to work on, and update issue
status as you work (`mb update sim-N --status in_progress`, `mb close sim-N`).

File issues for bugs, TODOs, and feature work rather than leaving stale TODO
comments in code. Reference issue IDs (e.g. sim-1) in commit messages when
closing issues.

NEVER edit .beads/ files directly. Always use the mb CLI to create,
  update, and close issues. Direct edits produce corrupt issue files
  (wrong filename format, missing metadata) that break the tracker.

Performance Benchmarks
========================================

Throughput benchmarks measure speedup factor (simulated time / wall-clock time).
They are NOT part of `validate.sh`.

### Running benchmarks

    make benchmark                           # Full suite, current results
    ./scripts/run_benchmark.sh               # Full suite, records to history CSV

### Post-commit: benchmark tracking

Run `./scripts/periodically_run_benchmarks.sh` after committing. This runs the
full benchmark suite if >= 5 commits have elapsed since the last recorded
benchmark. If it modifies `data/benchmarks/<CPU>/perf_history.csv`, make a
follow-up commit with the updated CSV.

Only runs meaningfully on the primary benchmark target machine (identified by
CPU model name in the CSV directory structure).

### Dependencies

Set up the Python venv (one-time, requires network via with-proxy):

    python3 -m venv .venv && with-proxy .venv/bin/pip install plotly pandas

The benchmark scripts auto-detect `.venv/bin/python3` if available.

Dependencies and Missing Software
========================================

Never work around a missing dependency with a compromised fallback. If software
is needed, install it — build from source, use the package manager, or ask the
user for help. The `~/bin/` directory is on `$PATH` for locally-built tools.

**NEVER skip a validation step because a tool is missing.** Install the tool
and run the check. If installation fails, escalate to the human — do NOT
silently skip the check and report success. A skipped check is a lie. Examples:

- mypy not found → `pip install mypy` (or `.venv/bin/pip install mypy`), then run it
- clippy not available → install it, don't skip lint
- a test runner is missing → install it, don't skip tests

The same principle applies to test failures: if a test fails, fix it or
escalate. Never comment out, skip, or ignore a failing test to make the suite
"pass."

Common tools already available:

- **rt-app**: `~/bin/rt-app` (built from `~/playground/rt-app`)
- **bpftrace**: system-installed
- **mb** (minibeads): local issue tracker
- **e9patch**: `make install-e9patch` (requires network; use `with-proxy make install-e9patch` on Meta machines). After install: `make -C schedulers e9` to build instrumented scheduler libraries.

Every TODO in source code MUST reference an issue: `TODO(sim-XXXXX)`. Do not
leave TODOs without a tracking issue — file one first, then add the TODO.

No Silent Failures
========================================

We AVOID silent failures at all cost. We prefer fatal errors with clear
messages. When something fails that would produce incorrect results, panic or
return an error -- never silently degrade.

Examples:
- If a hardware breakpoint cannot be created in replay mode, PANIC with a
  message explaining that replay requires HW breakpoints. Do NOT fall back to
  cooperative-only mode with a `tracing::warn()` -- that produces wrong results.
- If a trace file's parameters (CPUs, tasks, seed, duration) do not match the
  current scenario, PANIC with a clear mismatch message. Do NOT silently
  proceed with mismatched parameters.
- If a PMU timer fd is invalid in replay mode, PANIC. Without it, preemption
  points cannot be reproduced.

The philosophy: it is better to crash loudly with a clear error than to
silently produce incorrect results that waste hours of debugging.

PMU RBC Determinism
========================================

PMU Retired Branch Conditional (RBC) counters ARE deterministic for a given
sequential instruction stream. This is the foundational principle of Mozilla
RR and Hermit. **Never claim that RBC counter values are inherently
nondeterministic due to hardware or microarchitecture.** That is false.

Two things are true simultaneously:
- **Counter reads are DETERMINISTIC**: same instruction stream → same RBC count.
- **Signal delivery has skid**: the PMU overflow signal arrives a few
  instructions after the counter hits the target. This makes the *preemption
  point* nondeterministic, but the *counter value* at any given instruction
  is exact.

If we observe nondeterministic RBC values in the simulator, it is OUR BUG —
not hardware. Possible causes to investigate: signal handler code adding
branches, shared-library init differences, kernel-injected code (vDSO),
counter reset timing, or thread scheduling affecting instruction streams.

Kernel Fidelity
========================================

The simulator models a SUBSET of kernel behavior. That is acceptable — we cannot
simulate everything. What is NOT acceptable is admitting behaviors that are not
realizable by the kernel. Every callback invocation, flag value, and ordering
must correspond to a real kernel code path. If the kernel would not make a
particular call in a given situation, neither should the simulator.

Be extremely cautious with shortcuts that deviate from kernel semantics.
Hard-coding unconditional calls (e.g. always calling ops.dequeue regardless of
task state) is dangerous because it introduces behaviors no kernel execution
would produce. Schedulers may rely on the invariant that certain callbacks only
fire in specific states.

When a shortcut is unavoidable (e.g. because we haven't yet modeled the
requisite state), mark it with `DANGER TODO(<issue>)` in the code. The `DANGER`
prefix makes these easy to grep for and signals that the code is known to
deviate from kernel semantics. The `<issue>` is a minibeads issue ID (e.g.
`sim-1ae1a`) that describes the proper fix. Example:

```rust
// DANGER TODO(sim-1ae1a): unconditional dequeue is not kernel-accurate.
// The kernel only calls ops.dequeue when the task is in SCX_OPSS_QUEUED
// state. We need to model ops_state.
```

Multi-Agent Team Mode
========================================

When you find yourself in a `work/` directory containing multiple worktrees of
the same repository (e.g., `<MULTI_SCX>/work/` with `sched-test1/`, `sched-test2/`,
`sched-test3/`, `sched-test4/`), switch into orchestrator mode automatically.

Read `.claude/agents/orchestrator.md` for the full orchestrator protocol. The key principle:
you coordinate and delegate, you do NOT implement. All code changes, testing,
and validation are done by sub-agents working in their assigned worktrees.
