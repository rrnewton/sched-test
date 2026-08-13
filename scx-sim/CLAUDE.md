
scx_simulator: Development Guidelines
=======================================================

This document contains the development guidelines and instructions for the project. This guide OVERRIDES any default behaviors and MUST be followed exactly.

If you become stuck with an issue you cannot debug, you can file an issue for it and leave it to work on other topics. Of course, the tests should be always passing before each commit and achieve reasonably good code coverage as described below.

CRITICAL: No-Stub Rule (scxsim runs 100% of scheduler logic)
================================================================================

**scxsim MUST execute 100% of the same scheduler logic that runs on the BPF
side. NEVER stub out, no-op, elide, or otherwise replace any part of a BPF
scheduler with a fake implementation.** A scheduler is NOT considered
"supported" in scxsim until the entirety of its BPF logic is actually being
executed during simulation.

This rule applies to:

- BPF scheduler `.bpf.c` files compiled into the scheduler library (e.g.
  `scx_lavd`, including all of its helper translation units).
- BPF helper / library code that the scheduler calls into (e.g.
  `cgroup_bw.bpf.c` and friends).
- Any state machine, accounting, or decision logic implemented on the BPF
  side. The Rust-side scxsim runtime models the *kernel/BPF substrate* — it
  does NOT replace the scheduler's own logic.

Concretely, the following are FORBIDDEN as ways to "support" a scheduler in
scxsim:

- **No-op shim functions.** `sim_*_*()` wrappers that return success
  without doing the work, "to be filled in later."
- **Elided libraries.** Quietly omitting a translation unit (e.g. dropping
  `cgroup_bw.bpf.c` from the scheduler library build) so the scheduler
  appears to compile and run while a chunk of its logic is missing.
- **Interface-only Rust reimplementations.** Re-implementing a BPF
  subsystem in Rust on the scxsim side, providing the same *interface*
  the scheduler calls into, but only *approximating* the BPF semantics
  rather than executing the BPF code itself. The Rust replacement is
  not the scheduler — it is a guess at what the scheduler does.
- **Silent fallbacks** that swap real logic for a simplified path
  (covered also by `No Silent Failures` below; doubly forbidden when the
  silent fallback replaces scheduler code).

If a BPF feature genuinely cannot run under scxsim today (e.g. it depends
on a kernel facility we have not yet modeled in the scxsim BPF substrate),
that is a **scxsim infrastructure task** — file an issue, build the
substrate. It is NOT a license to stub the scheduler. Until the substrate
exists, the affected scheduler is "not yet supported," not "supported with
a shim."

When a temporary deviation from real scheduler logic is unavoidable during
development, mark it with `DANGER TODO(<issue>)` in the code (see
`Kernel Fidelity` below) and treat the scheduler as unsupported until the
TODO is resolved. `DANGER TODO` is for transient development state — it is
NOT a way to permanently legitimize a stub.

Worked-example failure mode (cpu-bw-stall-bug):
The cpu-bw-stall-bug investigation was derailed for weeks by exactly this
antipattern: `sim_cgroup_bw_*` wrappers written as no-op shims, and a
Rust `BandwidthManager` state machine standing in as a fake interface
that *approximated* `cgroup_bw.bpf.c` without preserving its semantics.
The scheduler under test was effectively not running its bandwidth logic
at all under scxsim, so the stall reproducer was diagnosing the shim, not
the scheduler. A cold fresh-agent reproduction, an independent
sev-reader hypothesis comparison, and the `cgroup-bw-audit` (audit
`audit-cgroup-bw-real-shim-state-202605`) all converged on the same
diagnosis. See:

- Design doc:
  `experiments/lavd_cpubw_stalls_202604/SCXSIM_REAL_CGROUP_BW_LIBRARY_DESIGN.md`
- Audit: `audit-cgroup-bw-real-shim-state-202605`

This rule exists so that recurrence is impossible. Reviewers MUST refuse
to land scxsim integrations that stub, no-op, or elide BPF scheduler
logic, regardless of how convenient the shortcut looks.

CRITICAL: Don't Model the Scheduler — Model the Kernel
================================================================================

