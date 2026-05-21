# Debugging with LLDB

> **Status — stub.** This recipe will document attaching lldb to a
> live scxsim process via `--wait-debugger` and using the helpers under
> [`scx-sim/lldb_debug/`](https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/lldb_debug).

Sketch:

1. Launch with `--wait-debugger`:
   ```bash
   scxsim run -s lavd --cpus 4 --duration 1s \
       --wait-debugger \
       examples/cpu_bound.json
   ```
   scxsim spins until lldb attaches and prints the attach command and
   the path to a breakpoint script written next to the scheduler `.so`.
2. In another terminal, run the attach command lldb prints.
3. Load the helpers:
   ```text
   (lldb) command script import lldb_debug/scxsim_formatters.py
   (lldb) bug1_diagnose
   ```
4. See [`scx-sim/lldb_debug/README.md`][lldb-readme] for the 10
   bundled type summaries and the `worked_example.sh` end-to-end demo
   against the Bug-1 canonical reproducer.

[lldb-readme]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/lldb_debug/README.md
