# Twin Design Principles

> **Status — stub.** This page will explain the two design pillars
> codified in
> [`scx-sim/CLAUDE.md`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/CLAUDE.md):
> match-production by default, opt-in exaggerated knobs for stress.

Planned content:

1. **Match production by default.** Engine, harness, and infra default
   to matching the live kernel + live trace observations. The
   bpftrace structops/helpers tracer and the JSONL emitter
   (`--structops-jsonl`) plus the `bug_finding/` diff harness are the
   canonical fidelity check. Divergence is debt to file — examples
   from the cpu-bw-stall-bug investigation will be linked.
2. **Opt-in exaggerated knobs.** Stress-test knobs (e.g.
   `--stochastic-timer-interleave`, `--charge-granularity tick`) are
   opt-in only, self-documenting in their name, and surface in the
   trace header so a reader can immediately tell whether a run is in
   "production-fidelity" or "stress-test" mode. Default is *never*
   exaggerated.
3. **Two run modes.** A worked example will contrast both modes on the
   same workload and show how each is intended to be used.
4. **Bug investigation workflow.** Always reproduce in
   production-fidelity mode first; reach for stress knobs only to
   probe a hypothesis once the baseline is captured.

Source: `scx-sim/CLAUDE.md` "CRITICAL: Twin Design Principles" section
(added in commit `88e4388`).
