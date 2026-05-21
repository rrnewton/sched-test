# Comparing Two Schedulers

> **Status — stub.** This recipe will show the canonical "same
> workload, different `--scheduler`" comparison loop.

Sketch:

```bash
WORKLOAD=examples/cpu_bound.json

for sched in simple lavd cosmos; do
    scxsim run -s "$sched" --cpus 4 --duration 200ms \
        --perfetto "/tmp/$sched.json" \
        "$WORKLOAD"
done
```

Then open each `/tmp/<sched>.json` in <https://ui.perfetto.dev/> and
compare per-CPU utilization, slice lengths, and idle periods.

For numeric comparison, capture `--structops-jsonl` for each run and
diff via `scripts/compare_live_vs_scxsim_calls.sh` (or a per-counter
`jq` pipeline).
