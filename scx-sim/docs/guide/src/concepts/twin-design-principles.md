# Twin Design Principles

scxsim has two design pillars, codified in
[`scx-sim/CLAUDE.md`][claude-md]. Internalize them before reaching
for any of the knobs:

[claude-md]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/CLAUDE.md

1. **Match production by default.**
2. **Opt-in exaggerated knobs for stress.**

These shape every default in the CLI and every promise the simulator
makes about its output.

## Principle 1: Match production by default

The default invocation —

```bash
scxsim run -s lavd --cpus 4 --duration 200ms examples/cpu_bound.json
```

— must produce a trace whose scheduler behaviour matches what
running the same scheduler on a live kernel against the same workload
would produce. "Match" is measured along three concrete axes:

- **Same struct_ops + helper call sequence.** Capture both with
  `--structops-jsonl` (simulator) and `scripts/probes/structops_full.bt`
  + `scripts/probes/helpers_full.bt` (kernel under bpftrace), then
  diff via `scripts/compare_live_vs_scxsim_calls.sh`. A divergence
  is a debt to file, not an "expected difference."
- **Same exit kind.** Whether the workload stalls, completes, or
  exhausts a controller should match. If a Bug-1-class stall
  reproduces in production but the simulator emits `ExitKind::Normal`,
  the simulator is wrong.
- **Same timing shape.** Per-task run-duration and inter-arrival
  distributions (from `--verbose-summary` or
  `--structops-jsonl`) should mirror live captures within engine-noise
  bounds.

The bpftrace probes, the JSONL diff harness, and the `bug_finding/`
matrix runner are the *canonical fidelity check*. When this check
fails, the bug is in scxsim, not in the user's workload.

## Principle 2: Opt-in exaggerated knobs

There is also a category of "make rare things happen often" knobs:
stress-test mode. These are useful — they collapse the bug-discovery
loop from hours of fuzzing to seconds — but they distort the trace,
and a reader who doesn't know they are on will mis-interpret what they
see. So they are **always opt-in**, never on by default, and they
self-name as exaggeration:

| Knob | Default | What it exaggerates |
|---|---|---|
| `--stochastic-timer-interleave` | off | Hoist BPF-timer events into cgroup_bw yield sites, modelling otherwise-rare timer-vs-dispatch race windows. |
| `--stochastic-timer-interleave-one-in N` | 4 (when interleave on) | The rate at which eligible timers fire at yield sites. |
| `--preemptive` | off | Preempt mid-C-code at random RBC intervals (implies `--interleave`). Stress-tests preemption safety of scheduler internals. |
| `--rbc-ns 100` (10× default) | 10 | Exaggerate per-RBC time charge; makes the scheduler look heavier and stretches preemption windows. |
| `--interleave` | off | Run dispatch callbacks for multiple idle CPUs on separate OS threads with PRNG token-passing. |
| `--native-concurrent` | off | True OS-thread parallelism for dispatch with clock-window sync. |

Two design rules ensure these knobs cannot be mistaken for production
behaviour:

- **The name tells you.** Flags with `--stochastic-*`, `--preemptive`,
  `--native-concurrent`, exaggerated numeric values, etc. cannot be
  passed accidentally and not noticed.
- **The trace header records them.** Concurrency / interleaving
  options reflect in the preemption-trace metadata so a stored trace
  is always self-describing.

## Two run modes

Putting the two principles together gives two run modes:

| Mode | Use for | Example invocation |
|---|---|---|
| **Production fidelity** (default) | "Does this bug reproduce?" "Does this workload behave like production?" | `scxsim run -s lavd --cpus 4 --duration 200ms examples/cpu_bound.json` |
| **Stress / fuzz** | "Are there *any* rare race conditions in this scheduler?" "Find me an interleaving that hits this assertion." | `scxsim run -s lavd --cpus 4 --duration 1s --preemptive --stochastic-timer-interleave --seed entropy <workload>` |

Production fidelity must come first when investigating a real bug.
If the bug doesn't reproduce in production-fidelity mode, switching to
stress mode and finding *some* interleaving that does is a much weaker
finding than reproducing the same shape live observed.

## Bug investigation workflow

The recommended loop:

1. **Capture live.** Run the failing workload under bpftrace probes;
   collect structops/helpers JSONL.
2. **Reproduce in production-fidelity mode.** No stress flags. Pin
   `--seed`, pin the scheduler `.so` (`--scheduler-file`).
3. **Diff.** Compare scxsim's `--structops-jsonl` against the live
   bpftrace JSONL. Divergence ⇒ scxsim debt; reach for `vm-run` as
   a cross-check.
4. **Only then reach for stress.** Use `--preemptive` /
   `--stochastic-timer-interleave` *as hypothesis probes* once the
   baseline reproduces, to test "would this also stall under a
   different interleaving?" — not to fish for stalls in the first
   place.

Source: `scx-sim/CLAUDE.md` "CRITICAL: Twin Design Principles" section
(introduced in commit `88e4388`). See also
[Running Simulations → The `run` Subcommand](../running-simulations/run.md)
for the practical flag reference.
