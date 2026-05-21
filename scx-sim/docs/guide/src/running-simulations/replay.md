# Replaying Preemption Traces

> **Status — stub.** This page will document the
> record → replay loop used to harden non-deterministic preemption
> sites into a reproducible trace.

Sketch:

1. **Record** during a normal `scxsim run`:
   ```bash
   scxsim run -s lavd --cpus 4 --duration 200ms \
       --record-preemptions /tmp/preempts.txt \
       examples/cpu_bound.json
   ```
2. **Replay** later with `scxsim replay`:
   ```bash
   scxsim replay /tmp/preempts.txt
   ```
3. Options on the `replay` subcommand mirror their `run` counterparts:
   `--scheduler-file`, `--verbose-summary`, `--record-preemptions`
   (re-record), `--no-pmu-signal`, `--preempt-mode {pmu|e9patch}`,
   `--wait-debugger`.
