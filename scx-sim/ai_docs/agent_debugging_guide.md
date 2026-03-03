# Agent-Driven Debugging Guide for scx-sim

This document describes how an AI agent (such as Claude Code) can use debuggers
(gdb, lldb, rr) to debug scheduler behavior in the simulator. It covers three
approaches -- interactive debugging via tmux, Python pty scripting, and batch
mode -- with tmux being the recommended primary approach.

## 1. Approach Comparison

| Approach | Interactivity | User Can Observe | Works with rr | Best For |
|----------|---------------|------------------|---------------|----------|
| **tmux (Recommended)** | Full multi-step, decisions across Bash calls | Yes (`tmux attach`) | Yes | Interactive multi-step debugging |
| **Python pty** | Multi-step within single Bash call | No (runs in subprocess) | Yes | Complex automated scripts in one invocation |
| **Batch mode** | None (one-shot) | No | Yes | Quick one-shot queries when you know exactly what to inspect |

**Recommendation**: Use **tmux** for all interactive debugging. It allows the
agent to send commands, read output, make decisions, and send follow-up commands
across separate Bash tool calls. The user can attach to the tmux session at any
time to observe or take over. Fall back to batch mode only for quick one-shot
inspections where you know all the commands upfront.

## 2. Interactive Debugging via tmux (Recommended)

The key insight: `tmux` provides a persistent terminal session that survives
across separate Bash tool calls. The agent can start a debugger in a tmux
session, send commands via `tmux send-keys`, read output via
`tmux capture-pane`, make decisions based on the output, and send follow-up
commands -- all across independent Bash invocations.

### 2.1 Starting a Debug Session

```bash
# Start scxsim with --wait-debugger
./target/release/scxsim run <workload> --scheduler <name> --wait-debugger > /tmp/debug.out 2>&1 &
sleep 2
PID=$(grep -oP 'PID:\s+\K\d+' /tmp/debug.out | head -1)
SCRIPT=$(grep -oP 'command source \K[^"]+' /tmp/debug.out)

# Start gdb in a tmux session
tmux new-session -d -s debug "gdb -p $PID -x $SCRIPT"
```

For lldb instead of gdb:

```bash
SCRIPT=$(grep -oP 'command source \K\S+' /tmp/debug.out | head -1 | tr -d '"')
tmux new-session -d -s debug "lldb -p $PID -o 'command source $SCRIPT'"
```

### 2.2 Sending Commands

```bash
tmux send-keys -t debug "continue" Enter
sleep 2
tmux capture-pane -t debug -p  # Read output
```

Always include a `sleep` after `send-keys` to give the debugger time to
execute the command and produce output before capturing.

### 2.3 Reading Output

```bash
tmux capture-pane -t debug -p              # Current screen
tmux capture-pane -t debug -p -S -100      # Last 100 lines of scrollback
tmux capture-pane -t debug -p > /tmp/gdb-output.txt  # Save to file for parsing
```

The `-S -100` flag captures the last 100 lines of scrollback buffer, which is
useful when debugger output exceeds one screenful.

### 2.4 Making Decisions Based on Output

The agent can parse output from one command to decide what to do next. Each
step is a separate Bash tool call, allowing the agent to reason between steps.

```bash
# Parse output from previous command
VAR=$(tmux capture-pane -t debug -p | grep "some_field" | awk '{print $3}')
# Decide next action based on the value
if [ "$VAR" -gt 100 ]; then
    tmux send-keys -t debug "print detailed_struct" Enter
else
    tmux send-keys -t debug "continue" Enter
fi
```

### 2.5 User Can Observe or Take Over

The user can attach to the tmux session at any time to watch the agent debug,
or take over interactive control:

```bash
# User attaches to watch the agent debug (read-only or interactive)
tmux attach -t debug
# Detach with Ctrl-b d to let agent continue
```

This is a significant advantage over batch mode and Python pty approaches,
where the user has no visibility into the debugging session.

### 2.6 Cleanup

```bash
tmux send-keys -t debug "quit" Enter
sleep 1
tmux send-keys -t debug "y" Enter
tmux kill-session -t debug 2>/dev/null
```

### 2.7 Works with rr Too

The tmux approach works identically with rr replay sessions:

```bash
rr record -- ./target/release/scxsim run <workload> --scheduler <name>
GDB_SCRIPT=$(find target/release/build -name "libscx_<name>.gdb" | head -1)
tmux new-session -d -s rr-debug "rr replay -d /usr/bin/gdb -- -x $GDB_SCRIPT"
# Then same send-keys/capture-pane pattern
```

For rr, remember to send `set breakpoint pending on` early in the session
for scheduler `.so` function breakpoints.

