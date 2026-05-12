# scxsim debug helpers — lldb formatters for scheduler state

LAVD-focused lldb data formatters and a custom `bug1_diagnose` command,
designed for debugging the cgroup-bandwidth runnable-task stall (H6 path)
in the userspace scxsim simulator.

This sits one layer up from the agent → scxsim debugger workflow set up in
[`experiments/agent_debugger_setup_20260512/`](../../../../experiments/agent_debugger_setup_20260512/README.md):
that README picks the driver (lldb, batch + tmux). This directory adds
the LAVD-specific pretty-printers an agent uses while inside an lldb
session.

## Files

| File | Purpose |
|---|---|
| `lldb_lavd_formatters.py` | Python module: 10 type summaries + `print_and_continue` / `print_once_and_disable` breakpoint callbacks + `bug1_diagnose` custom command. |
| `init.lldb` | Settings only (SIGFPE pass-through, ASLR, step-avoid). Loads no Python — call `command script import` separately, or use `worked_example.sh`. |
| `worked_example.sh` | Reproducible end-to-end run against the Bug-1 reproducer. Outputs `phaseA.transcript.txt` and `phaseB.transcript.txt`. |
| `phaseA.transcript.txt` | Captured demonstration: 4 strategic breakpoints fire and our formatters render the locals. |
| `phaseB.transcript.txt` | Captured demonstration: `bug1_diagnose` invoked at a `check_watchdog` stop. |

## Type summaries (10)

All registered under category `lavd`. Match by regex on the fully-qualified
Rust path so both `scx_simulator::safe::types::Pid` and
`scx_simulator::types::Pid` are covered.

| Type | Format | Demonstrated in |
|---|---|---|
| `types::Pid` | `pid=N` | implicit child of SimTask, also `task.pid` |
| `types::CpuId` | `cpu=N` | child of SimTask |
| `types::Vtime` | `vtime=N` | DSQ entries |
| `types::DsqId` | `dsq=GLOBAL\|LOCAL\|LOCAL_ON(cpu=N)\|user(N)` | `phaseA bp3` (`k: dsq=user(8192)`) |
| `cgroup::CgroupId` | `cgid=N(ROOT)?` | `phaseA bp2` (`cgid: cgid=2`) |
| `task::TaskState` | `TaskState::Sleeping\|Runnable\|Running{cpu=N}\|Exited` | child of SimTask (lldb stock variant rendering can override; see Limitations) |
| `unsafe_impl::sim_task::SimTask` | one-liner with pid, name, state, prev_cpu, runnable_at, enabled | `phaseA bp4` (`task: SimTask{pid=1 ...}`) |
| `cgroup_bw::CgroupBandwidthState` | `BWState{quota=Xms period=Yms remain=Zms throttled=BOOL period_start=Tms throttled_pids=N}` | `cgroup_bw.rs:78` body — release-build optimization sometimes hides `self` (see Limitations) |
| `cgroup_bw::BandwidthManager` | `BWMgr{states.children=N}` | `phaseA bp2` (`self: BWMgr{states.children=1}`) |
| `dsq::Dsq` | `Dsq{mode=FIFO\|PRIQ\|Empty fifo=… vtime=… inserts=N}` | exercised when `Dsq::insert_*` fires (FIFO insert is rare in this Bug-1 trace) |

## Custom command: `bug1_diagnose`

```
(lldb) bug1_diagnose
== bug1_diagnose ==
frame: scx_simulator::safe::engine::Simulator<S>::check_watchdog
could not reach SimState via {self,s,guard,fields}.sim — select a frame
that holds it (e.g. process_event_inner).
```

Walks the current frame for a `SimState` (or `SimulatorState`) reachable
via common variable names. When invoked from a frame that holds the
guard locally (`process_event_inner` mid-callback, etc.), it dumps:
- key SimulatorState fields (`clock`, `current_cpu`)
- `tasks`, `bw_manager`, `cgroup_registry` summaries

When SimState is NOT reachable (e.g., from `check_watchdog` whose only
arg is `&HashMap<Pid, SimTask>`), it prints a clear advisory pointing
the user at a frame that does carry the state.

## Quick usage

From the sched-test repo root with the worktree in place:

```bash
# Run the worked example end-to-end:
./scx-sim/lldb_debug/worked_example.sh

# Or wire it up by hand:
lldb -b \
  -o "command source scx-sim/lldb_debug/init.lldb" \
  -o "command script import scx-sim/lldb_debug/lldb_lavd_formatters.py" \
  -o "breakpoint set --file engine.rs --line 1222" \
  -o "breakpoint command add 1 -F lldb_lavd_formatters.print_once_and_disable" \
  -o "run" \
  -o "quit" \
  -- target/release/scxsim --no-disable-aslr run \
     crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json \
     --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
     --watchdog 80ms -s lavd --cpus 4 --duration 500ms
```