This rule EXTENDS the No-Stub Rule above. The No-Stub Rule covers the
obvious antipattern (no-op shims, elided libraries). This rule covers
the subtler — and historically more damaging — antipattern: a Rust
reimplementation that *looks correct*, mirrors the scheduler's
interface, maintains its own parallel state, and produces plausible
behavior — but is a **fake approximation** of what the scheduler does
rather than the scheduler itself.

User mandate (web 2026-05-13):
> *Move towards NOT modeling any fake approximation of schedulers but
> just modeling what the kernel actually does with the proper
> relationship to the running SCX scheduler.*

The principle in one line: **scxsim models the kernel; the SCX
scheduler models the scheduler.** Their relationship inside scxsim must
mirror the kernel↔BPF-scheduler relationship in production.

What scxsim's Rust runtime DOES model (the kernel's job):

- Delivering scheduler callbacks (`enqueue`, `dispatch`, `runnable`,
  `quiescent`, `tick`, etc.) at the right times, with the right
  arguments, in the right order.
- Owning the run queues, DSQs, and per-CPU dispatch state THAT THE
  KERNEL OWNS.
- Advancing simulated time, accounting runtime, raising scheduler
  ticks, delivering wakeups and IPIs.
- Providing the kfunc / helper surface that BPF programs see.
- Faithfully simulating the BPF substrate (maps, timers, iterators,
  per-CPU storage) the scheduler runs on top of.

What scxsim's Rust runtime MUST NOT model (the scheduler's job):

- Cgroup CPU bandwidth enforcement (throttle / put-aside / refill /
  unthrottle decisions). These belong to `cgroup_bw.bpf.c` running
  *inside the linked-in scheduler library*, not to a Rust
  `BandwidthManager` running alongside it.
- Dispatch policy (which task to pick next, fairness, vruntime, BTQ
  ordering, latency-criticality scoring, etc.). These belong to the
  scheduler's own `.bpf.c`.
- Any other accounting, decision, or state-machine logic that lives in
  the BPF scheduler in production.

The test (apply this to every Rust struct/function in the scxsim
runtime path):

> For this piece of Rust code that maintains state about what the
> scheduler is doing — or that decides what the scheduler should do —
> ask: **In production, does the KERNEL maintain that state / make
> that decision, or does the BPF SCHEDULER?**
>
> - If the **kernel** owns it → fine, this is legitimate scxsim engine
>   code.
> - If the **BPF scheduler** owns it → DELETE the Rust code. Let the
>   scheduler's own BPF code execute and own that state. The Rust
>   replacement, no matter how carefully written, is a guess at what
>   the scheduler does. The scheduler is the only code that knows what
>   the scheduler does.

Why this is *stronger* than the No-Stub Rule:

A no-op stub is obviously wrong — it returns success without doing the
work, and any reviewer can see the lie. A **fake approximation** is
much more dangerous because it *looks correct*: the Rust code has
plausible state, plausible transitions, plausible outputs. It will
match the scheduler's behavior on the easy cases and diverge on the
exact corner cases that bugs hide in. Worse, the scheduler's real BPF
logic is typically *not running at all* on the path the approximation
covers, so production-faithful diagnosis is impossible. The
cpu-bw-stall-bug investigation lost weeks to exactly this confusion.

Worked example (canonical): `BandwidthManager`.
The Rust `BandwidthManager` at
`scx-sim/crates/scx_simulator/src/safe/cgroup_bw.rs` (518 lines, under
`#![forbid(unsafe_code)]`) was an interface-shaped Rust state machine
that approximated `cgroup_bw.bpf.c` — the same enforcement library
linked into the scheduler. It maintained parallel cgroup state,
parallel quota/refill bookkeeping, and parallel throttle decisions.
Both the rule above and the audit (`audit-cgroup-bw-real-shim-state-202605`)
identified it as a textbook fake approximation. The fix is the
in-flight tg task
`shrink-rust-bandwidthmanager-518-to-30-lines-no-fake-approximation`:
delete the fake-approximation code; the scheduler's `cgroup_bw.bpf.c`
becomes the single source of truth, and the surviving ~30 lines of
Rust are limited to what the kernel genuinely owns (e.g. delivering
the BPF timer that drives refill).

When the audit
`audit-scxsim-for-other-fake-approximation-violations` finds further
candidates, apply the test above and remove them by the same pattern.
Every Rust line that models scheduler-side state is a line where
scxsim and the production kernel can disagree — and disagree
silently.