### 2.8 Example: Full Interactive Debug Session

```bash
# === Bash call 1: Start the session ===
./target/release/scxsim run workloads/simple_wake.json \
    --scheduler simple --wait-debugger > /tmp/debug.out 2>&1 &
sleep 2
PID=$(grep -oP 'PID:\s+\K\d+' /tmp/debug.out | head -1)
SCRIPT=$(grep -oP 'gdb -p \d+ -x \K\S+' /tmp/debug.out | head -1)
tmux new-session -d -s debug "/usr/bin/gdb --nx -p $PID -x $SCRIPT"
sleep 3
tmux capture-pane -t debug -p  # Verify attached

# === Bash call 2: Continue to first breakpoint ===
tmux send-keys -t debug "continue" Enter
sleep 2
tmux capture-pane -t debug -p  # See which breakpoint was hit

# === Bash call 3: Inspect state (agent decides based on previous output) ===
tmux send-keys -t debug "info args" Enter
sleep 1
tmux capture-pane -t debug -p

# === Bash call 4: Set conditional breakpoint and continue ===
tmux send-keys -t debug "break simple_enqueue if p->pid == 2" Enter
sleep 1
tmux send-keys -t debug "continue" Enter
sleep 2
tmux capture-pane -t debug -p

# === Bash call 5: Cleanup ===
tmux send-keys -t debug "quit" Enter
sleep 1
tmux send-keys -t debug "y" Enter
tmux kill-session -t debug 2>/dev/null
kill $(grep -oP 'PID:\s+\K\d+' /tmp/debug.out | head -1) 2>/dev/null
```

## 3. Python pty Approach (Alternative)

For complex automated debugging scripts that need to run in a single Bash
invocation, a Python script using `pty` can drive gdb interactively:

```python
import subprocess, os, pty, select, time

master_fd, slave_fd = pty.openpty()
proc = subprocess.Popen(
    ['gdb', '-q', '-p', str(pid)],
    stdin=slave_fd, stdout=slave_fd, stderr=slave_fd
)
os.close(slave_fd)

def read_until_prompt(timeout=5):
    """Read gdb output until we see the (gdb) prompt."""
    output = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        r, _, _ = select.select([master_fd], [], [], 0.1)
        if r:
            data = os.read(master_fd, 4096).decode('utf-8', errors='replace')
            output.append(data)
            if '(gdb)' in data:
                break
    return ''.join(output)

def send_command(cmd):
    """Send a command to gdb and return output."""
    os.write(master_fd, (cmd + '\n').encode())
    return read_until_prompt()

# Usage
read_until_prompt()  # Wait for initial prompt
send_command('break simple_enqueue')
send_command('continue')
output = send_command('info args')
# Parse output and make decisions...
send_command('quit')
```

**Trade-offs**: This approach allows complex multi-step logic within a single
Bash call, but the user cannot observe the session, and all logic must be
encoded upfront in the Python script rather than decided interactively by the
agent across multiple Bash calls.

## 4. Quick One-Shot Inspection (Batch Mode)

Batch mode is useful when you know exactly what commands to run upfront and
do not need interactive decision-making.

### 4.1 lldb Batch Mode with --wait-debugger

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

### 4.2 gdb Batch Mode with --wait-debugger

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

### 4.3 rr Batch Mode

```bash
# 1. Record
rr record -- ./target/release/scxsim run \
    crates/scx_simulator/workloads/simple_wake.json \
    --scheduler simple --seed 42

# 2. Replay with batch commands
rr replay -d /usr/bin/gdb -- -batch \
    -ex "set breakpoint pending on" \
    -ex "break simple_enqueue" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "info args" \
    -ex "print p->pid" \
    -ex "quit"
```

### 4.4 rr Batch Mode with Reverse Debugging

```bash
rr replay -d /usr/bin/gdb -- -batch \
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

### 4.5 Batch Mode Limitations

Batch mode is **one-shot**: each invocation runs all commands and exits. The
agent cannot make decisions based on intermediate output. To ask follow-up
questions, the agent must re-run with different commands. For interactive
multi-step debugging, use tmux instead (Section 2).

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
   Attach a debugger (via tmux or batch mode) and inspect state.

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

## 6. rr-Specific Details

### 6.1 Recording

```bash
rr record -- ./target/release/scxsim run \
    crates/scx_simulator/workloads/simple_wake.json \
    --scheduler simple --seed 42
```

The recording is stored in `~/.local/share/rr/scxsim-N/` (N increments).

### 6.2 GDB/MI (Machine Interface) Mode

rr supports GDB's Machine Interface, which produces structured output:

```bash
rr replay -d /usr/bin/gdb -- -i=mi -batch \
    -ex "break ffi.rs:972" \
    -ex "continue" \
    -ex "bt 5" \
    -ex "quit"
