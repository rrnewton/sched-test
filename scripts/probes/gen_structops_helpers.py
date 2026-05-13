#!/usr/bin/env python3
"""
gen_structops_helpers.py — generate structops_full.bt + helpers_full.bt.

Both probe files emit ONE JSONL record per call/return, with the canonical
schema shared with scxsim's matching structops-jsonl emitter:

    {"ts_ns":<u64>,"cpu":<i32>,"pid":<i32>,
     "kind":"<structop|helper>","name":"<op>","phase":"<entry|exit>",
     "args":{...},"ret":<val|null>}

Why generated: there are 34 structop wrappers (fentry+fexit on the 7 that
return a value) and 40 BPF helpers — ~80 probe blocks. Hand-writing all of
them invites typos and drift. Editing the SCHEMA tables in this file is
the single source of truth.

Tested on devbig176, kernel 6.16.1-fbk2, bpftrace v0.25.0-be6e (May 2026).

Usage:
    ./gen_structops_helpers.py
        # writes structops_full.bt + helpers_full.bt next to this script
"""
from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Arg:
    name: str           # bpftrace arg accessor: args.<name> (fentry only)
    json_key: str       # JSON field name in the args object
    fmt: str            # printf format specifier (%d, %llu, %s, etc.)
    expr: str           # fentry-style expression: e.g. args.p->pid
    quoted: bool = False  # if True, wrap value in JSON quotes
    # kprobe-style expression (positional argN with explicit cast). If None,
    # the emitter rewrites by replacing args.<x> tokens. For complex
    # expressions (str(...), nullable derefs) you must supply this manually.
    kprobe_expr: str | None = None


@dataclass
class Op:
    name: str           # short name (for "name" field in JSON)
    sym: str            # full kernel symbol (e.g. sched_ext_ops__enqueue)
    args: list[Arg]     # entry arg projection
    ret_fmt: str | None = None   # if non-None, also emit exit probe with this fmt
    ret_expr: str = "retval"  # bare identifier on fexit/kretprobe
    # CPU expression for the JSON "cpu" header field — bpftrace `cpu`
    # builtin works inside kprobe context for structops (and inside fentry
    # for helpers).
    cpu_expr: str = "cpu"


# ---------------------------------------------------------------------------
# Structops surface — derived from struct sched_ext_ops in
# scx/scheds/vmlinux/vmlinux.h:46582-46628 + verified hookable via
# `bpftrace -l 'fentry:vmlinux:sched_ext_ops__*'` on host kernel.
#
# 34 of 37 callable ops have wrappers. The 3 missing (cgroup_set_idle,
# sub_attach, sub_detach) are filed as gaps in SURVEY.md.
#
# ATTACH MECHANISM: We use **kprobe** (not fentry) for structops because
# bpftrace v0.25's strict-mode verifier rejects unconditional `cpu` builtin
# usage and `printf` (perf_event_output) inside the fentry trampoline
# context for several structops (notably dispatch, called from the idle
# loop with NULL current). kprobe contexts are permissive enough for these.
# Helpers stay on fentry (kfunc context is verifier-clean).
#
# Cost: kprobe overhead is ~50ns vs fentry's ~10ns. Acceptable for our
# stall-bug repro use case (call rates < 1M/s on small workloads).
#
# kprobe arg accessors are positional: arg0, arg1, ... cast to the right
# type per the wrapper signature (verified via `bpftrace -lv fentry:...`).
# ---------------------------------------------------------------------------

# ---- kprobe arg helpers ----
# bpftrace kprobe accessors are positional: arg0, arg1, ... cast to type.
def kp_task_pid(idx: int, json_key: str = "task_pid") -> Arg:
    return Arg(
        f"p_arg{idx}", json_key, "%d",
        f"((struct task_struct *)arg{idx})->pid",
    )


def kp_task_comm(idx: int, json_key: str = "task_comm") -> Arg:
    return Arg(
        f"p_arg{idx}", json_key, "%s",
        f"str(((struct task_struct *)arg{idx})->comm)",
        quoted=True,
    )


