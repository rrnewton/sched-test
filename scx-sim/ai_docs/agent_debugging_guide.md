# Agent-Driven Debugging Guide for scx-sim

This document describes how an AI agent (such as Claude Code) can use debuggers
(lldb, gdb, rr) in its inner loop when debugging scheduler behavior in the
simulator. It covers what works today, what does not, and recommends a practical
workflow.

## 1. Overview of Approaches

There are four main approaches to agent-driven debugging of the simulator:

| Approach | Debugger | Time Travel | Agent Scriptable | Maturity |
|----------|----------|-------------|------------------|----------|
| **lldb batch mode + --wait-debugger** | lldb | No (re-run) | Yes | Production-ready |
| **rr record/replay + gdb batch** | gdb via rr | Yes (reverse-continue) | Yes | Production-ready |
| **Deterministic re-run** | Any | Implicit (re-run same seed) | Yes | Production-ready |
| **ChatDBG** | gdb/lldb | No | Partial (LLM-driven) | Research prototype |
| **MCP debugger servers** | Various | Varies | Yes (MCP protocol) | Experimental |

**Recommendation**: Use **lldb batch mode with `--wait-debugger`** for scheduler
`.so` debugging, and **rr with gdb batch mode** when you need time-travel
debugging of the simulator engine itself.

## 2. rr Record/Replay

### 2.1 How It Works

rr records a complete execution trace, then replays it deterministically under
gdb. This enables reverse-continue, reverse-step, and other time-travel
debugging commands.

### 2.2 Recording

```bash
~/bin/rr record -- ./target/release/scxsim run \
    crates/scx_simulator/workloads/simple_wake.json \
    --scheduler simple --seed 42
```

The recording is stored in `~/.local/share/rr/scxsim-N/` (N increments).

### 2.3 Replaying in Batch Mode

The key to agent integration: use `-batch` with `-ex` commands:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "set breakpoint pending on" \
    -ex "break simple_enqueue" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "info args" \
    -ex "print p->pid" \
    -ex "quit"
```

**Critical**: Use `set breakpoint pending on` for scheduler `.so` functions.
The scheduler is loaded via `dlopen()` at runtime, so GDB does not see the
symbols at startup. With pending breakpoints, GDB resolves them when the
shared library loads.

### 2.4 Reverse Debugging

After hitting a breakpoint, you can reverse-continue to go back in time:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "set breakpoint pending on" \
    -ex "break simple_enqueue" \
    -ex "continue" \
    -ex "continue" \
    -ex "continue" \
    -ex "print \"=== AT 3RD ENQUEUE ===\"" \
    -ex "info args" \
    -ex "print p->pid" \
    -ex "reverse-continue" \
    -ex "print \"=== REVERSED TO 2ND ENQUEUE ===\"" \
    -ex "info args" \
    -ex "print p->pid" \
    -ex "quit"
```

This was tested and confirmed working. The agent can navigate forward and
backward through scheduler callback invocations.

### 2.5 Simulator Engine Breakpoints

For simulator-side breakpoints (Rust code), use `file:line` syntax:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "break ffi.rs:972" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "info args" \
    -ex "up" \
    -ex "info locals" \
    -ex "quit"
```

The `info functions <regex>` command helps discover Rust function names:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "info functions tick" \
    -ex "quit"
```

### 2.6 GDB/MI (Machine Interface) Mode

rr supports GDB's Machine Interface, which produces structured output:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -i=mi -batch \
    -ex "break ffi.rs:972" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "quit"
```

MI output is machine-parseable JSON-like format. This could be useful for
agents that want to extract structured data from debugger output.

### 2.7 GDB Python Scripting via rr

GDB Python can be used inline via `-ex "python ..."` commands:

```bash
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "break ffi.rs:972" \
    -ex "continue" \
    -ex "python
import gdb
frame = gdb.selected_frame()
print(f'FRAME: {frame.name()}')
" \
    -ex "quit"
