# Verifying Determinism

Three ways to answer "is my reproducer byte-stable?" — from cheap
to thorough.

## 1. Built-in: `--determinism-check`

The canonical CI gate. Add the flag to any `scxsim run`:

```bash
scxsim run -s lavd --cpus 4 --duration 50ms --seed 42 \
    --determinism-check examples/hello.json
```

Last line on success:

```text
Determinism check PASSED: 22 checkpoints matched
```

On failure, scxsim prints the first divergent checkpoint and exits
non-zero. This runs the simulation twice internally with aggressive
checkpoint collection at scheduling events.

Use this in CI. Use this as the smoke check before publishing a
reproducer.

## 2. External: two runs, byte-diff the structops stream

When you want explicit evidence (e.g., to paste into a PR
description), capture two runs and diff them:

```bash
WORKLOAD=examples/cpu_bound.json
ARGS="-s lavd --cpus 4 --duration 200ms --seed 42"

scxsim run $ARGS --structops-jsonl /tmp/run-a.jsonl "$WORKLOAD"
scxsim run $ARGS --structops-jsonl /tmp/run-b.jsonl "$WORKLOAD"

if diff /tmp/run-a.jsonl /tmp/run-b.jsonl; then
    echo "DETERMINISTIC: structops stream byte-identical"
else
    echo "NON-DETERMINISTIC"
    exit 1
fi
```

The JSONL stream is byte-identical when the run is deterministic. A
non-empty diff is the failure proof.

You can do the same with `--perfetto` (Chrome JSON), `--dump-trace`
(stderr text), or `--record-preemptions` outputs; the JSONL stream
is the densest and most diff-friendly.

## 3. Cross-machine: record / replay

The strongest determinism evidence: capture a preemption trace on
machine A, replay it on machine B, and confirm the summary matches.

On machine A:

```bash
scxsim run -s lavd --cpus 4 --duration 200ms --seed 42 \
    --record-preemptions /tmp/repro.preempts \
    --structops-jsonl /tmp/A.jsonl \
    examples/cpu_bound.json
```

Ship `/tmp/repro.preempts`, the workload JSON, and (if used) the
TOML sidecar to machine B. The preemption trace header includes
`so_hash`, which is verified at load time; if machine B's `.so`
differs (different scx submodule SHA, different build flags), the
replay fails loudly rather than producing a misleading run.

On machine B:

```bash
scxsim replay \
    --record-preemptions /tmp/B.preempts \
    /tmp/repro.preempts
```

Compare the two preemption traces' bodies (strip headers; paths
differ across machines):

```bash
diff \
    <(grep -v '^#' /tmp/repro.preempts) \
    <(grep -v '^#' /tmp/B.preempts) \
    && echo "CROSS-MACHINE DETERMINISTIC"
```

This is the gold standard for "my reproducer is portable" and is
the basis for the `bin_cache` regression-bisect workflow.

## What can break determinism

If `--determinism-check` fails or diffs are non-empty, suspect (in
order):

1. **Missing `--seed`.** Default is `42` but if `SCX_SIM_SEED` is in
   your shell env, you may be picking up a different seed across
   runs.
2. **`--seed entropy`.** That's the explicit "non-deterministic"
   mode. Use a fixed u32.
3. **ASLR not disabled.** If you passed `--no-disable-aslr`, `.so`
   base addresses vary across runs and replay can drift.
4. **Different scheduler `.so`.** Cargo's `rerun-if-changed` does
   not include `scheds/rust/scx_lavd/src/bpf`. A submodule SHA bump
   without `cargo clean` can yield a stale `.so`. Pin with
   `--scheduler-file /path/to/specific/libscx_lavd.so` to be sure.
5. **Recent scxsim revision change.** Determinism is per-revision;
   the contract is per `(workload, seed, .so, scxsim revision)`. A
   `git pull` between runs is a likely culprit.
6. **A real non-determinism bug.** File it. The
   `--determinism-check` flag itself was added as the CI gate to
   catch regressions in this class.

## When to use which

| You need to … | Use |
|---|---|
| Add a CI gate to a new test | `--determinism-check` |
| Convince a reviewer in a PR | external two-runs diff |
| Ship a portable reproducer | record / replay across machines |

See [Concepts → Determinism](../concepts/determinism.md) for the
underlying contract and the hardening checklist.
