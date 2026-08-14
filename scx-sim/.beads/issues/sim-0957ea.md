---
title: 'scxsim substrate: deliver futex wait/wake events to LAVD lock.bpf.c boost hooks'
status: in_progress
priority: 2
issue_type: feature
created_at: 2026-07-23T20:53:28.625149229+00:00
updated_at: 2026-07-26T17:51:57.785681633+00:00
---

# Description

LAVD lock.bpf.c (futex lock-holder boosting) is compiled into libscx_lavd.so (wrapper.c:584) but 0% executed under sim: the engine never delivers any futex fexit/tracepoint, so none of the 12 hooks nor inc/dec_futex_boost ever run (COVERAGE_AUDIT_20260722 sec 3a). The consumer side (reset_lock_futex_boost, is_lock_holder(_running), lat_cri NEED_LOCK_BOOST) DOES run every cycle but always with FUTEX_BOOST=false, so the boosted branches are dead.

Hooks are exported (T) so FFI-callable, but they call bpf_get_current_task_btf()/get_cpu_ctx() which need SIM_ARC + current task/cpu -> abort outside Simulator::run (same class as sim-91a825).

Minimum substrate (tracepoint route, No-Stub: run real lock.bpf.c, do not reimplement boost in Rust): add a scenario futex event 'task T's contended WAIT returned success' / 'WAKE returned >0', tied to T while running on its CPU; engine invokes the scheduler's real hook (rtp_sys_exit_futex_wait / rtp_sys_exit_futex_wake, or the rtp_sys_enter_futex+rtp_sys_exit_futex pair) with SIM_ARC set. Natural modeling: futex_wait = Sleep-until-woken whose resume carries 'lock acquired' -> fire inc on resume; futex_wake = Wake carrying 'lock released' -> fire dec on waker.

A single WAIT-success event on a running task (with a competing task present) + a later STOP lights up: inc/__inc_futex_boost, one wait hook, reset_lock_futex_boost boosted branch, is_lock_holder(_running) true, preempt protection (preempt.bpf.c:56), dispatch continuation (main.bpf.c:1255), and lat_cri NEED_LOCK_BOOST (+128 weight). WAKE-success lights dec/__dec_futex_boost. Covering all 12 hook symbols needs per-hook (op-parameterized) invocation; covering the boost LOGIC needs only one route.

Research: tg research-lavd-futex-hooks. Related: sim-91a825 (syscall-prog injection), sim-39706c (execve tracepoint).

# Notes

Phase-1 substrate LANDED at 106493f (integration) and VERIFIED 2026-07-26: EventKind::FutexOp + handle_futex_op deliver the event; lavd_futex_hook (wrapper.c) calls REAL rtp_sys_enter_futex+rtp_sys_exit_futex -> __inc/__dec_futex_boost in real lock.bpf.c (No-Stub). tests/lavd_futex_boost.rs (2 tests) confirm LAVD_FLAG_FUTEX_BOOST set on WaitAcquired / cleared on WakeReleased, and non-running-task ops are skipped not mis-attributed. Full ./validate.sh PASSES: 987 tests pass / 14 skipped, fmt+clippy clean, mypy clean, stress.py smoke ok. lock.bpf.c coverage 0% -> 45.5% regions; 5/17 funcs now execute. REMAINING (keep issue open): Phase-3 = the 7 fexit hooks + 3 dedicated tracepoints (redundant, all funnel to inc/dec) per FUTEX_SIM_DESIGN; rt-app lock->futex mapping (rtapp-locks-futex-scxsim-gap).