def kp_int(idx: int, json_key: str, signed: bool = True) -> Arg:
    fmt = "%d" if signed else "%u"
    cast = "int32" if signed else "uint32"
    return Arg(f"i_arg{idx}", json_key, fmt, f"({cast})arg{idx}")


def kp_u64(idx: int, json_key: str) -> Arg:
    return Arg(f"u_arg{idx}", json_key, "%llu", f"(uint64)arg{idx}")


def kp_cgroup_id(idx: int, json_key: str) -> Arg:
    return Arg(
        f"cg_arg{idx}", json_key, "%llu",
        f"((struct cgroup *)arg{idx})->kn->id",
    )


def kp_task_pid_nullable(idx: int, json_key: str) -> Arg:
    """For struct task_struct *foo__nullable args."""
    return Arg(
        f"p_arg{idx}", json_key, "%d",
        f"arg{idx} != 0 ? ((struct task_struct *)arg{idx})->pid : -1",
    )


# Each Op below has args described by POSITIONAL kprobe access.
# Signatures verified by `bpftrace -lv 'fentry:vmlinux:sched_ext_ops__<op>'`.
STRUCTOPS: list[Op] = [
    # --- core scheduling path (HOT) ---
    # select_cpu(struct task_struct *p, s32 prev_cpu, u64 wake_flags) → s32
    Op("select_cpu", "sched_ext_ops__select_cpu", [
        kp_task_pid(0), kp_task_comm(0),
        kp_int(1, "prev_cpu"),
        kp_u64(2, "wake_flags"),
    ], ret_fmt="%d"),
    # enqueue(struct task_struct *p, u64 enq_flags)
    Op("enqueue", "sched_ext_ops__enqueue", [
        kp_task_pid(0), kp_task_comm(0),
        kp_u64(1, "enq_flags"),
    ]),
    # dequeue(struct task_struct *p, u64 enq_flags)  [kernel sig says enq_flags]
    Op("dequeue", "sched_ext_ops__dequeue", [
        kp_task_pid(0), kp_task_comm(0),
        kp_u64(1, "deq_flags"),
    ]),
    # dispatch(s32 prev_cpu, struct task_struct *prev__nullable)
    Op("dispatch", "sched_ext_ops__dispatch", [
        kp_int(0, "prev_cpu"),
        kp_task_pid_nullable(1, "prev_pid"),
    ]),
    # tick(struct task_struct *p)
    Op("tick", "sched_ext_ops__tick", [
        kp_task_pid(0), kp_task_comm(0),
    ]),
    # runnable(struct task_struct *p, u64 enq_flags)
    Op("runnable", "sched_ext_ops__runnable", [
        kp_task_pid(0), kp_task_comm(0),
        kp_u64(1, "enq_flags"),
    ]),
    # running(struct task_struct *p)
    Op("running", "sched_ext_ops__running", [
        kp_task_pid(0), kp_task_comm(0),
    ]),
    # stopping(struct task_struct *p, bool runnable)
    Op("stopping", "sched_ext_ops__stopping", [
        kp_task_pid(0), kp_task_comm(0),
        kp_int(1, "still_runnable"),
    ]),
    # quiescent(struct task_struct *p, u64 deq_flags)
    Op("quiescent", "sched_ext_ops__quiescent", [
        kp_task_pid(0), kp_task_comm(0),
        kp_u64(1, "deq_flags"),
    ]),
    # yield(struct task_struct *from, struct task_struct *to__nullable) → bool
    Op("yield", "sched_ext_ops__yield", [
        kp_task_pid(0, "from_pid"),
        kp_task_pid_nullable(1, "to_pid"),
    ], ret_fmt="%d"),
    # core_sched_before(struct task_struct *a, struct task_struct *b) → bool
    Op("core_sched_before", "sched_ext_ops__core_sched_before", [
        kp_task_pid(0, "a_pid"),
        kp_task_pid(1, "b_pid"),
    ], ret_fmt="%d"),
    # set_weight(struct task_struct *p, u32 weight)
    Op("set_weight", "sched_ext_ops__set_weight", [
        kp_task_pid(0),
        kp_int(1, "weight", signed=False),
    ]),
    # set_cpumask(struct task_struct *p, const struct cpumask *mask)
    Op("set_cpumask", "sched_ext_ops__set_cpumask", [
        kp_task_pid(0),
    ]),
    # update_idle(s32 cpu, bool idle)
    Op("update_idle", "sched_ext_ops__update_idle", [
        kp_int(0, "cpu_arg"),
        kp_int(1, "idle"),
    ]),
    # cpu_acquire(s32 cpu, struct scx_cpu_acquire_args *args)
    Op("cpu_acquire", "sched_ext_ops__cpu_acquire", [
        kp_int(0, "cpu_arg"),
    ]),
    # cpu_release(s32 cpu, struct scx_cpu_release_args *args)
    Op("cpu_release", "sched_ext_ops__cpu_release", [
        kp_int(0, "cpu_arg"),
    ]),

    # --- task lifecycle ---
    # init_task(struct task_struct *p, struct scx_init_task_args *args) → s32
    Op("init_task", "sched_ext_ops__init_task", [
        kp_task_pid(0), kp_task_comm(0),
    ], ret_fmt="%d"),
    # exit_task(struct task_struct *p, struct scx_exit_task_args *args)
    Op("exit_task", "sched_ext_ops__exit_task", [
        kp_task_pid(0), kp_task_comm(0),
    ]),
    Op("enable", "sched_ext_ops__enable", [
        kp_task_pid(0), kp_task_comm(0),
    ]),
    Op("disable", "sched_ext_ops__disable", [
        kp_task_pid(0), kp_task_comm(0),
    ]),

    # --- diagnostic dump ---
    # dump(struct scx_dump_ctx *ctx)
    Op("dump", "sched_ext_ops__dump", []),
    # dump_cpu(struct scx_dump_ctx *ctx, s32 cpu, bool idle)
    Op("dump_cpu", "sched_ext_ops__dump_cpu", [
        kp_int(1, "cpu_arg"),
        kp_int(2, "idle"),
    ]),
    # dump_task(struct scx_dump_ctx *ctx, struct task_struct *p)
    Op("dump_task", "sched_ext_ops__dump_task", [
        kp_task_pid(1),
    ]),

    # --- cgroup control path (HIGHLY relevant to cpu-bw-stall-bug) ---
    # cgroup_init(struct cgroup *cgrp, struct scx_cgroup_init_args *args) → s32
    Op("cgroup_init", "sched_ext_ops__cgroup_init", [
        kp_cgroup_id(0, "cgrp_id"),
    ], ret_fmt="%d"),
    Op("cgroup_exit", "sched_ext_ops__cgroup_exit", [
        kp_cgroup_id(0, "cgrp_id"),
    ]),
    # cgroup_prep_move(struct task_struct *p, struct cgroup *from, struct cgroup *to) → s32
    Op("cgroup_prep_move", "sched_ext_ops__cgroup_prep_move", [
        kp_task_pid(0),
        kp_cgroup_id(1, "from_cgid"),
        kp_cgroup_id(2, "to_cgid"),
    ], ret_fmt="%d"),
    Op("cgroup_move", "sched_ext_ops__cgroup_move", [
        kp_task_pid(0),
        kp_cgroup_id(1, "from_cgid"),
        kp_cgroup_id(2, "to_cgid"),
    ]),
    Op("cgroup_cancel_move", "sched_ext_ops__cgroup_cancel_move", [
        kp_task_pid(0),
        kp_cgroup_id(1, "from_cgid"),
        kp_cgroup_id(2, "to_cgid"),
    ]),
    # cgroup_set_weight(struct cgroup *cgrp, u32 weight)
    Op("cgroup_set_weight", "sched_ext_ops__cgroup_set_weight", [
        kp_cgroup_id(0, "cgrp_id"),
        kp_int(1, "weight", signed=False),
    ]),
    # cgroup_set_bandwidth(struct cgroup *cgrp, u64 period_us, u64 quota_us, u64 burst_us)
    Op("cgroup_set_bandwidth", "sched_ext_ops__cgroup_set_bandwidth", [
        kp_cgroup_id(0, "cgrp_id"),
        kp_u64(1, "period_us"),
        kp_u64(2, "quota_us"),
        kp_u64(3, "burst_us"),
    ]),

    # --- cpu hotplug ---
    Op("cpu_online", "sched_ext_ops__cpu_online", [
        kp_int(0, "cpu_arg"),
    ]),
    Op("cpu_offline", "sched_ext_ops__cpu_offline", [
        kp_int(0, "cpu_arg"),
    ]),

    # --- scheduler lifecycle ---
    Op("init", "sched_ext_ops__init", [], ret_fmt="%d"),
    Op("exit", "sched_ext_ops__exit", []),
]