Reviewers MUST refuse to land code that re-introduces fake
approximations under any name (`*Manager`, `*State`, `*Tracker`,
`*Cache`, `Sim*`) when the production owner of that state is a BPF
scheduler.

CRITICAL: Twin Design Principles (match production + exaggerated knobs)
================================================================================

The two No-Stub rules above govern WHAT executes inside scxsim — the
scheduler's own BPF code, not a Rust approximation of it. The Twin
Design Principles below govern HOW the scxsim engine, harness, and
infrastructure surrounding the scheduler must BEHAVE. They are
complementary, not redundant: the No-Stub rules prevent fake
SCHEDULERS; the Twin Design Principles prevent a fake KERNEL /
HARNESS underneath an otherwise-real scheduler.

User mandate (web 2026-05-14):
> *(1) be able to model production as closely as possible, always try
> to match to what the live kernel does and what we can observe in
> traces of live kernel workloads.*
>
> *(2) have knobs to selectively EXAGGERATE dimensions for stress
> testing (e.g. extra delays) beyond what is probable on live kernel /
> real HW.*

Principle 1 — Match Production by Default
------------------------------------------

The scxsim engine, harness, and infrastructure default to **matching
what the live kernel does and what we can observe in traces of live
kernel workloads.** Any divergence from production behavior is
**technical debt** — file it explicitly (a tg task, a `DANGER TODO(<issue>)`
in code, or both) and either pay it down or make it an opt-in knob
under Principle 2.

Concrete checkpoints scxsim must match production on by default:

- **Engine timing.** Per-task runtime accounting, period boundaries,
  IPI semantics, scheduler tick delivery, watchdog firing. Charging
  runtime to a task that wasn't actually on-CPU during that interval
  (the V4-A engine over-charge bug) is a Principle 1 violation.
- **BPF substrate semantics.** Kfunc return values, map operations,
  per-CPU storage, BPF timer firing, iterator scope. The scheduler
  observes a substrate that behaves like the kernel, not a
  scxsim-specific approximation.
- **Trace stream.** The bpftrace structops/helpers tracer (the
  side-by-side BPF-call diff harness — `scxsim --trace-format perfetto`
  + the matched live-kernel bpftrace recipe + the JSONL emitter +
  `bug_finding/` diff tooling) is the **canonical fidelity check**.
  Two runs of "the same workload" — one inside scxsim, one inside a
  live kernel under wprof — should produce equivalent BPF call
  streams within documented tolerances. Divergence in that diff
  *is* the debt-discovery channel.
- **Scheduler observation.** Anything the BPF scheduler can see
  (CPU count, cgroup hierarchy, task state transitions, queue
  occupancy, vtime, wake flags) must reflect what the kernel would
  show. The scheduler must EXPERIENCE scxsim the same way it would
  experience the kernel.

Worked example (canonical anti-pattern): the cpu-bw-stall-bug
**engine over-charge** finding (V4-A trace evidence,
`audit-cgroup-bw-real-shim-state-202605`) and its 2-line surgical
fix in V4-C. scxsim's engine was passing a stale `prev_task` to
`lavd_dispatch(cpu, prev)` on idle CPUs, causing LAVD's
`account_task_runtime` → `scx_cgroup_bw_consume(prev->cgroup,
~100M ns/period)` to charge runtime to a task that wasn't actually
on-CPU. The result: scxsim reproduced cpu-bw-stall-bug via a
**different mechanism** than the live kernel does (PR #3521
timer-MIN-bound regression). Both bugs were real, both produced the
same observable symptom (LAVD `runnable task stall` watchdog → cgroup
permanently throttled), but they were different bugs. The engine
divergence had been silent debt for the entire investigation; once
named (V4-A, via `CgroupBwConsumeNs` TraceKind), it became fixable
(V4-C, +2 lines clearing `prev_task=None` on idle CPUs).

Permanent infrastructure for catching the next Principle 1 violation
of this class:

- `CgroupBwConsumeNs` and `CgroupBwReplenish` TraceKinds expose every
  consume / refill of cgroup_bw debt with the causal fields
  (`keep_throttled`, `debt`, `period_budget_*`) queryable via
  trace_processor SQL.
