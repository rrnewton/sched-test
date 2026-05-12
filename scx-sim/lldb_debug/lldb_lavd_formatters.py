# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# lldb data formatters for scxsim's userspace Rust types.
#
# Focused on the LAVD Bug-1 (cgroup-bandwidth runnable-task stall) path:
# the structures an agent inspects to answer "is this task runnable?",
# "is this cgroup throttled?", "what's on each DSQ?".
#
# Load once per session:
#   (lldb) command script import scx-sim/lldb_debug/lldb_lavd_formatters.py
# or via the bundled init script:
#   (lldb) command source scx-sim/lldb_debug/init.lldb
#
# After loading, `frame variable` and `expression` automatically render
# the registered Rust types as one-liners. The custom command
# `bug1_diagnose` walks the live SimState (when reachable) and prints
# a structured diagnostic.
#
# Type matching uses regex so the formatters match both fully-qualified
# (`scx_simulator::safe::types::Pid`) and re-exported
# (`scx_simulator::types::Pid`) paths.

from __future__ import annotations

import lldb  # type: ignore[import-not-found]


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _child_value(valobj, name_or_index, signed=False):
    """Get a child SBValue by name (str) or index (int)."""
    if isinstance(name_or_index, int):
        c = valobj.GetChildAtIndex(name_or_index)
    else:
        c = valobj.GetChildMemberWithName(name_or_index)
    if not c.IsValid():
        return None
    return c.GetValueAsSigned() if signed else c.GetValueAsUnsigned()


def _fmt_ns(ns: int) -> str:
    """Format a TimeNs (nanoseconds) with a human-readable suffix."""
    if ns is None:
        return "None"
    if ns == 0:
        return "0"
    if ns < 1_000:
        return f"{ns}ns"
    if ns < 1_000_000:
        return f"{ns / 1_000:.1f}us"
    if ns < 1_000_000_000:
        return f"{ns / 1_000_000:.2f}ms"
    return f"{ns / 1_000_000_000:.3f}s"


# ---------------------------------------------------------------------------
# Newtype wrappers
# ---------------------------------------------------------------------------

def summary_pid(valobj, internal_dict):
    v = _child_value(valobj, 0, signed=True)
    return "pid=None" if v is None else f"pid={v}"


def summary_cpuid(valobj, internal_dict):
    v = _child_value(valobj, 0)
    return "cpu=None" if v is None else f"cpu={v}"


def summary_vtime(valobj, internal_dict):
    v = _child_value(valobj, 0)
    return "vtime=None" if v is None else f"vtime={v}"


def summary_cgroupid(valobj, internal_dict):
    v = _child_value(valobj, 0)
    if v is None:
        return "cgid=None"
    if v == 1:
        return "cgid=1(ROOT)"
    return f"cgid={v}"


def summary_dsqid(valobj, internal_dict):
    v = _child_value(valobj, 0)
    if v is None:
        return "dsq=None"
    FLAG_BUILTIN = 1 << 63
    LOCAL_ON_MASK = 0xC000000000000000
    LOCAL_CPU_MASK = 0x00000000FFFFFFFF
    if v == FLAG_BUILTIN | 1:
        return "dsq=GLOBAL"
    if v == FLAG_BUILTIN | 2:
        return "dsq=LOCAL"
    if (v & LOCAL_ON_MASK) == LOCAL_ON_MASK:
        return f"dsq=LOCAL_ON(cpu={v & LOCAL_CPU_MASK})"
    if v & FLAG_BUILTIN:
        return f"dsq=BUILTIN(0x{v:x})"
    return f"dsq=user({v})"


# ---------------------------------------------------------------------------
# TaskState enum (Sleeping / Runnable / Running{cpu} / Exited)
# ---------------------------------------------------------------------------

def summary_taskstate(valobj, internal_dict):
    # lldb's Rust enum support exposes a single child: the active variant.
    # Walk the tree for whatever variant is live.
    if valobj.GetNumChildren() == 0:
        return str(valobj.GetValue() or "?")
    # First child is the variant container.
    variant = valobj.GetChildAtIndex(0)
    name = variant.GetName() or variant.GetType().GetName()
    # Strip enum-prefix so we get just "Runnable" not "$discr$:Runnable".
    if "::" in name:
        name = name.rsplit("::", 1)[-1]
    # Running{cpu: CpuId} carries data; surface it.
    if "Running" in name:
        cpu_field = variant.GetChildMemberWithName("cpu")
        if cpu_field.IsValid():
            inner = _child_value(cpu_field, 0)
            return f"TaskState::Running{{cpu={inner}}}"
    return f"TaskState::{name}"


# ---------------------------------------------------------------------------
# SimTask — the live simulator-side task
# ---------------------------------------------------------------------------