assert len(STRUCTOPS) == 34, f"expected 34 structops, got {len(STRUCTOPS)}"


# ---------------------------------------------------------------------------
# BPF helper surface — derived from `bpftrace -l 'fentry:vmlinux:scx_bpf_*'`
# on host kernel; cross-checked against scx/scheds/include/scx/common.bpf.h.
# 40 helpers. Args projected from `bpftrace -lv` per helper.
# ---------------------------------------------------------------------------

def task_pid_arg() -> Arg:
    return Arg("p", "task_pid", "%d", "args.p->pid")

HELPERS: list[Op] = [
    # --- DSQ insert / dispatch (HOT) ---
    Op("dsq_insert", "scx_bpf_dsq_insert", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("slice", "slice", "%llu", "args.slice"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ]),
    Op("dsq_insert_vtime", "scx_bpf_dsq_insert_vtime", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("slice", "slice", "%llu", "args.slice"),
        Arg("vtime", "vtime", "%llu", "args.vtime"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ]),
    Op("dsq_move", "scx_bpf_dsq_move", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ], ret_fmt="%d"),
    Op("dsq_move_vtime", "scx_bpf_dsq_move_vtime", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ], ret_fmt="%d"),
    Op("dsq_move_to_local", "scx_bpf_dsq_move_to_local", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
    ], ret_fmt="%d"),
    Op("dsq_move_set_slice", "scx_bpf_dsq_move_set_slice", [
        Arg("slice", "slice", "%llu", "args.slice"),
    ]),
    Op("dsq_move_set_vtime", "scx_bpf_dsq_move_set_vtime", [
        Arg("vtime", "vtime", "%llu", "args.vtime"),
    ]),
    Op("consume", "scx_bpf_consume", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
    ], ret_fmt="%d"),
    Op("dispatch", "scx_bpf_dispatch", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("slice", "slice", "%llu", "args.slice"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ]),
    Op("dispatch_vtime", "scx_bpf_dispatch_vtime", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("slice", "slice", "%llu", "args.slice"),
        Arg("vtime", "vtime", "%llu", "args.vtime"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ]),
    Op("dispatch_from_dsq", "scx_bpf_dispatch_from_dsq", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ], ret_fmt="%d"),
    Op("dispatch_from_dsq_set_slice", "scx_bpf_dispatch_from_dsq_set_slice", [
        Arg("slice", "slice", "%llu", "args.slice"),
    ]),
    Op("dispatch_from_dsq_set_vtime", "scx_bpf_dispatch_from_dsq_set_vtime", [
        Arg("vtime", "vtime", "%llu", "args.vtime"),
    ]),
    Op("dispatch_vtime_from_dsq", "scx_bpf_dispatch_vtime_from_dsq", [
        task_pid_arg(),
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("enq_flags", "enq_flags", "%llu", "args.enq_flags"),
    ], ret_fmt="%d"),
    Op("dispatch_cancel", "scx_bpf_dispatch_cancel", []),
    Op("dispatch_nr_slots", "scx_bpf_dispatch_nr_slots", [], ret_fmt="%u"),
    Op("dsq_nr_queued", "scx_bpf_dsq_nr_queued", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
    ], ret_fmt="%d"),
    Op("dsq_peek", "scx_bpf_dsq_peek", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
    ]),
    Op("create_dsq", "scx_bpf_create_dsq", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
        Arg("node", "node", "%d", "args.node"),
    ], ret_fmt="%d"),
    Op("destroy_dsq", "scx_bpf_destroy_dsq", [
        Arg("dsq_id", "dsq_id", "%llu", "args.dsq_id"),
    ]),

    # --- CPU select / kick ---
    Op("select_cpu_dfl", "scx_bpf_select_cpu_dfl", [
        task_pid_arg(),
        Arg("prev_cpu", "prev_cpu", "%d", "args.prev_cpu"),
        Arg("wake_flags", "wake_flags", "%llu", "args.wake_flags"),
    ], ret_fmt="%d"),
    Op("select_cpu_and", "scx_bpf_select_cpu_and", [
        task_pid_arg(),
        Arg("prev_cpu", "prev_cpu", "%d", "args.prev_cpu"),
        Arg("wake_flags", "wake_flags", "%llu", "args.wake_flags"),
    ], ret_fmt="%d"),
    Op("pick_idle_cpu", "scx_bpf_pick_idle_cpu", [
        Arg("flags", "flags", "%llu", "args.flags"),
    ], ret_fmt="%d"),
    Op("pick_idle_cpu_node", "scx_bpf_pick_idle_cpu_node", [
        Arg("node", "node", "%d", "args.node"),
        Arg("flags", "flags", "%llu", "args.flags"),
    ], ret_fmt="%d"),
    Op("pick_any_cpu", "scx_bpf_pick_any_cpu", [
        Arg("flags", "flags", "%llu", "args.flags"),
    ], ret_fmt="%d"),
    Op("pick_any_cpu_node", "scx_bpf_pick_any_cpu_node", [
        Arg("node", "node", "%d", "args.node"),
        Arg("flags", "flags", "%llu", "args.flags"),
    ], ret_fmt="%d"),
    Op("test_and_clear_cpu_idle", "scx_bpf_test_and_clear_cpu_idle", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
    ], ret_fmt="%d"),
    Op("kick_cpu", "scx_bpf_kick_cpu", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
        Arg("flags", "flags", "%llu", "args.flags"),
    ]),

    # --- idle / cpumask (low value, often inlined-away — may be no-ops) ---
    Op("get_idle_cpumask", "scx_bpf_get_idle_cpumask", []),
    Op("get_idle_cpumask_node", "scx_bpf_get_idle_cpumask_node", [
        Arg("node", "node", "%d", "args.node"),
    ]),
    Op("get_idle_smtmask", "scx_bpf_get_idle_smtmask", []),
    Op("get_idle_smtmask_node", "scx_bpf_get_idle_smtmask_node", [
        Arg("node", "node", "%d", "args.node"),
    ]),
    Op("get_online_cpumask", "scx_bpf_get_online_cpumask", []),
    Op("get_possible_cpumask", "scx_bpf_get_possible_cpumask", []),
    Op("put_cpumask", "scx_bpf_put_cpumask", []),
    Op("put_idle_cpumask", "scx_bpf_put_idle_cpumask", []),

    # --- cpuperf / introspection (low rate) ---
    Op("cpu_node", "scx_bpf_cpu_node", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
    ], ret_fmt="%d"),
    Op("cpu_rq", "scx_bpf_cpu_rq", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
    ]),
    Op("cpuperf_cap", "scx_bpf_cpuperf_cap", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
    ], ret_fmt="%u"),
    Op("cpuperf_cur", "scx_bpf_cpuperf_cur", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
    ], ret_fmt="%u"),
    Op("cpuperf_set", "scx_bpf_cpuperf_set", [
        Arg("cpu", "cpu_arg", "%d", "args.cpu"),
        Arg("perf", "perf", "%u", "args.perf"),
    ]),
    Op("now", "scx_bpf_now", [], ret_fmt="%llu"),
    Op("nr_cpu_ids", "scx_bpf_nr_cpu_ids", [], ret_fmt="%u"),
    Op("nr_node_ids", "scx_bpf_nr_node_ids", [], ret_fmt="%u"),
    Op("task_cpu", "scx_bpf_task_cpu", [
        task_pid_arg(),
    ], ret_fmt="%d"),
    Op("task_running", "scx_bpf_task_running", [
        task_pid_arg(),
    ], ret_fmt="%d"),
    Op("task_cgroup", "scx_bpf_task_cgroup", [
        task_pid_arg(),
    ]),  # returns struct cgroup * — noisy to deref in fexit; entry-only
    Op("events", "scx_bpf_events", []),
    Op("reenqueue_local", "scx_bpf_reenqueue_local", [], ret_fmt="%u"),

    # --- diagnostic (low rate) ---
    Op("dump_bstr", "scx_bpf_dump_bstr", []),
    Op("error_bstr", "scx_bpf_error_bstr", []),
    Op("exit_bstr", "scx_bpf_exit_bstr", []),
]