- `CbwPutAside`, `CbwDrainBtqBatch`, `cbw_throttle_cgroups`,
  `LavdBailOnCgroupThrottle`, `LavdReenqueueViaBtqDrain`,
  `CgroupSetBandwidth`, `CgroupInit/Exit/Move` complete the
  cgroup-bw-relevant trace surface.
- The bpftrace structops/helpers tracer + scxsim JSONL emitter +
  `bug_finding/` diff harness operationalize the live ↔ scxsim
  comparison.

Principle 2 — Exaggerated-Knob Stress Testing (Opt-In Only)
------------------------------------------------------------

Optional, **opt-in** modes that push scxsim BEYOND production
behavior are explicitly allowed and encouraged for robustness
testing — race-window enlargement, extra delays in dispatch /
wake-up paths, jitter in IPI delivery, artificial cgroup-quota
oscillation patterns, etc. They exist because some bug classes only
surface under exaggerated conditions that production rarely (but
not never) encounters.

Every such knob MUST be:

- **Explicitly opt-in.** A CLI flag (`--stochastic-timer-interleave`,
  `--charge-granularity tick`), a fixture-config field, or an env
  var. Default behavior is **never** exaggerated.
- **Self-documenting.** The name describes what dimension is being
  exaggerated. `--extra-dispatch-delay-ns` is good; `--mode2` is
  not.
- **Clearly distinguished from production-fidelity baseline mode.**
  A trace, log, or report produced under exaggerated knobs MUST
  surface the active knob set (e.g., in the run header) so that
  downstream consumers cannot mistake exaggerated-mode output for
  production-fidelity output.
