---
title: tickless wrapper has no bpf_timer plumbing — bpf_timer_init would call absolute address 169
status: closed
priority: 1
issue_type: bug
depends_on:
  sim-hfvmf: related
created_at: 2026-08-12T14:50:35.568735707+00:00
updated_at: 2026-08-12T22:30:34.878445878+00:00
closed_at: 2026-08-12T22:30:34.878445707+00:00
---

# Description

`schedulers/tickless/wrapper.c` provides no `bpf_timer_*` overrides, unlike every other scheduler that uses timers.

Count of 'timer' mentions per wrapper: lavd 128, cosmos 38, mitosis 31, **tickless 0**, simple 0.

lavd/mitosis/cosmos each `#undef` and `#define` `bpf_timer_init` / `bpf_timer_set_callback` / `bpf_timer_start` to route into the simulator's timer substrate. Without those overrides, tickless's calls resolve to libbpf's raw BPF helper-ID function pointers from bpf_helper_defs.h:

    static long (* const bpf_timer_init)(struct bpf_timer *timer, void *map, __u64 flags) = (void *) 169;

i.e. an indirect call to absolute address 169, which segfaults in userspace. `nm -D` on libscx_tickless.so confirms zero `bpf_timer_*` symbols -- neither defined nor undefined, so nothing is linked to intercept them.

This is currently MASKED by sim-hfvmf (the arena reset wipes tickless's primary CPU mask, so `tickless_init` never reaches `init_timer`). Fixing sim-hfvmf alone will convert a silent coverage gap into a SIGSEGV. The two must land together.

The simulator's timer substrate itself exists and works -- kfuncs.rs `MAX_BPF_TIMERS = 8`, `EventKind::TimerFired`, `pending_timers`; lavd drives it today. tickless is simply not wired to it.

Uncovered as a result: init_timer, start_timer_on_cpu, sched_timerfn, tick_interval_ns (plus start_timer, which additionally has no Rust-side caller). Found in tg task tickless-timer-coverage-gap.

# Design

Mirror lavd's slot-table approach (schedulers/lavd/wrapper.c:209-233): a fixed-size table keyed by `(struct bpf_timer *)` mapping to callback + map, with `bpf_timer_init` allocating a slot, `bpf_timer_set_callback` recording the callback, and `bpf_timer_start` calling the Rust `sim_timer_start()` kfunc. Mitosis and cosmos use a simpler single-timer variant that would also suffice, since tickless arms one timer per primary CPU -- note that is per-CPU, so the slot table is the better fit.

Separately, `start_timer` is a SEC("syscall") entry point with no caller in the simulator; exercising it needs a Rust-side invocation, mirroring how enable_primary_cpu is called from tickless_setup.

# Acceptance Criteria

- tickless_init successfully initializes a timer on each primary CPU without crashing.
- sched_timerfn fires during a simulation run and is observable in the trace.
- tickless function coverage rises from 20/28; expected 27/28 with this plus sim-hfvmf, 28/28 if start_timer also gets a caller.

# Notes

VERDICT ON THE NO-STUB QUESTION: tickless's real timer logic is ABSENT. It does not run in a degraded form — it does not run at all.

This is the distinction that decides urgency, so stating it plainly and with evidence rather than by inference. From the llvm-cov measurement at f76d4d9 (task fresh-coverage-all-schedulers), scx_tickless/src/bpf/main.bpf.c is 20/28 functions. ALL EIGHT uncovered functions are one coherent gap, and it is the scheduler's central mechanism:

  init_timer, start_timer, start_timer_on_cpu, sched_timerfn, tick_interval_ns
      -> the entire BPF timer path. sched_timerfn, the periodic callback that IS tickless's
         reason for existing, never executes once in the whole test suite.
  dispatch_cpu, dispatch_all_cpus, is_pcpu_task
      -> the multi-CPU bounce path that the timer callback drives.

So a scheduler named 'tickless' currently runs in scxsim with its tick/timer mechanism entirely
unexecuted. ops.dispatch degenerates to a bare scx_bpf_dsq_move_to_local(SHARED_DSQ).

Under the No-Stub Rule that means tickless is NOT 'supported with a gap' — a chunk of its BPF logic is
simply not being executed, which is the same category as an elided library. It should be treated as
NOT YET SUPPORTED until both this and sim-hfvmf are fixed, and any prior result obtained from tickless
in scxsim describes a scheduler that was not doing tickless's actual work.

TWO BUGS EACH HIDING THE OTHER, which is why this stayed invisible:
  sim-hfvmf wipes the primary-CPU cpumask on the per-run arena reset, so is_primary_cpu() is false
  forever, so tickless_init never reaches init_timer -- and therefore never reaches the missing
  bpf_timer plumbing this issue is about. Fixing hfvmf ALONE converts a silent gap into a SIGSEGV
  (bpf_timer_init is a libbpf function pointer holding raw helper id 169). They must land together,
  hfvmf first or simultaneously, never rq117 alone.

Function coverage could not have caught this on its own: the eight functions simply never appear, and
the suite stays green throughout, because no test asserts that tickless's timer ever fires.