def summary_simtask(valobj, internal_dict):
    pid = valobj.GetChildMemberWithName("pid")
    pid_v = _child_value(pid, 0, signed=True) if pid.IsValid() else None
    name = valobj.GetChildMemberWithName("name")
    # Rust String: we just take the summary lldb already produces.
    name_s = name.GetSummary() if name.IsValid() else None
    state = valobj.GetChildMemberWithName("state")
    state_s = state.GetSummary() if state.IsValid() else "?"
    runnable_at = valobj.GetChildMemberWithName("runnable_at_ns")
    runnable_s = runnable_at.GetSummary() if runnable_at.IsValid() else "?"
    prev_cpu = valobj.GetChildMemberWithName("prev_cpu")
    prev_cpu_s = prev_cpu.GetSummary() if prev_cpu.IsValid() else "?"
    enabled = valobj.GetChildMemberWithName("enabled")
    enabled_v = enabled.GetValue() if enabled.IsValid() else "?"
    return (
        f"SimTask{{pid={pid_v} name={name_s} {state_s} "
        f"prev_{prev_cpu_s} runnable_at={runnable_s} enabled={enabled_v}}}"
    )


# ---------------------------------------------------------------------------
# Cgroup bandwidth state
# ---------------------------------------------------------------------------

def summary_cgroupbwstate(valobj, internal_dict):
    quota = _child_value(valobj, "quota_ns") or 0
    period = _child_value(valobj, "period_ns") or 0
    remain_field = valobj.GetChildMemberWithName("runtime_remaining_ns")
    remain = remain_field.GetValueAsSigned() if remain_field.IsValid() else 0
    throttled = valobj.GetChildMemberWithName("throttled")
    throttled_v = throttled.GetValue() if throttled.IsValid() else "?"
    period_start = _child_value(valobj, "period_start_ns") or 0
    pids = valobj.GetChildMemberWithName("throttled_pids")
    pids_len = pids.GetSummary() if pids.IsValid() else "?"
    return (
        f"BWState{{quota={_fmt_ns(quota)} period={_fmt_ns(period)} "
        f"remain={_fmt_ns(remain) if remain >= 0 else '-' + _fmt_ns(-remain)} "
        f"throttled={throttled_v} period_start={_fmt_ns(period_start)} "
        f"throttled_pids={pids_len}}}"
    )


def summary_bwmgr(valobj, internal_dict):
    states = valobj.GetChildMemberWithName("states")
    if not states.IsValid():
        return "BWMgr{?}"
    summary = states.GetSummary()
    if summary:
        return f"BWMgr{{states={summary}}}"
    # Fall back to direct child count when lldb's stock HashMap formatter
    # can't summarize (common in release builds with stripped generics).
    n = states.GetNumChildren()
    return f"BWMgr{{states.children={n}}}"


# ---------------------------------------------------------------------------
# DSQ
# ---------------------------------------------------------------------------

def summary_dsq(valobj, internal_dict):
    mode = valobj.GetChildMemberWithName("mode")
    mode_s = mode.GetSummary() if mode.IsValid() else "?"
    # Strip "DsqMode::" prefix if present.
    if mode_s and "::" in mode_s:
        mode_s = mode_s.rsplit("::", 1)[-1]
    fifo = valobj.GetChildMemberWithName("fifo_entries")
    fifo_s = fifo.GetSummary() if fifo.IsValid() else "?"
    vtime = valobj.GetChildMemberWithName("vtime_entries")
    vtime_s = vtime.GetSummary() if vtime.IsValid() else "?"
    counter = _child_value(valobj, "insertion_counter") or 0
    return (
        f"Dsq{{mode={mode_s} fifo={fifo_s} vtime={vtime_s} "
        f"inserts={counter}}}"
    )


# ---------------------------------------------------------------------------
# Breakpoint callback: print all locals (using the registered formatters)
# and continue. Useful in batch scripts where `breakpoint command add -o`
# can only attach a single command.
#
# Wire up with:
#   breakpoint command add N -F lldb_lavd_formatters.print_and_continue
# ---------------------------------------------------------------------------

def print_and_continue(frame, bp_loc, extra_args, internal_dict):
    """Print arguments + locals (skipping statics) and auto-continue.
    Does NOT disable the breakpoint — every hit prints."""
    bp = bp_loc.GetBreakpoint()
    print(f"=== HIT bp{bp.GetID()} at {frame.GetFunctionName()} "
          f"({frame.GetLineEntry().GetFileSpec().GetFilename()}:"
          f"{frame.GetLineEntry().GetLine()}) ===")
    # GetVariables(args, locals, statics, in_scope_only). statics=False
    # avoids dumping every vtable in scope.
    for v in frame.GetVariables(True, True, False, True):
        name = v.GetName() or "?"
        summary = v.GetSummary()
        value = v.GetValue()
        rendered = summary if summary else (value if value else "?")
        if rendered and len(rendered) > 200:
            rendered = rendered[:200] + " …"
        print(f"  {name}: {rendered}")
    return False