```

**Limitation**: Script files via `-x` do not work with rr because the script
runs before rr has attached to the replayed process. Always use `-ex` commands
instead.

### 2.8 rr Limitations for Agent Use

1. **GDB only**: rr requires GDB -- it does not work with lldb.
2. **Recording overhead**: rr adds ~1.5x recording overhead (minimal for
   short simulations).
3. **No `-x` scripts**: Script files error out; must use `-ex` commands.
4. **Batch mode is one-shot**: Each `-batch` run executes all commands and
   exits. There is no interactive back-and-forth unless you use
   expect-like patterns or named pipes.
5. **Symbol issues**: Rust function names contain special characters
   (`{impl#1}`) that may confuse GDB. Use `file:line` breakpoints instead.
6. **`.gdbinit` conflicts**: The Meta system `.gdbinit` may cause GDB to
   crash. rr's `-d /usr/bin/gdb` avoids this, but direct GDB should use
   `--nx` to skip `.gdbinit`.

## 3. ChatDBG Architecture

### 3.1 What It Is

ChatDBG (by Emery Berger et al., FSE'25) integrates an LLM into gdb/lldb/pdb.
The user types `why` at the debugger prompt, and ChatDBG sends the stack trace
plus error context to an LLM, which can then issue debugger commands
autonomously to investigate the root cause.

Source location: `~/work/multi_scx/work/ChatDBG/`

### 3.2 Architecture

ChatDBG's architecture is straightforward:

1. **Debugger extension**: A Python script loaded into gdb/lldb via
   `.gdbinit`/`.lldbinit`.
   - GDB: `chatdbg_gdb.py` -- uses `gdb.Command` and `gdb.execute()`
   - LLDB: `chatdbg_lldb.py` -- uses `@lldb.command` and
     `debugger.GetCommandInterpreter().HandleCommand()`

2. **Dialog loop** (`dbg_dialog.py`):
   - Gathers context: stack trace, error message, command line, input
   - Sends initial prompt to LLM with enriched stack trace
   - LLM can call `debug(command)` function to execute debugger commands
   - LLM can call `get_code_surrounding(file, line)` to read source
   - LLM can call `find_definition(file, line, symbol)` via clangd LSP

3. **Assistant** (`assistant.py`):
   - Uses `litellm` to talk to OpenAI API (GPT-4o by default)
   - Supports function calling for debugger commands
   - Streams responses back to the user

4. **Safety**: Commands are whitelisted (`safety.py`). Only read-only
   commands are allowed by default: `bt`, `frame`, `info`, `list`, `up`,
   `down`, `print` (with restricted patterns).

### 3.3 Key Design Decisions

- **Direct debugger API, not DAP/MI**: ChatDBG uses gdb/lldb Python APIs
  directly (`gdb.execute()`, `interpreter.HandleCommand()`), not the Debug
  Adapter Protocol or Machine Interface.
- **LLM drives the debugger**: The LLM decides which commands to run via
  function calling. It is NOT the other way around.
- **No process control**: ChatDBG only works in post-mortem or stopped state.
  It cannot start/stop processes.

### 3.4 Applicability to Agent Workflow

ChatDBG's approach is instructive but not directly reusable for our agent:

- **Requires interactive debugger session**: ChatDBG runs inside an interactive
  gdb/lldb session. Our agent needs batch mode.
- **Requires OpenAI API key**: Adds external dependency and cost.
- **Valuable patterns to adopt**:
  - Enriched stack trace with source context
  - Safety-whitelisted command execution
  - Systematic variable inspection across frames
  - Using `clangd` for definition lookup

## 4. MCP Debugger Servers

### 4.1 Landscape

Several MCP servers for debuggers exist (as of early 2026):

1. **debugmcp/mcp-debugger** (github.com/debugmcp/mcp-debugger):
   LLM-driven debugger server providing step-through debugging via MCP.

2. **pansila/mcp_server_gdb** (github.com/pansila/mcp_server_gdb):
   MCP server exposing GDB capabilities.

3. **mizchi/debugger-mcp** (github.com/mizchi/debugger-mcp):
   WIP Debug Adapter Protocol (DAP) tools for MCP.

4. **KashunCheng/dap_mcp** (github.com/KashunCheng/dap_mcp):
   MCP server wrapping the Debug Adapter Protocol.

### 4.2 Assessment

These projects could not be evaluated in depth due to network restrictions
preventing access to their repositories. However, based on the search results:

- **Maturity**: All appear to be experimental/early-stage projects.
- **DAP-based approaches** (dap_mcp, debugger-mcp) are more portable but add
  a layer of abstraction over gdb/lldb.
- **GDB-specific** (mcp_server_gdb) may be more direct but limited to GDB.
- **None appear production-ready** for our use case.

### 4.3 Why We May Not Need Them

The agent (Claude Code) already has shell access via the `Bash` tool. Since
both `lldb -b` and `gdb -batch` work well for non-interactive debugging,
the agent can simply invoke these directly without needing an MCP intermediary.
An MCP server would add value only if it provided:

- Persistent debugger sessions (attach once, issue commands over time)
- Structured data extraction (parsed variables, not raw text)
- State tracking across multiple debugging steps

For now, batch mode with multiple `-ex`/`-o` commands is simpler and sufficient.

## 5. Deterministic Simulation as Time-Travel Substitute

### 5.1 The Key Insight

The simulator supports deterministic mode: running the same simulation with
the same `--seed` produces identical results. This is a form of time travel
without rr -- you can observe a bug, then re-run with breakpoints to
investigate.

### 5.2 Verified Determinism

Tested and confirmed: two runs with `--seed 42` produce byte-identical output:

```bash
RUN1=$(./target/release/scxsim run workload.json --scheduler simple --seed 42 2>&1)
RUN2=$(./target/release/scxsim run workload.json --scheduler simple --seed 42 2>&1)
# RUN1 == RUN2 : CONFIRMED
```

### 5.3 The Workflow

1. **Run 1 -- Observe**: Run the simulation and capture output. Identify
   the anomaly (wrong dispatch, unexpected idle, panic, etc.).

2. **Run 2 -- Debug**: Re-run with `--wait-debugger` and the same `--seed`.
   Attach a debugger, set breakpoints at the relevant ops callback,
   and inspect state.

```bash
# Step 1: Observe
./target/release/scxsim run workload.json --scheduler simple --seed 42 2>&1 \
    | tee /tmp/sim-output.txt

# Step 2: Re-run with debugger
./target/release/scxsim run workload.json --scheduler simple --seed 42 \
    --wait-debugger > /tmp/debug-wait.out 2>&1 &
sleep 3

# Step 3: Extract PID and attach
PID=$(grep -oP 'PID:\s+\K\d+' /tmp/debug-wait.out)
SCRIPT=$(grep -oP 'command source \K\S+' /tmp/debug-wait.out | tr -d '"')

lldb -b -p $PID \
    -o "command source $SCRIPT" \
    -o "breakpoint delete 1" \
    -o "continue" \
    -o "bt" \
    -o "frame variable" \
    -o "kill" -o "quit"
```

### 5.4 Comparison: Deterministic Re-run vs rr

| Feature | Deterministic Re-run | rr Record/Replay |
|---------|---------------------|------------------|
| Reverse execution | No (must re-run from start) | Yes (reverse-continue, reverse-step) |
| Recording overhead | None | ~1.5x |
| Debugger support | lldb AND gdb | GDB only |
| `--wait-debugger` | Yes (auto-generated scripts) | No (must set breakpoints manually) |
| Scheduler `.so` symbols | Automatically loaded | Need `set breakpoint pending on` |
| Speed for short sims | Fast (re-run takes <1s) | Fast (replay takes <1s) |
| Speed for long sims | Slow (full re-run) | Fast (can seek to any point) |
| Preemptive mode determinism | Only with `--preempt-mode e9patch` | Full determinism (records actual execution) |

**Verdict**: For short simulations (< 10 seconds), deterministic re-run with
`--wait-debugger` is more practical because it works with lldb, auto-generates
breakpoint scripts, and requires no recording step. For long simulations or
when reverse execution is needed, use rr.

## 6. Practical Agent Debugging Workflow

### 6.1 Workflow A: lldb Batch Mode with --wait-debugger (Recommended)

This is the simplest and most reliable approach for debugging scheduler ops.

```bash
# 1. Start simulation in background with --wait-debugger
./target/release/scxsim run \
    crates/scx_simulator/workloads/simple_wake.json \
    --scheduler simple --seed 42 --wait-debugger \
    > /tmp/debug-wait.out 2>&1 &
BGPID=$!
sleep 3

# 2. Extract connection info
PID=$(grep -oP 'PID:\s+\K\d+' /tmp/debug-wait.out | head -1)
SCRIPT=$(grep -oP 'command source \K\S+' /tmp/debug-wait.out | head -1 | tr -d '"')

# 3. Attach and debug
lldb -b -p $PID \
    -o "command source $SCRIPT" \
    -o "breakpoint delete 1" \
    -o "breakpoint delete 5 6 7 8" \
    -o "continue" \
    -o "bt 5" \
    -o "frame variable" \
    -o "frame select 1" \
    -o "frame variable" \
    -o "kill" -o "quit"

# 4. Cleanup
kill $BGPID 2>/dev/null; wait $BGPID 2>/dev/null
```

**What the auto-generated script does**:
- Configures SIGFPE handling (for BPF div-by-zero semantics)
- Sets `step-avoid-libraries` so stepping stays in scheduler code
- Sets breakpoints on all ops callbacks (init, select_cpu, enqueue,
  dispatch, running, stopping, enable, exit)

**Agent tips**:
- Delete breakpoints you do not need to avoid stopping at every callback
- Use `breakpoint modify N --condition "p->pid == 2"` for conditional breaks
- Use `expression` to evaluate complex expressions
- The first `continue` always hits `simple_init`

### 6.2 Workflow B: rr Record/Replay with Reverse Debugging

Use when you need to go backward through execution.

```bash
# 1. Record
~/bin/rr record -- ./target/release/scxsim run \
    crates/scx_simulator/workloads/simple_wake.json \
    --scheduler simple --seed 42

# 2. Replay with reverse debugging
~/bin/rr replay -d /usr/bin/gdb -- -batch \
    -ex "set breakpoint pending on" \
    -ex "break simple_enqueue" \
    -ex "continue" \
    -ex "continue" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "info args" \
    -ex "print p->pid" \
    -ex "reverse-continue" \
    -ex "bt 3" \
    -ex "info args" \
    -ex "quit"
```

**Agent tips**:
- Always use `-d /usr/bin/gdb` to specify the GDB binary (avoids
  system GDB `.gdbinit` issues)
- Always use `set breakpoint pending on` for scheduler function breakpoints
- Use `info functions <regex>` to discover Rust symbol names
- Use `file:line` for Rust code breakpoints (avoids mangled name issues)
- For simulator engine breakpoints: `break engine.rs:1704`
- For scheduler ops breakpoints: `break simple_enqueue` (C function names)

### 6.3 Workflow C: GDB with --wait-debugger (Alternative to lldb)

If you need GDB features (like Python scripting) but want --wait-debugger:

```bash
./target/release/scxsim run workload.json --scheduler simple --seed 42 \
    --wait-debugger > /tmp/debug-wait.out 2>&1 &
sleep 3

PID=$(grep -oP 'PID:\s+\K\d+' /tmp/debug-wait.out | head -1)
GDB_SCRIPT=$(grep -oP 'gdb -p \d+ -x \K\S+' /tmp/debug-wait.out | head -1)

/usr/bin/gdb --nx -batch -p $PID \
    -x "$GDB_SCRIPT" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "info args" \
    -ex "info locals" \
    -ex "kill" -ex "quit"
```

**Critical**: Use `--nx` to skip `.gdbinit` (the Meta system `.gdbinit`
contains extensions that crash GDB 9.1 when attaching to this binary).

### 6.4 What the Agent Can Extract

From a single batch debugging session, the agent can extract:

| Information | lldb command | gdb command |
|-------------|-------------|-------------|
| Backtrace | `bt 10` | `bt 10` |
| Function arguments | `frame variable` | `info args` |
| Local variables | `frame variable` | `info locals` |
| Specific variable | `expression var_name` | `print var_name` |
| Struct field | `expression p->pid` | `print p->pid` |
| Navigate frames | `frame select N` | `frame N` |
| Source context | `source list` | `list` |
| Memory | `memory read addr` | `x/10x addr` |
| Registers | `register read` | `info registers` |
| All breakpoints | `breakpoint list` | `info breakpoints` |
| Type info | `type lookup TypeName` | `ptype TypeName` |

### 6.5 Limitations

1. **Batch mode is one-shot**: The agent cannot have a multi-turn conversation
   with the debugger. Each `lldb -b` or `gdb -batch` invocation runs all
   commands and exits. To ask follow-up questions, the agent must re-run
   with different commands.

2. **Re-running is cheap**: For the `simple_wake.json` workload, the entire
   simulation takes ~100ms. Re-running with `--wait-debugger` and attaching
   takes ~3 seconds total. This is fast enough for interactive agent use.

3. **Optimized-out variables**: Release builds with debuginfo still optimize
   aggressively. Some variables show `<optimized out>` or `<unavailable>`.
   The scheduler `.so` (compiled from C with `-g`) has better variable
   visibility than the Rust simulator code.

4. **No conditional re-entry**: If the agent wants to "stop at the 47th
   enqueue call", it must either:
   - Use `ignore N 46` (GDB) to skip the first 46 hits
   - Use a conditional breakpoint: `break simple_enqueue if p->pid == 3`
   - Use rr with multiple `continue` commands

## 7. Recommendations

### 7.1 What to Use Today

1. **Primary workflow**: lldb batch mode with `--wait-debugger`.
   - Best for: debugging scheduler ops callbacks (C code in `.so`)
   - Auto-generated breakpoint scripts make this turnkey
   - Works with lldb (the system debugger on this platform)

2. **When you need reverse debugging**: rr record + gdb batch replay.
   - Best for: "how did we get into this state?" questions
   - Best for: debugging simulator engine (Rust code)
   - Enables reverse-continue to trace back through execution

3. **For quick inspection without debugger**: deterministic re-run with
   tracing. Add `RUST_LOG=debug` or use the simulator's built-in trace
   events to understand execution flow before reaching for a debugger.

### 7.2 What to Build Next

1. **Agent debugging helper script**: A shell script or Rust binary that
   wraps the common pattern: start with `--wait-debugger`, extract PID,
   attach debugger, run commands, capture output. This would reduce the
   boilerplate in each agent debugging session to a single command like:
   ```bash
   ./debug-sim.sh --workload simple_wake.json --break simple_enqueue \
       --commands "bt 5" "info args" "print p->pid"
   ```

2. **Structured output mode**: Extend the simulator to dump scheduler state
   (current task on each CPU, DSQ contents, vtime values) in JSON format
   at specified simulation ticks. This would let the agent inspect state
   without a debugger for many common cases.

3. **MCP debugger server** (longer term): If the agent needs persistent
   debugger sessions (attach once, issue commands over time), wrapping
   lldb or gdb in an MCP server would be valuable. The ChatDBG architecture
   shows how to do this (use the debugger's Python API to execute commands
   and return results). Key features:
   - `debug_attach(pid)` -- attach to running process
   - `debug_command(cmd)` -- execute debugger command, return output
   - `debug_breakpoint(location)` -- set breakpoint
   - `debug_continue()` -- resume execution
   - `debug_backtrace()` -- get structured backtrace

4. **rr checkpoint navigation**: rr supports checkpoints (`checkpoint`,
   `restart N`). An agent could set checkpoints at key simulation points
   and navigate between them efficiently, avoiding the need to replay
   from the beginning each time.

### 7.3 Anti-Patterns to Avoid

1. **Do not use interactive debugger modes**: The agent cannot drive an
   interactive TTY session reliably. Always use batch/script mode.

2. **Do not use `.gdbinit`**: The system `.gdbinit` has Meta-specific
   extensions that crash GDB. Always use `--nx` with GDB.

3. **Do not use `-x` script files with rr**: They execute before rr
   attaches to the process. Use `-ex` commands instead.

4. **Do not rely on Rust mangled names**: Use `file:line` breakpoints
   for Rust code. The demangled names with `{impl#N}` syntax are
   fragile across compiler versions.

5. **Do not skip `set breakpoint pending on` with rr**: Without it,
   breakpoints on scheduler `.so` functions silently fail because the
   shared library is not loaded at GDB startup.