```

MI output is machine-parseable JSON-like format. This could be useful for
agents that want to extract structured data from debugger output.

### 6.3 GDB Python Scripting via rr

GDB Python can be used inline via `-ex "python ..."` commands:

```bash
rr replay -d /usr/bin/gdb -- -batch \
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
instead (or use tmux with rr, where `-x` works because the session is
interactive).

### 6.4 Simulator Engine Breakpoints

For simulator-side breakpoints (Rust code), use `file:line` syntax:

```bash
rr replay -d /usr/bin/gdb -- -batch \
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
rr replay -d /usr/bin/gdb -- -batch \
    -ex "info functions tick" \
    -ex "quit"
```

### 6.5 rr Limitations

1. **GDB only**: rr requires GDB -- it does not work with lldb.
2. **Recording overhead**: rr adds ~1.5x recording overhead (minimal for
   short simulations).
3. **No `-x` scripts in batch mode**: Script files error out in batch mode;
   must use `-ex` commands. (In tmux interactive mode, `-x` works fine.)
4. **Symbol issues**: Rust function names contain special characters
   (`{impl#1}`) that may confuse GDB. Use `file:line` breakpoints instead.
5. **`.gdbinit` conflicts**: The Meta system `.gdbinit` may cause GDB to
   crash. rr's `-d /usr/bin/gdb` avoids this, but direct GDB should use
   `--nx` to skip `.gdbinit`.

## 7. ChatDBG Architecture (Reference)

### 7.1 What It Is

ChatDBG (by Emery Berger et al., FSE'25) integrates an LLM into gdb/lldb/pdb.
The user types `why` at the debugger prompt, and ChatDBG sends the stack trace
plus error context to an LLM, which can then issue debugger commands
autonomously to investigate the root cause.

Source location: `~/work/multi_scx/work/ChatDBG/`

### 7.2 Key Design Decisions

- **Direct debugger API, not DAP/MI**: ChatDBG uses gdb/lldb Python APIs
  directly (`gdb.execute()`, `interpreter.HandleCommand()`), not the Debug
  Adapter Protocol or Machine Interface.
- **LLM drives the debugger**: The LLM decides which commands to run via
  function calling. It is NOT the other way around.
- **No process control**: ChatDBG only works in post-mortem or stopped state.
  It cannot start/stop processes.

### 7.3 Applicability to Agent Workflow

ChatDBG's approach is instructive but not directly reusable for our agent:

- **Requires interactive debugger session**: ChatDBG runs inside an interactive
  gdb/lldb session. With tmux, our agent can now drive interactive sessions too.
- **Requires OpenAI API key**: Adds external dependency and cost.
- **Valuable patterns to adopt**:
  - Enriched stack trace with source context
  - Safety-whitelisted command execution
  - Systematic variable inspection across frames
  - Using `clangd` for definition lookup

## 8. What the Agent Can Extract

From a debugging session (tmux or batch), the agent can extract:

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

## 9. Auto-Generated Debugger Scripts

The `--wait-debugger` flag generates scripts for both lldb and gdb that:

- Configure SIGFPE handling (for BPF div-by-zero semantics)
- Set `step-avoid-libraries` / `skip` so stepping stays in scheduler code
- Set breakpoints on all ops callbacks (init, select_cpu, enqueue,
  dispatch, running, stopping, enable, exit)
- Set `breakpoint pending on` (gdb) for scheduler `.so` symbols

**Agent tips**:
- Delete breakpoints you do not need to avoid stopping at every callback
- Use conditional breakpoints: `break simple_enqueue if p->pid == 2`
  (gdb) or `breakpoint modify N --condition "p->pid == 2"` (lldb)
- The first `continue` after attaching always hits the scheduler `init` function

## 10. Anti-Patterns to Avoid

1. **Do not use `.gdbinit`**: The system `.gdbinit` has Meta-specific
   extensions that crash GDB. Always use `--nx` with GDB.

2. **Do not use `-x` script files with rr in batch mode**: They execute
   before rr attaches to the process. Use `-ex` commands instead.

3. **Do not rely on Rust mangled names**: Use `file:line` breakpoints
   for Rust code. The demangled names with `{impl#N}` syntax are
   fragile across compiler versions.

4. **Do not skip `set breakpoint pending on` with rr/gdb**: Without it,
   breakpoints on scheduler `.so` functions silently fail because the
   shared library is not loaded at GDB startup.

5. **Do not forget `sleep` after `tmux send-keys`**: The debugger needs
   time to execute the command before `capture-pane` will show the result.
