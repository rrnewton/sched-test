# Verifying Determinism

> **Status — stub.** This recipe will show the standard "is my
> reproducer byte-stable?" check.

Sketch:

```bash
# Run twice with the same seed; compare structops streams byte-for-byte.
scxsim run -s lavd --seed 42 --duration 200ms \
    --structops-jsonl /tmp/run-a.jsonl examples/cpu_bound.json
scxsim run -s lavd --seed 42 --duration 200ms \
    --structops-jsonl /tmp/run-b.jsonl examples/cpu_bound.json
diff /tmp/run-a.jsonl /tmp/run-b.jsonl && echo "DETERMINISTIC"
```

Or use the built-in `--determinism-check`:

```bash
scxsim run -s lavd --seed 42 --determinism-check examples/cpu_bound.json
```

which runs the simulation twice internally and compares the checkpoint
sequences, exiting non-zero on divergence.