- **Documented at the call-site and in `--help`.** State what the
  knob exaggerates, why it exists (which bug class it targets),
  and the production-fidelity cost (e.g., "race-window enlargement;
  may produce events that are physically possible but extremely
  rare on real hardware").

Existing examples that follow this discipline:

- `--stochastic-timer-interleave` (PR #32, `concurrent-mode-phase3`):
  random interleaving of cgroup_bw timer firing with dispatch
  decisions to surface timer-vs-dispatch races. Off by default;
  surfaces in trace headers when on.
- `--charge-granularity tick` (`charge-granularity-experiment`):
  per-tick (HZ=250) cgroup_bw charging instead of per-stop charging;
  intended to characterize sensitivity to charge-rate granularity.
  Off by default.

Anti-pattern: a knob whose default-on behavior diverges from
production. If you find yourself writing one, you have either
(a) a Principle 1 violation that should be tracked as debt and
fixed in the engine, or (b) an opt-in knob that has been mis-wired
on. Neither is acceptable.

How the Two Principles Interact
--------------------------------

A scxsim run is in one of two **modes**:

1. **Production-fidelity mode (default).** No exaggerated knobs
   active. The engine behaves as Principle 1 demands. Output is
   trustworthy as a stand-in for live-kernel observation, subject to
   any documented (filed-as-debt) divergences. This is the only mode
   in which a scxsim result should be cited as evidence of how live
   production behaves.
2. **Stress-test mode (any exaggerated knob active).** One or more
   opt-in knobs are on. Output is useful for surfacing race classes
   and corner cases, but its quantitative claims do **not**
   automatically transfer to production. A bug found in stress mode
   must be reproduced in production-fidelity mode (or shown
   equivalent in the live kernel) before it can be claimed as a
   production bug.

When investigating a bug:

- Always start in production-fidelity mode. If production-fidelity
  reproduces the bug, you have a candidate live-kernel bug.
- If production-fidelity does NOT reproduce the bug but stress-mode
  does, you have a candidate race-class hypothesis — test it against
  the live kernel before publishing.
- If scxsim reproduces a live bug via a *different mechanism* than
  the live kernel does (the V4-A vs PR #3521 situation), you have
  TWO bugs: a Principle 1 violation in the scxsim engine AND the
  live-kernel bug. Both need to be fixed.

Reviewer Rule
--------------

Reviewers MUST refuse to land:

- Engine / harness / infrastructure changes that introduce silent
  divergence from production behavior, with no `DANGER TODO(<issue>)`
  marker and no tg task tracking the debt.
- Knobs that exaggerate beyond production but default to ON, or that
  do not surface their active state in trace / log output.
- Test fixtures or canonical reproducers whose results would be
  mis-citable as live-kernel evidence because the run was actually
  in stress-test mode.

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

Walkthroughs and demos need an adversarial reviewer
-----------------------------------------------------

Owner policy, standing (2026-08-12). Any walkthrough, demo, tutorial or
reproducer document must be checked by an adversarial reviewer who is **not
the author**, and who does **both** of these:

1. **Reads the code** and compares the document against what is actually on
   disk and actually running — every attribute, flag, path, command, SHA and
   claimed relationship, checked against the tree at the pinned commit.
2. **Runs every command the document gives**, at that commit, and confirms the
   output matches what is shown. *Reviewing by reading is not reviewing.*

### Honest-about-caveats and technically-correct are DIFFERENT properties

`ai_docs/BOTH_BACKENDS_DEMO_20260812.md` was reviewed and passed. What the
review actually checked was whether the document was **honest about its
limitations** — it disclosed the recorded-VM caveat, the missing IR dump, the
failing calibration metric. All true, all properly flagged. Nobody asked
whether the content was **correct**, and it showed `#[ktstr_test]` where its
central claim required `#[ktstr_scenario]`.

A document can be scrupulously honest about what it does not show while being
wrong about what it does show. Confirming the caveats are complete is not the
start of this review, let alone the end of it.

The author of that document flagged its own limitations unprompted and still
got the central attribute wrong. Author diligence does not substitute for an
adversarial second pass.

### Traps

- **Ignore-aware greps lie about absence.** ripgrep, ugrep and `git grep` skip
  untracked, ignored and other-worktree paths. Confirm any "this symbol does
  not exist" claim with
  `find . -name '*.rs' -print0 | xargs -0 grep -l <symbol>` first.
- **Name the commit you checked against**, or "not in the tree" means nothing.
- **A command you did not run is not verified** — say which and why.

This complements, and does not replace, the honesty requirements elsewhere in
this file (No-Stub, No Silent Failures, the tiered-reporting discipline).

Cache Reproducer Methodology
----------------------------------------
See `CACHE_REPRODUCER.md` (in this directory) for the authoritative methodology document covering:
- Valid mode × scheduler matrix (rtapp_sim × EEVDF is IMPOSSIBLE)
- Calibration parameters and production reference values
- Dependent variable metrics (E2E latency, scheduling latency, IRQ exposure)
- Statistical requirements (N≥3 reps, randomized order, warmup exclusion)
- Data provenance rules (every number must cite source file and computation)

Workflow: Commits and Version Control
================================================================================

Initial Clone Setup
--------------------------------------------

After cloning the repository, you **must** initialize git submodules before
building. The project depends on the `scx` submodule (sched_ext kernel headers
and BPF helpers). Without it, builds will silently fail with missing headers.

    git submodule update --init --recursive

This is a one-time step per clone. If you see build errors about missing
`<scx/common.bpf.h>` or similar headers, this is almost certainly the cause.

Rebuilding after a scx submodule SHA swap
--------------------------------------------

Whenever the `scx` submodule's SHA is swapped (matrix testing, bisect,
checking out a PR ref), the OFFICIAL way to pick up the new scheduler
source is plain `cargo build`:

    cargo build --release -p scx_simulator --bin scxsim

The build script's `cargo:rerun-if-changed=` list covers the scx
submodule subtrees that wrappers `#include` from (`scx/lib`,
`scx/scheds/rust/scx_lavd/src/bpf`, `scx/scheds/rust/scx_mitosis/src/bpf`,
`scx/scheds/rust/scx_cosmos/src/bpf`,
`scx/scheds/rust/scx_tickless/src/bpf`, `scx/scheds/include`,
`scx/scheds/vmlinux`), so cargo correctly re-runs the build script
and rebuilds the `.so` files when those files change.

For a discoverable single command that combines the SHA swap + rebuild
+ post-build sha256 verification — useful for matrix testing and for
the canonical reproducer report — use:

    make rebuild-schedulers                  # rebuild against current SHA
    make rebuild-schedulers SCX_SHA=<sha>    # check out SHA in scx/, then rebuild

Both forms force re-run of the build script (defensive belt-and-
suspenders for cases where a stale on-disk cache predates the
build.rs fix) and print the resulting `libscx_*.so` paths and sha256
sums. Idempotent: re-running with no source changes is a fast no-op.

DO NOT use the legacy workaround `touch crates/scx_simulator/build.rs`
— that hack predates the build.rs `rerun-if-changed=` extension and is
no longer needed. If you find a code path where plain `cargo build`
silently keeps a stale `.so` after a SHA swap, that is a BUG in
`build.rs`'s rerun-if-changed list (a watched scx subtree is missing).
File an issue, add the missing path, do not paper over it with `touch`.

For absolute per-SHA isolation (matrix testing where every SHA must
get a guaranteed-fresh from-scratch build, immune to any cache
confusion), see `experiments/bug1_scx_version_matrix_20260512/build_per_hash.sh`,
which uses per-SHA `CARGO_TARGET_DIR` to sidestep the question entirely.

Clean Start: Before beginning work on a task
--------------------------------------------

Make sure we start in a clean state. Check that we have no uncommitted changes in our working copy. Perform `git pull origin <BRANCH>` to make sure we are starting with the latest version on our branch. Check that `./validate.sh` passes in our starting state.

Pre-Commit: checks before committing to git
--------------------------------------------

Run `./validate.sh` and ensure that it passes or fix any problems before committing.

Also include a `Test Results Summary` section in every commit message that summarizes how many tests passed of what kind.

If you validate some changes with a new manual or temporary test, that test should be added to either the unit tests or integration tests and it should be called consistently from `./validate.sh`.

NEVER add binary files or large serialized artifacts to version control without explicit permission. Always carefully review what you are adding with `git add`, and update `.gitignore` as needed.

### Pre-commit hook (cargo fmt + clippy gate)

The repo ships a pre-commit hook at `scx-sim/scripts/git-hooks/pre-commit`
that automatically enforces the same `cargo fmt` and `cargo clippy` gates
that `validate.sh` runs — but scoped to the changes you are STAGING, so
it stays fast and only flags issues you introduced.

**Install once per clone (and re-run after every fresh worktree):**

    ./scx-sim/scripts/git-hooks/install.sh

The installer copies the hook into `<git-common-dir>/hooks/pre-commit`,
which is shared across all worktrees of the sched-test repo.

**What the hook does:**

- Skips entirely when no staged file lives under `scx-sim/`.
- `rustfmt --check` against each staged `.rs` file under `scx-sim/`
  (typically <1s; pre-existing formatting backlog in untouched files
  does NOT punish you).
- `cargo clippy --all-targets --workspace --no-deps -- -D warnings`
  from `scx-sim/` when any staged `.rs` / `Cargo.{toml,lock}` is under
  `scx-sim/` (~5–30s warm; `--no-deps` keeps it fast — CI still runs
  the full version).

**Escape hatches** (DO NOT use routinely — CI still rejects fmt/clippy
violations regardless of how the hook was bypassed):

    SKIP_SCXSIM_HOOK=1 git commit ...     # bypass entire hook
    SKIP_SCXSIM_CLIPPY=1 git commit ...   # bypass clippy only (still fmts)
    SCXSIM_HOOK_FULL_CLIPPY=1 git commit  # opt INTO full clippy (slower, matches CI)

**Why this exists:** repeated `cargo fmt` + clippy backlog accumulation
on `simulator.v6` (~16 violations cleared by PR #36 / the
`fix_simulator_v6_cargo` task; more accumulated again immediately
after) — agents commit without running `validate.sh`. The hook is the
mechanical gate that prevents recurrence; running `validate.sh`
manually before commit remains the gold standard (it also runs tests,
typecheck, conflict-marker check, etc. that the hook does not).

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

For long-lived shared branches that exist on both remotes, such as
`simulator.vN` or `work/N`, keep the fork and upstream in sync. After
validation, push the same branch to both remotes:

    with-proxy git push origin <branch>    # facebookexperimental/sched-test
    with-proxy git push mirror <branch>    # rrnewton/sched-test fork

Do not let `mirror/<branch>` drift from `origin/<branch>` silently. If only the
fork is writable or upstream auth fails, push to `mirror` only and explicitly
record the auth gap in the task/PR notes.

### You own your PR until it lands — no languishing work

Owner policy, standing (2026-08-12). Finishing the code is not finishing the
task. The flow is:

    work -> commit as you go -> rename off `agent/*` -> push (origin AND
    mirror) -> open PR against `integration` -> (optional reviewer-agent
    pass) -> LAND IT -> PUSH THE MIRROR AGAIN

- **AFTER MERGING A PR, PUSH THE MIRROR EXPLICITLY.** This is the step the
  flow above hides, and it catches everyone exactly once. Merging on GitHub
  writes the merge commit to **origin only** — `rrnewton/sched-test` is not a
  GitHub-native mirror, so nothing propagates. Lockstep is automatic for your
  *branch* pushes and NOT automatic for the *merge*. The moment your PR goes
  green-and-merged, `mirror/integration` is behind by exactly your merge
  commit, and nothing tells you:

      with-proxy git fetch origin integration
      with-proxy git push mirror FETCH_HEAD:refs/heads/integration
      # then prove it, do not assume it:
      for r in origin mirror; do \
        echo "$r $(with-proxy git ls-remote $r refs/heads/integration | awk '{print $1}')"; done

  Observed 2026-08-12 on PR #69: origin `841a3c8`, mirror still `80f9d78`
  immediately after the merge. Found only because the SHAs were compared;
  "I pushed both remotes earlier" was true and irrelevant.
- **OWN YOUR PR UNTIL IT LANDS.** Do not hand back a branch and walk away.
- **NEVER LEAVE UNCOMMITTED CHANGES LOCALLY.** Commit as you go; WIP messages
  are fine. Committing is not a claim that the work is done.
- **BEFORE GOING IDLE OR FINISHING:** worktree clean, work pushed, PR open or
  landed — and say so explicitly in your final note.

Pause for the owner only to land something RED, to rewrite shared history, or
to change the scx submodule pin. Everything else you land.

This does not relax anything above: `agent/*` names are local scratch and must
be renamed before any push, both remotes stay in lockstep, PRs target
`integration` rather than `main`, and an scx pin that is not an ancestor of
upstream `main` is never committed.

Note the interaction with the testing rule above: "do not push untested code"
still stands and is not an exception to this. It means *test it, then push* —
run the tests yourself, install what you need. It does not mean park the
branch and wait.

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

Set up the Python venv (one-time, requires network):

    python3 -m venv .venv && .venv/bin/pip install plotly pandas

(On Meta corporate machines behind a proxy, prefix pip with `with-proxy`.)

The benchmark scripts auto-detect `.venv/bin/python3` if available.

Dependencies and Missing Software
========================================

### Required system packages (Ubuntu/Debian)

The following packages must be installed before building:

    sudo apt-get install -y clang llvm libelf-dev zlib1g-dev \
        build-essential xxd pkg-config

- **clang / llvm**: Used as the C compiler for BPF scheduler code compiled as
  userspace C. Set `BPF_CLANG` to override the default `clang`.
- **libelf-dev**: Required by libbpf-sys (BPF object loading/parsing).
- **zlib1g-dev**: Required by libbpf-sys (compressed ELF support).
- **build-essential**: Standard C toolchain (gcc, make, etc.).
- **xxd**: Hex dump utility used during the build.
- **pkg-config**: Locates system libraries during `cargo build`.

### Rust toolchain

Install Rust via [rustup](https://rustup.rs/). The project uses Rust edition
2021; any recent stable toolchain should work.

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

### Optional: cargo-nextest (faster test runner)

    curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C ${CARGO_HOME:-~/.cargo}/bin

### Fedora/RHEL equivalents

    sudo dnf install clang llvm elfutils-libelf-devel zlib-devel \
        gcc make vim-common pkgconf-pkg-config

### Philosophy

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
- **e9patch**: `make install-e9patch` (requires network). After install: `make -C schedulers e9` to build instrumented scheduler libraries. (On Meta corporate machines behind a proxy, prefix with `with-proxy`.)

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

Read `scx-sim/.claude/agents/orchestrator.md` (relative to the repo root) for the full orchestrator protocol. The key principle:
you coordinate and delegate, you do NOT implement. All code changes, testing,
and validation are done by sub-agents working in their assigned worktrees.