# ---------------------------------------------------------------------------
# Probe emission
# ---------------------------------------------------------------------------

JSON_HEADER = (
    '"ts_ns":%llu,"cpu":%d,"pid":%d,'
    '"kind":"%s","name":"%s","phase":"%s"'
)


def _bt_escape(s: str) -> str:
    """
    Escape a Python-side string for inclusion inside a bpftrace double-quoted
    string literal: backslash → \\\\, double-quote → \\".
    """
    return s.replace("\\", "\\\\").replace('"', '\\"')


def emit_op(op: Op, kind: str) -> str:
    """
    Emit entry block + (optional) exit block for one op.

    Attach mechanism per kind:
      - kind=='structop': kprobe + kretprobe (positional args; cpu/nsecs OK)
      - kind=='helper'  : fentry + fexit    (typed args; cpu/nsecs OK)

    PID filter: $1 (positional arg). 0 = no filter; matches all PIDs.
    Predicate is only applied to helpers (where `pid` builtin is reliable);
    structops omit the filter because structop wrappers may fire from
    kthread / idle context where the calling thread's PID is meaningless.
    """
    blocks = []
    if kind == "structop":
        entry_prefix = "kprobe"
        exit_prefix = "kretprobe"
        attach_prefix_sep = ""  # "kprobe:sym" not "kprobe:vmlinux:sym"
        pred = "/* no filter — kprobe context, pid filter unreliable */"
    else:
        entry_prefix = "fentry"
        exit_prefix = "fexit"
        attach_prefix_sep = "vmlinux:"
        pred = "/$1 == 0 || pid == $1/"
    pid_expr = "0" if kind == "structop" else "pid"

    # ---- fentry ----
    args_inner_parts: list[str] = []
    args_fmt_parts: list[str] = []
    args_expr_parts: list[str] = []
    for a in op.args:
        if a.quoted:
            args_inner_parts.append(f'"{a.json_key}":"{a.fmt}"')
        else:
            args_inner_parts.append(f'"{a.json_key}":{a.fmt}')
        args_expr_parts.append(a.expr)
    args_inner = ",".join(args_inner_parts)
    args_blob = f',"args":{{{args_inner}}}' if op.args else ',"args":{}'
    ret_blob = ',"ret":null'

    fmt = _bt_escape("{" + JSON_HEADER + args_blob + ret_blob + "}") + "\\n"
    expr_list = ", ".join(args_expr_parts)
    expr_tail = f", {expr_list}" if expr_list else ""

    blocks.append(
        f'{entry_prefix}:{attach_prefix_sep}{op.sym}\n'
        f'{pred}\n'
        f'{{\n'
        f'    $ts = nsecs; $c = cpu;\n'
        f'    printf("{fmt}", $ts, $c, {pid_expr}, "{kind}", "{op.name}", "entry"{expr_tail});\n'
        f'}}\n'
    )

    # ---- exit probe (only if the op returns a value we want) ----
    if op.ret_fmt is not None:
        fmt_x = (
            _bt_escape(
                "{" + JSON_HEADER
                + ',"args":{}'
                + f',"ret":{op.ret_fmt}'
                + "}"
            )
            + "\\n"
        )
        blocks.append(
            f'{exit_prefix}:{attach_prefix_sep}{op.sym}\n'
            f'{pred}\n'
            f'{{\n'
            f'    $ts = nsecs; $c = cpu;\n'
            f'    printf("{fmt_x}", $ts, $c, {pid_expr}, "{kind}", "{op.name}", "exit", {op.ret_expr});\n'
            f'}}\n'
        )
    return "\n".join(blocks)


