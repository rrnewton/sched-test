---
title: 'scxsim lavd: cgroup_bw''s ATQ and arena spin locks are no-ops on a "single-threaded" premise that --preemptive and the interleave modes no longer meet, and the sim ATQ breaks vtime ties the other way'
status: open
priority: 2
issue_type: bug
labels:
- no-stub
- lavd
- substrate-gap
depends_on:
  sim-d9988: related
  sim-73ae8: related
created_at: 2026-09-25T05:18:55.260544777+00:00
updated_at: 2026-09-25T05:21:26.463942443+00:00
---

# Description

DEFECT

The lavd wrapper compiles `scx/lib/cgroup_bw.bpf.c` with its synchronisation stripped out:
- `scx_atq_lock()` and `scx_atq_unlock()` are no-ops.
- `arena_spin_lock()` and `arena_spin_unlock()` are no-ops, and `arena_spin_trylock()` always succeeds. `crates/scx_simulator/scxtest/overrides.h` defines the same lock and unlock no-ops.
- `bpf_rcu_read_lock()` and `bpf_rcu_read_unlock()` are no-ops.
- `smp_load_acquire()` and `smp_store_release()` are plain volatile accesses.

The ATQ itself is replaced by a host sorted array, `crates/scx_simulator/csrc/sim_atq.c`. Each locked entry point just calls its `_unlocked` twin, and `scx_atq_task_detach` does not wait for holders.

Every one of these rests on the same premise, stated at each site:
- the lavd wrapper's glue block: "`arena_spinlock_t` -> `int` and `arena_spin_lock/_unlock` -> no-op. The simulator is single-threaded; the BPF arena spinlock has no contention to model. Phase 3 (`design-and-implement-stochastic-timer-interleaving-mode-for-scxsim-phase3`) is where genuine timing-race coverage will live; until then no-op is correct."
- the wrapper's `scx_atq_lock`/`scx_atq_unlock` macros: "single-threaded simulator -- the spinlock has no contention to model";
- the `sim_atq.c` header: "The simulator is single-threaded -- the `scx_atq_lock` / `scx_atq_unlock` pair from the production header expand to no-ops (or to RBC_GUARD pairs if needed)";
- its task-lifecycle block: "the single-threaded sim collapses the lock/hold-wait loops to no-ops";
- `scx_atq_cancel`'s not-found return: "Race-loser path in production. Single-threaded sim never hits this."

The simulator is no longer single-threaded in that sense:

- `--preemptive` (`PreemptiveConfig`) preempts in the middle of C code after a random number of retired branches, and runs another CPU's callback.
  - Nothing makes these lock regions non-preemptible.
  - `csrc/sim_rbc_guard.h`'s RBC_GUARD is not used in `sim_atq.c` or around the lock calls.
- `--interleave` switches callbacks at kfunc yield points.
- Phase 3 exists:
  - `--stochastic-timer-interleave` fires cgroup_bw's replenish timer between dispatch-path steps;
  - so do the targeted `scxsim_cbw_yield` sites, which are the patch carried on the scx pin.

PRODUCTION (scx 413031d44)

- `cbw_put_aside()`:
  1. takes the BTQ lock, returning -EBUSY if it cannot;
  2. re-checks under the lock that `bill_id` still maps to the LLC context. The code's own comment: "cbw_free_llc_ctx() deletes the map entry before draining the BTQ ... a concurrent free can only drain after we unlock";
  3. checks `taskc->atq`;
  4. inserts with `scx_atq_insert_vtime_unlocked()`.
- `cbw_cancel_with_hold()` takes the lock the same way, and takes a hold on the task before removing it.
- Every `scx_atq_*` entry point in `scx/lib/atq.bpf.c` takes the same lock internally, so another CPU spins on it.
- `scx_atq_task_detach()` latches `SCX_ATQ_DEAD`, then spins `while ((holdcnt = taskc->holdcnt) > 0 && can_loop)`. So `ops.exit_task` frees the task context only after every holder has dropped it. `scx_cgroup_bw_move()` relies on this: "A concurrent ops.exit_task may latch SCX_ATQ_DEAD during the window, but it must wait for our hold before freeing the task context".
- The vtime BTQ is an `RB_DUPLICATE` rbtree (`scx/lib/rbtree.bpf.c`). Insertion descends left on `key <= node->key`, so of two tasks with the same vtime, the one inserted later pops first.

SIMULATOR (sched-test 24d864c6)

- A preemption can land between `scx_atq_lock(btq)` and `scx_atq_unlock(btq)`.
- The other CPU's callback can then pop from, remove from, or drain that BTQ, because `sim_atq.c` takes no lock. `cbw_free_llc_ctx()` can drain it too. The production lock rules all of that out.
- `scx_atq_task_detach` latches `SCX_ATQ_DEAD` and returns at once. Its comment: "the single-threaded sim has no concurrent holders, so holdcnt is already balanced by the paired hold/drop calls and no wait is needed". Under `--preemptive`, a cgroup move preempted while it holds a task can see `ops.exit_task` free that task's context before the hold is dropped. The simulator never reclaims arena memory within a run (`sim_sdt_stubs.c`: "sim_arena_free() never reclaims within a run"), so nothing crashes, but production's ordering is gone.
- Equal vtimes pop in the opposite order. `sim_atq.c` inserts after every entry with an equal key ("first entry with vtime > key"), so of two tasks with the same vtime, the older pops first. FIFO-mode queues agree with production, because their keys are unique sequence numbers.

CONSEQUENCE

1. Races that production cannot have become reachable. A task parked in a BTQ that was drained after the re-check is exactly the lost-task case the lock exists to prevent. So a failure found under `--preemptive` in this code may be a simulator artefact, not a lavd bug.
2. The contention production does have is never exercised: spinning on the BTQ lock, the -EBUSY path, and the wait in `scx_atq_task_detach`.
3. Throttled tasks with equal vtimes leave the BTQ in the opposite order to production, in every mode, not only under `--preemptive`.

Today's impact is UNVERIFIED.

FIX DIRECTION

- Model the locks instead of deleting them. When a callback tries to take a held lock, hand control back to the interleaver until the holder releases it. That makes both contention and the failure paths reachable.
- As an interim minimum, mark each critical section non-preemptible with RBC_GUARD, so a preemption cannot land inside one.
- Make `scx_atq_task_detach` yield to the interleaver while `holdcnt > 0`, where production spins.
- Either run lib/atq.bpf.c itself once the arena substrate allows it (sim-d9988), or insert equal keys ahead of existing ones and prove the order with a differential test.
- Correct the "single-threaded" comments in the lavd wrapper and `sim_atq.c`.

ACCEPTANCE

- A `--preemptive` or targeted-yield test forces a switch inside `cbw_put_aside`'s lock region. It asserts that the second CPU's access to the same BTQ waits until the first releases the lock.
- A test runs `ops.exit_task` while `scx_cgroup_bw_move` holds the task, and asserts that the detach returns only after the hold is dropped.
- A differential test inserts tasks with equal vtimes and pops them in lib/atq's order.
- Until this is fixed, `sim_atq.c` and the wrapper's `scx_atq_lock`/`scx_atq_unlock` carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row S08 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