def print_once_and_disable(frame, bp_loc, extra_args, internal_dict):
    """Print on first hit, disable the breakpoint, then auto-continue.
    Useful when a hot function (e.g. `check_watchdog`) would otherwise
    flood the transcript."""
    bp = bp_loc.GetBreakpoint()
    print_and_continue(frame, bp_loc, extra_args, internal_dict)
    bp.SetEnabled(False)
    print(f"  [bp{bp.GetID()} disabled after first hit]")
    return False


# ---------------------------------------------------------------------------
# Custom command: bug1_diagnose
# ---------------------------------------------------------------------------

def cmd_bug1_diagnose(debugger, command, exe_ctx, result, internal_dict):
    """
    bug1_diagnose — print a focused diagnostic for the LAVD Bug-1 path.

    Looks for a `SimState` instance reachable from the current frame's
    `self` (or via `expression`) and dumps:
      - tasks runnable for too long (potential watchdog victims)
      - throttled cgroups in BandwidthManager
      - per-DSQ population

    Usage from any Rust-side breakpoint where SimState is accessible:
        (lldb) bug1_diagnose
    """
    target = exe_ctx.GetTarget()
    process = exe_ctx.GetProcess()
    thread = exe_ctx.GetThread()
    if not (target.IsValid() and process.IsValid() and thread.IsValid()):
        result.SetError("bug1_diagnose: no live process / thread")
        return

    frame = thread.GetSelectedFrame()
    if not frame.IsValid():
        result.SetError("bug1_diagnose: no selected frame")
        return

    result.AppendMessage("== bug1_diagnose ==")
    result.AppendMessage(f"frame: {frame.GetFunctionName()}")

    # Best-effort: try to find a SimState through `self` or `s`.
    expr_options = lldb.SBExpressionOptions()
    expr_options.SetIgnoreBreakpoints(True)
    expr_options.SetTimeoutInMicroSeconds(2_000_000)

    candidate_paths = ["s.sim", "self.sim", "guard.sim", "fields.sim"]
    sim = None
    for path in candidate_paths:
        v = frame.EvaluateExpression(path, expr_options)
        if v.IsValid() and v.GetError().Success():
            sim = v
            result.AppendMessage(f"reached SimulatorState via `{path}`")
            break
    if sim is None:
        result.AppendMessage(
            "could not reach SimState via {self,s,guard,fields}.sim — "
            "select a frame that holds it (e.g. process_event_inner)."
        )
        return

    # Dump a few key fields.
    for field in ("clock", "current_cpu"):
        v = sim.GetChildMemberWithName(field)
        if v.IsValid():
            result.AppendMessage(f"  {field}: {v.GetSummary() or v.GetValue()}")

    # tasks/bw_manager hang off SimState (the parent of `sim`), not sim itself.
    # Try common access paths.
    for path in ("s.tasks", "s.bw_manager", "s.cgroup_registry",
                 "self.tasks", "self.bw_manager"):
        v = frame.EvaluateExpression(path, expr_options)
        if v.IsValid() and v.GetError().Success():
            label = path.split(".", 1)[1]
            summary = v.GetSummary() or "(no summary)"
            result.AppendMessage(f"  {label}: {summary}")


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

# Each entry: (regex, formatter callable name)
_FORMATTERS = [
    (r"^scx_simulator::(safe::)?types::Pid$", "summary_pid"),
    (r"^scx_simulator::(safe::)?types::CpuId$", "summary_cpuid"),
    (r"^scx_simulator::(safe::)?types::Vtime$", "summary_vtime"),
    (r"^scx_simulator::(safe::)?types::DsqId$", "summary_dsqid"),
    (r"^scx_simulator::(safe::)?cgroup::CgroupId$", "summary_cgroupid"),
    (r"^scx_simulator::(safe::)?task::TaskState$", "summary_taskstate"),
    (r"^scx_simulator::unsafe_impl::sim_task::SimTask$", "summary_simtask"),
    (r"^scx_simulator::(safe::)?cgroup_bw::CgroupBandwidthState$",
     "summary_cgroupbwstate"),
    (r"^scx_simulator::(safe::)?cgroup_bw::BandwidthManager$", "summary_bwmgr"),
    (r"^scx_simulator::(safe::)?dsq::Dsq$", "summary_dsq"),
]


def __lldb_init_module(debugger, internal_dict):
    module = "lldb_lavd_formatters"
    for regex, fn in _FORMATTERS:
        cmd = (
            f'type summary add -F {module}.{fn} -x "{regex}" --category lavd'
        )
        debugger.HandleCommand(cmd)
    debugger.HandleCommand("type category enable lavd")
    debugger.HandleCommand(
        f'command script add -f {module}.cmd_bug1_diagnose bug1_diagnose'
    )
    print(
        f"[lldb_lavd_formatters] registered {len(_FORMATTERS)} type "
        f"summaries + command `bug1_diagnose`"
    )