PROBE_HEADER_STRUCTOPS = '''#!/usr/bin/env bpftrace
/*
 * structops_full.bt — comprehensive sched_ext structop tracer (JSONL output).
 *
 * GENERATED FILE. Do not edit by hand. Edit gen_structops_helpers.py instead
 * and regenerate.
 *
 * Attaches kprobe+kretprobe on every `sched_ext_ops__<op>` wrapper exposed
 * by the kernel (34 of 37 callable ops; cgroup_set_idle / sub_attach /
 * sub_detach are not yet wrapped — see SURVEY.md). kprobe (not fentry) is
 * required because bpftrace v0.25's strict-mode verifier rejects `printf`
 * in several structop fentry trampoline contexts (e.g. dispatch fires
 * from the idle loop with NULL current).
 *
 * Output: one JSONL record per call, schema:
 *   {"ts_ns":<u64>,"cpu":<i32>,"pid":<i32>,
 *    "kind":"structop","name":"<op>","phase":"<entry|exit>",
 *    "args":{...},"ret":<val|null>}
 *
 * Designed to be byte-compatible with scxsim's matching `--trace-format
 * structops-jsonl` emitter so that scripts/compare_live_vs_scxsim_calls.sh
 * can do a mechanical sequence-diff.
 *
 * Usage:
 *   sudo bpftrace structops_full.bt          # all PIDs
 *   sudo bpftrace structops_full.bt 12345    # filter to PID 12345
 *
 * Tested: kernel 6.16.1-fbk2 on devbig176; bpftrace v0.25.0-be6e (May 2026).
 * Tg:     bpftrace-structops-helpers-trace-for-scxsim-vs-live-comparison
 */

/*
 * No BEGIN block: bpftrace v0.25's BEGIN expansion references
 * bpf_get_smp_processor_id, which the strict-mode null-check refuses to
 * accept inside the structops fentry trampoline context. The generated
 * output is JSONL-only — no banner.
 */

'''

