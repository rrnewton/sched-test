# Debugging with LLDB

scxsim is a single-process Rust binary that `dlopen`s a native
shared library built from the real sched_ext scheduler's C source
(compiled with clang's native target, not the BPF target — so the
scheduler runs as ordinary userspace machine code). Both the engine
and the scheduler are debuggable under lldb, including with full
type summaries and a Bug-1-specific diagnose command. **This is
something kernel BPF cannot offer:** there is no native debugger
for BPF bytecode running in the kernel.

This recipe walks the canonical attach loop and the helpers under
[`scx-sim/lldb_debug/`][lldb-debug].

[lldb-debug]: https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/lldb_debug

## 1. Launch with `--wait-debugger`

```bash
scxsim run -s lavd --cpus 4 --duration 1s \
    --wait-debugger \
    examples/cpu_bound.json
```

scxsim:

- loads the scheduler `.so`,
- writes an lldb breakpoint script *next to the `.so`* (so the
  script's breakpoint addresses match the just-loaded image),
- prints a copy-pasteable lldb attach command,
- spin-waits until a debugger attaches.

Sample stderr:

```text
scxsim: disabling ASLR and re-executing...
scxsim: waiting for debugger on PID 12345
scxsim: lldb attach command:
    lldb -p 12345 -s /.../libscx_lavd.so.lldb
```

(The `.lldb` extension is the breakpoint script; the path is
deterministic because ASLR is disabled and `.so` placement is
stable.)

## 2. Attach from another terminal

```bash
lldb -p 12345 -s /.../libscx_lavd.so.lldb
```

The script sets breakpoints on the standard sched_ext entry points
(`ops.select_cpu`, `ops.enqueue`, `ops.dispatch`, `ops.running`,
`ops.stopping`, `ops.tick`, etc.). A single `continue` from the
attach-stop hits the first `ops` breakpoint.

```text
(lldb) c
Process 12345 resuming
Process 12345 stopped
* thread #1, name = 'scxsim', stop reason = breakpoint 1.1
    frame #0: 0x... libscx_lavd.so`lavd_select_cpu(...)
```

## 3. Load the helpers

```text
(lldb) command script import lldb_debug/scxsim_formatters.py
(lldb) bug1_diagnose
```

`scxsim_formatters.py` registers ~10 type summaries (cgroup tree
nodes, DSQ entries, task contexts, etc.) so `p`/`po` on engine state
prints something readable instead of raw byte arrays.

`bug1_diagnose` is a custom command that walks the cgroup tree,
prints per-cgroup `runtime_ns` and `is_throttled`, and identifies
which cgroup is the deadlock pivot. It is the canonical helper for
the [Bug-1 reproducer](./repro-stall.md).

Other helpers (per `lldb_debug/README.md`):

| Helper | What it does |
|---|---|
| `cgroup_tree` | Walks and pretty-prints the cgroup hierarchy. |
| `dsq_dump <cpu>` | Prints the per-CPU DSQ state. |
| `task_ctx <pid>` | Per-task scheduler context dump. |
| `runqueue` | Global runqueue state across all CPUs. |
| `bug1_diagnose` | Bug-1 cgroup-bw stall pivot identification. |

See [`scx-sim/lldb_debug/README.md`][lldb-readme] for the full list
and the `worked_example.sh` end-to-end demo against
`bug1_canonical.json`.

[lldb-readme]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/lldb_debug/README.md

## 4. Run the workload to completion

A single `continue` from any `ops` breakpoint runs the next callback.
Run several iterations to settle the workload, then trigger the bug:

```text
(lldb) breakpoint disable 1   # stop hitting ops.select_cpu
(lldb) c
...
Process 12345 stopped
* stop reason = breakpoint 2.1  # ops.enqueue
(lldb) bug1_diagnose
cgroup /interactive: runtime_ns=80000 quota=100000 throttled=false
cgroup /background:  runtime_ns=0      quota=20000  throttled=true  *** PIVOT
...
```

For step-through inside scheduler code at the C-statement level,
use `--preempt-mode e9patch`:

```bash
scxsim run -s lavd --cpus 4 --duration 1s \
    --preempt-mode e9patch --wait-debugger \
    examples/cpu_bound.json
```

e9patch instruments the `.so` with software RBC, removing PMU skid
and making single-instruction stepping deterministic. Requires the
`_e9.so` variant; install via `scripts/install_e9patch.sh`.

## Combining with replay

The most powerful workflow: record a preemption trace, then debug
the replay (the replay is deterministic, so every `continue` lands
in the same place every time):

```bash
# 1. Record.
scxsim run -s lavd --cpus 4 --duration 200ms --preemptive \
    --record-preemptions /tmp/repro.preempts \
    examples/cpu_bound.json

# 2. Replay under debugger.
scxsim replay --wait-debugger --preempt-mode e9patch \
    /tmp/repro.preempts
```

Now every step under lldb is reproducible across attach sessions.

## Common attach pitfalls

- **ASLR not disabled.** The breakpoint script is built from the
  load-time `.so` base address; with ASLR on, every run lands at a
  different address and the script's breakpoints miss. Default
  `--no-disable-aslr` is **off**, which is correct; do not pass it.
- **Wrong `.so`.** If you rebuild the scheduler between the
  `scxsim` launch and the `lldb` attach, the script's addresses are
  stale. Don't rebuild during an attach session.
- **Attach delay.** scxsim spin-waits but does not poll forever; if
  you take more than a few minutes, it may exit. Pre-stage the
  `lldb` command in a second terminal before launching.
- **Stripped `.so`.** Type summaries require DWARF; the in-tree
  build keeps symbols by default. Stripped binaries (release-strip
  builds, third-party `.so`s) get raw byte dumps instead.

## See also

- [Recipes → Reproducing a Stall Bug](./repro-stall.md) — uses
  `bug1_diagnose`.
- [Running Simulations → Replaying Preemption Traces](../running-simulations/replay.md) — record/replay for deterministic step-through.
- [Concepts → Determinism](../concepts/determinism.md) — why ASLR
  off + e9patch yields the strongest step-through guarantees.