Inside an interactive tmux+lldb session, just import the formatters
and use `frame variable` normally — the registered summaries take
effect automatically.

## Worked example: what the transcripts prove

`phaseA.transcript.txt` (136 lines) shows four strategic Rust-side
breakpoints firing under the Bug-1 reproducer. The four hits exercise
the formatters as follows:

```
=== HIT bp1 at CgroupBandwidthState::charge (cgroup_bw.rs:78) ===
  delta_ns: 3996510

=== HIT bp2 at BandwidthManager::charge (cgroup_bw.rs:219) ===
  self: BWMgr{states.children=1}
  cgid: cgid=2
  delta_ns: 3996510

=== HIT bp3 at hashbrown::HashMap::get_mut (inlined from DsqManager::insert_vtime) ===
  k: dsq=user(8192)

=== HIT bp4 at Simulator::check_watchdog (engine.rs:1222) ===
  task: SimTask{pid=1 name=None TaskState::$variants$ prev_cpu=0
                runnable_at=None enabled=false}
```

Four formatters demonstrably fire end-to-end:
**BandwidthManager**, **CgroupId**, **DsqId**, **SimTask**.
The remaining six (Pid, CpuId, Vtime, CgroupBandwidthState,
TaskState, Dsq) are exercised in different trace conditions:
Pid/CpuId fire as nested children of SimTask; Vtime and Dsq fire when
a vtime DSQ is materialized (the Bug-1 path inserts into the global
DSQ via inlined HashMap access, so Dsq::insert_vtime itself is
skipped); CgroupBandwidthState rendering is gated on lldb's ability
to dereference `self` (see Limitations).

`phaseB.transcript.txt` (62 lines) shows `bug1_diagnose` invoked at
the `check_watchdog` stop. As documented above, it correctly reports
that SimState is not reachable from that frame's parameters.

## Known limitations / gotchas

1. **Release-build optimization hides some `self` references.** At
   `CgroupBandwidthState::charge`'s function entry, lldb shows
   `self: ?` because the pointer hasn't been spilled to a stable
   location yet. Workaround: rebuild with `CARGO_PROFILE_RELEASE_DEBUG=2`
   AND a less-aggressive opt level, OR set the breakpoint a few lines
   into the function body where `self` is used (lldb resolves it via
   register tracking after first use).
2. **Rust enum discriminant rendering varies by lldb version.** Our
   `summary_taskstate` walks the enum's child variant; on the
   Meta-built lldb 23.x in use here, the child sometimes appears as
   `$variants$` instead of the resolved variant name. The full enum
   value is still available via `frame variable -d 1 task.state`.
3. **String summaries return None in release builds.** `SimTask.name`
   shows as `name=None` in our transcript because lldb's stock
   `alloc::string::String` summary requires uninstrumented heap
   layout that release builds may strip. Fall back to
   `expression -- task.name.as_str()`.
4. **HashMap counts may be unavailable.** When lldb can't summarize
   a `HashMap<K,V>` (typical with mangled generics), `BandwidthManager`
   falls back to `states.children=N` instead of `states={k:v, ...}`.
5. **`bug1_diagnose` requires a frame holding the SimState guard.**
   Most `Simulator::*` methods do, but standalone helpers like
   `check_watchdog` (which take only `&HashMap<Pid, SimTask>`) do
   not. Move up the stack with `frame select N` before invoking it.
6. **`breakpoint command add -o` only takes the LAST `-o`.** Use
   `print_and_continue` or `print_once_and_disable` Python callbacks
   (provided in `lldb_lavd_formatters.py`) to chain multiple actions
   at a stop in batch mode. Inline `#` comments inside `lldb -s
   SCRIPT.lldb` files break argument parsing; keep comments on their
   own lines.
7. **Inlining shifts breakpoint locations.** A breakpoint set on
   `dsq.rs:217` (DsqManager::insert_vtime) resolves to an inlined
   `hashbrown::HashMap::get_mut` site instead — the surrounding
   variables (`dsq_id`, `pid`, `vtime`) may not be in scope. The
   transcript shows `k: dsq=user(8192)` instead of `dsq_id`. Pick
   breakpoints on the OUTER function body (e.g. `dsq.rs:218`) when
   that matters.

## What's NOT in this directory

Per the task scope (scxsim is pure userspace, focused 5-10 helpers):

- No drgn helpers, no BPF struct introspection, no kernel-side
  formatters. scxsim's wrapper.c structs (`task_struct`, `cgroup`,
  `task_ctx`) are intentionally NOT formatted here — they belong in
  the LAVD wrapper-side debug, not the simulator-side debug.
- No gdb pretty-printers. gdb 9.1 on this host crashes on the scxsim
  binary; lldb is the only viable option (per the parent
  `experiments/agent_debugger_setup_20260512/README.md`).
- No `Simulator` or `SimulatorState` whole-struct summary. They're
  large enough that a free-form summary would be useless; use
  `bug1_diagnose` from a frame that holds the state instead.
