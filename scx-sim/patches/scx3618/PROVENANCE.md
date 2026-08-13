# scx#3618, carried out-of-pin

`scx3618-vs-pin-59c30bae.patch` is the [scx#3618](https://github.com/sched-ext/scx/pull/3618)
change, expressed against **our scx pin `59c30bae`** so it applies strictly.

It is carried as a patch file rather than as a submodule pin bump because
**`a8f72d09` is not in upstream `main`'s history at all** — scx#3618 is an
unmerged pull request. Pinning it would put the submodule on a ref that upstream
may rebase or abandon, and that no one else can resolve from `sched-ext/scx`.
Carrying the patch keeps the before/after reproducible without doing that.

### The pin invariant, stated correctly

Getting this wrong has already cost us, so it is spelled out rather than
paraphrased.

- **Wrong:** *"the gitlink must name an ancestor of upstream `main`."*
- **Right:** *"the pin's **base** must be an ancestor of upstream `main`"* — i.e.
  the pin must share history with upstream, while being free to carry local
  commits on top of it.

The naive form is **false every day**, because our pin legitimately carries
local patches. As of 2026-08-13 the pin `59c30bae` carries exactly one:

```
96e4f928  (base — an ancestor of upstream main c630d994)   ✔ invariant satisfied
└─ 59c30bae  lib/cgroup_bw: add scxsim targeted yield hooks   ← ours, deliberately
```

So `git merge-base --is-ancestor 59c30bae <upstream main>` answers **no**, and
that is correct and expected. The check that matters is
`git merge-base --is-ancestor $(git merge-base 59c30bae <upstream main>) <upstream main>`,
which answers **yes**.

Why the distinction is not pedantry: a check encoding the naive form fires
constantly, gets ignored as noise, and is eventually "fixed" by deleting the
local patches so it passes. That is exactly what a bare `git checkout
origin/main` in a sync workflow does — silently — and it is why that workflow
needed repairing. **If you are writing a pin check, check the base.**

None of this is why scx#3618 is carried out-of-pin. Its reason is stronger and
unrelated: the PR head is not in upstream history at all, so there is no base to
be an ancestor of anything.

## Why this file exists

Earlier rounds of this investigation applied the patch into the submodule
working tree and never committed it. The consequence was not a tidiness problem:
**results were published without recording which side of the before/after was
patched**, and at least one "unpatched" measurement was in fact patched. The
conclusion drawn from it — that the fix does not bound the wait — was wrong and
has been withdrawn. See PR sched-test#104.

So the rule this directory enforces is: *no before/after claim without a build
state that a reader can reconstruct.*

## Provenance

| | |
|---|---|
| upstream PR | `sched-ext/scx#3618` |
| PR head | `a8f72d09e27f47dfdd97fb41ed929c5380462010` |
| PR head subject | `lib/cgroup_bw: blend wall-clock time into BTQ vtime to bound throttle delay` |
| merge-base with our pin | `3aa52aafef9a99db682ed3d77b51d524641f5883` |
| our pin | `59c30baee7d7a70f32f3983cfb3fe2f383a144b0` (`v1.1.2-65-g59c30bae`) |
| shape | 3 files, +332 / -71 |
| sha256 of this patch | `7e9754f9e5f949c8f834890b74027495622a3c7939e4db51ae8e6c9a9469f446` |

The three upstream commits it squashes:

```
a8f72d09  lib/cgroup_bw: blend wall-clock time into BTQ vtime to bound throttle delay
5a05be1d  lib/cgroup_bw, scx_lavd: add scx_cgroup_bw_pressure() API
50ad8fcd  lib/cgroup_bw: factor taskc-cached cgx/llcx accessors into helpers
```

## How it was generated, and the one caveat

```bash
git -C scx fetch origin 'refs/pull/3618/head:refs/remotes/origin/pr-3618'
git -C scx diff 3aa52aaf a8f72d09 > /tmp/upstream.patch   # sha256 01f5dd7d…
git -C scx apply -3 /tmp/upstream.patch                    # 3-way REQUIRED
git -C scx diff HEAD > scx3618-vs-pin-59c30bae.patch       # strict against our pin
```

**Caveat, stated because it is the kind of thing that later gets assumed away:**
the upstream diff does **not** apply strictly to `59c30bae` — it needs `-3`,
because our pin carries `lib/cgroup_bw.bpf.c` changes the PR branch does not.
The carried patch is therefore the *result of a three-way merge*, not a
byte-exact copy of the upstream change. Its `--stat` is identical to upstream's
(+332/-71 over the same 3 files), which is good evidence the merge was
uneventful, but it is evidence and not proof. If upstream rebases the PR,
regenerate rather than reconcile by hand.

## Use

```bash
scx-sim/scripts/scx3618_patch.sh status   # what state is the tree in
scx-sim/scripts/scx3618_patch.sh apply
scx-sim/scripts/scx3618_patch.sh revert
```

### Exit codes — check them, because silence is not success

`scx3618_patch.sh` is meant to be called from scripts, so its contract is
stated rather than left to be inferred:

| exit | meaning |
|---|---|
| `0` | the requested action succeeded; `status` printed the current state |
| `1` | refused, with a reason on **stderr** — wrong pin, dirty scx tree, already applied, missing patch file, bad subcommand |

It never exits non-zero silently, and that is deliberate rather than incidental.
The first version of this script did exactly that: `applied=$(grep -l "$MARKER"
… | wc -l)` under `set -e` with `pipefail` aborts the entire script when `grep`
finds nothing — which is the **clean, expected** case — exiting 1 having printed
nothing at all. A caller that does not check the status reads no-output-no-error
as fine and proceeds to measure a build it never verified. That is precisely the
failure this directory exists to prevent, reintroduced in the tool built to
prevent it.

**So: check the exit status, and treat empty output as a bug, not a pass.** The
same applies to `verify_scx3618_build_state.sh` — if it lists no `.so` at all,
nothing has been built yet; that is not a clean tree.

Then confirm what you actually built, from the artifact rather than the tree:

```bash
scx-sim/scripts/verify_scx3618_build_state.sh
```

Checking the source tree is not sufficient. A stale `.so` from a previous build
will happily be reused, so the only trustworthy statement about a run is what
was in the binary that ran.

`verify_scx3618_build_state.sh` deliberately reports **every** profile it finds,
not just the one you last built. `scxsim run` uses the release `.so` while
`cargo nextest` uses the debug one, so building release and then running tests
measures a binary you did not build. Read the line for the profile you are
actually about to exercise.

## Verified round-trip

Measured 2026-08-13 from committed state, patch state confirmed from the
artifact immediately before each run:

| build | command exit | result |
|---|---|---|
| unpatched (pin) | 42 | `ExitKind::ErrorStall pid=4 runnable_for_ns=39879690773` — 39.88s |
| patched (this patch applied) | 0 | ran to completion, no stall |

An incremental `cargo build --release` does pick the change up; no `clean` is
needed. That was checked rather than assumed — the release `.so` flipped from
`unpatched` to `PATCHED` across a 24-second incremental rebuild.