PROBE_HEADER_HELPERS = '''#!/usr/bin/env bpftrace
/*
 * helpers_full.bt — comprehensive scx_bpf_* helper tracer (JSONL output).
 *
 * GENERATED FILE. Do not edit by hand. Edit gen_structops_helpers.py instead
 * and regenerate.
 *
 * Attaches fentry+fexit on every `scx_bpf_*` helper exposed by the kernel
 * (40 helpers, cross-checked against scx/scheds/include/scx/common.bpf.h).
 *
 * Output: one JSONL record per call, schema:
 *   {"ts_ns":<u64>,"cpu":<i32>,"pid":<i32>,
 *    "kind":"helper","name":"<helper>","phase":"<entry|exit>",
 *    "args":{...},"ret":<val|null>}
 *
 * Designed to be byte-compatible with scxsim's matching `--trace-format
 * structops-jsonl` emitter.
 *
 * Usage:
 *   sudo bpftrace helpers_full.bt           # all PIDs
 *   sudo bpftrace helpers_full.bt 12345     # filter to PID 12345
 *
 * Tested: kernel 6.16.1-fbk2 on devbig176; bpftrace v0.25.0-be6e (May 2026).
 * Tg:     bpftrace-structops-helpers-trace-for-scxsim-vs-live-comparison
 */

BEGIN
{
    if ($1 != 0) {
        printf("# helpers_full.bt: PID filter = %d\\n", $1);
    } else {
        printf("# helpers_full.bt: no PID filter (all tasks)\\n");
    }
}

'''


def main(argv: list[str]) -> int:
    out_dir = Path(__file__).parent.resolve()
    structops_path = out_dir / "structops_full.bt"
    helpers_path = out_dir / "helpers_full.bt"

    structops_body = "\n".join(emit_op(op, "structop") for op in STRUCTOPS)
    helpers_body = "\n".join(emit_op(op, "helper") for op in HELPERS)

    structops_path.write_text(PROBE_HEADER_STRUCTOPS + structops_body)
    helpers_path.write_text(PROBE_HEADER_HELPERS + helpers_body)
    os.chmod(structops_path, 0o755)
    os.chmod(helpers_path, 0o755)

    print(f"wrote {structops_path} ({len(STRUCTOPS)} ops, "
          f"{sum(1 for o in STRUCTOPS if o.ret_fmt) } fexit pairs)")
    print(f"wrote {helpers_path} ({len(HELPERS)} helpers, "
          f"{sum(1 for o in HELPERS if o.ret_fmt)} fexit pairs)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
