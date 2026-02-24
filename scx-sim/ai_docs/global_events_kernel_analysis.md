# Global Events: Kernel Implementation Analysis

This document analyzes the Linux kernel implementation of the six event types
that the simulator currently classifies as "global" (no CPU affinity). For each
event, we trace the kernel code path from trigger to sched_ext callback,
document the exact locking sequence, identify which CPU executes the callback,
and assess concurrency properties.

**Kernel source reference**: Local tree at
`<REPO_ROOT>/../../rrn_scx_playground/linux/` (based on a recent mainline kernel
with sched_ext support).

---

## 1. Summary Table

| Event Type | Origin (HW/OS Trigger) | CPU That Runs Callback | Execution Context | Key Locks Held | Can Run Concurrently With Self? | Can Run Concurrently With Other Ops? |
|---|---|---|---|---|---|---|
| **TaskWake** | Syscall, signal, IRQ completion, timer, futex, pipe, etc. on the **waker's CPU** | **Waker's CPU** (the CPU executing `try_to_wake_up`) runs `select_cpu`; the **selected target CPU's rq lock** is acquired for `runnable`/`enqueue` | Process context (waker) or softirq/IRQ context | `p->pi_lock`, then `target_rq->lock` | **Yes** -- two different tasks waking on different CPUs run fully in parallel | **Yes** -- wake path only holds per-task `pi_lock` + per-CPU `rq->lock` |
| **TimerFired** (BPF timer) | `hrtimer` expiry on the **CPU where `bpf_timer_start()` was called** (with PINNED) or potentially migrated CPU (without PINNED) | **The CPU whose softirq fires** (typically the CPU that armed the timer) | Softirq context (`hrtimer_run_softirq`) | **No scheduler locks** -- BPF timer callback runs without any rq lock or pi_lock | **Yes** (on different CPUs) -- each CPU has independent softirq | **Yes** -- no scheduler locks held |
| **CgroupMigrate** | Userspace writes to `cgroup.procs` / `cgroup.threads` (syscall: `write(2)`) | `can_attach` (including `cgroup_prep_move`): **any CPU** (process context of writer). `attach`/`sched_move_task` (including `cgroup_move`): **any CPU** (process context of writer) | Process context (the task writing to cgroupfs) | `cgroup_mutex` (global), `cgroup_threadgroup_rwsem`, then per-task `p->pi_lock` + `rq->lock` via `task_rq_lock()` | **No** -- `cgroup_mutex` is a global mutex; only one cgroup migration at a time | **Partially** -- `cgroup_mutex` blocks other cgroup ops; but `rq->lock` is per-CPU so scheduler callbacks on other CPUs can proceed |
| **CgroupCreate** | Userspace `mkdir` on cgroupfs (syscall: `mkdir(2)`) | **Any CPU** (process context of the task creating the directory) | Process context | `cgroup_mutex` (global) | **No** -- serialized by `cgroup_mutex` | **No** with other cgroup ops (same mutex); **Yes** with scheduler ops on other CPUs |
| **CgroupDestroy** | Userspace `rmdir` on cgroupfs or last reference dropped (syscall: `rmdir(2)`) | **Any CPU** (process context of the task removing the directory, or RCU callback CPU for deferred cleanup) | Process context (or RCU callback for `css_offline`) | `cgroup_mutex` (global) | **No** -- serialized by `cgroup_mutex` | **No** with other cgroup ops; **Yes** with scheduler ops |
| **CgroupCpusetChange** | Userspace writes to `cpuset.cpus` (syscall: `write(2)`) | **Any CPU** (process context of writer); per-task `set_cpus_allowed_ptr` calls use `task_rq_lock` | Process context | `cpuset_mutex`, `cgroup_mutex` (often held together); per-task: `p->pi_lock` + `rq->lock` | **No** -- serialized by `cpuset_mutex`/`cgroup_mutex` | **No** with other cgroup ops; individual task affinity updates hold per-CPU `rq->lock` |

---

## 2. Detailed Analysis per Event

### 2.1 TaskWake

#### Kernel Code Path

```
User/kernel trigger (signal, futex, pipe, timer, etc.)
  -> wake_up_process() / try_to_wake_up()
      -> [acquire p->pi_lock]
      -> ttwu_state_match() -- check task state
      -> smp_rmb() / smp_cond_load_acquire(&p->on_cpu)
      -> select_task_rq(p, ...)
          -> select_task_rq_scx(p, prev_cpu, wake_flags)
              -> SCX_CALL_OP_TASK_RET(sch, SCX_KF_ENQUEUE|SCX_KF_SELECT_CPU,
                                       select_cpu, NULL, p, prev_cpu, wake_flags)
              [NOTE: NULL rq -- no rq lock held during select_cpu]
      -> set_task_cpu(p, cpu)  [under p->pi_lock, no rq lock]
      -> ttwu_queue(p, cpu, wake_flags)
          -> [acquire target rq->lock]
          -> ttwu_do_activate(rq, p, wake_flags, &rf)
              -> activate_task(rq, p, en_flags)
                  -> enqueue_task(rq, p, flags)
                      -> enqueue_task_scx(rq, p, enq_flags)
                          -> set_task_runnable(rq, p)
                          -> SCX_CALL_OP_TASK(sch, SCX_KF_REST, runnable, rq, p, enq_flags)
                          -> do_enqueue_task(rq, p, enq_flags, sticky_cpu)
                              -> SCX_CALL_OP_TASK(sch, SCX_KF_ENQUEUE, enqueue, rq, p, enq_flags)
          -> [release target rq->lock]
      -> [release p->pi_lock]
```

#### Exact Locking Sequence

1. **`p->pi_lock`** -- acquired at entry to `try_to_wake_up()`. This is a
   per-task spinlock that serializes concurrent wakeups of the same task.
2. **`ops.select_cpu`** is called **without any rq lock** (the `rq` parameter to
   `SCX_CALL_OP` is `NULL`). Only `p->pi_lock` is held. The `SCX_KF_ENQUEUE |
   SCX_KF_SELECT_CPU` mask allows the BPF program to call `scx_bpf_dispatch()`
   (direct dispatch).
3. **`target_rq->lock`** -- acquired in `ttwu_queue()` for the CPU selected by
   `select_task_rq_scx()`.
4. **`ops.runnable`** is called with `target_rq->lock` held (`SCX_KF_REST`).
5. **`ops.enqueue`** is called with `target_rq->lock` held (`SCX_KF_ENQUEUE`).
6. Both locks released in reverse order.

#### CPU Selection Logic

- `try_to_wake_up()` executes on the **waker's CPU** (the CPU of the task that
  calls `wake_up_process()`, or the CPU handling the interrupt/softirq that
  triggers the wakeup).
- `ops.select_cpu()` runs on the **waker's CPU** with only `p->pi_lock` held.
  The BPF helper `bpf_get_smp_processor_id()` returns the waker's CPU.
- After `select_task_rq_scx()` returns a CPU, `ttwu_queue()` acquires the
  **target CPU's rq lock**. If the target CPU is remote and the task is still
  running there, `ttwu_queue_wakelist()` may send an IPI and defer the enqueue
  to the target CPU itself.
- `ops.runnable()` and `ops.enqueue()` run on the **waker's CPU** while holding
  the **target CPU's rq lock** (remote lock acquisition), unless the wake list
  path was taken, in which case they run on the **target CPU** via
  `sched_ttwu_pending()`.

#### Concurrency Properties

- **Two TaskWake events for different tasks on different CPUs**: Fully concurrent.
  Each holds its own `p->pi_lock` and its own target `rq->lock`. No contention
  unless they both target the same CPU's rq.
- **Two TaskWake events targeting the same CPU**: Serialized by `rq->lock` of
  that CPU during the `runnable`/`enqueue` phase. The `select_cpu` phase can
  still run concurrently (different `pi_lock`).
- **TaskWake concurrent with Tick on same CPU**: Serialized by the target
  `rq->lock`. The tick holds the local CPU's `rq->lock`, and if a remote wakeup
  targets that CPU, it must wait for the rq lock.
- **TaskWake concurrent with Tick on different CPU**: Fully concurrent (different
  rq locks).

---

### 2.2 TimerFired (BPF Timer)

#### Kernel Code Path

```
bpf_timer_start(timer, nsecs, flags)
  -> hrtimer_start(&t->timer, ns_to_ktime(nsecs), HRTIMER_MODE_REL_SOFT [| PINNED])
  [timer is enqueued on current CPU's hrtimer base, or migrated if !PINNED]

... time passes ...

Hardware timer interrupt on the CPU where the hrtimer is queued
  -> hrtimer_interrupt() or tick_irq_enter()
      -> raise_timer_softirq(HRTIMER_SOFTIRQ)

Softirq processing:
  -> hrtimer_run_softirq()
      -> __hrtimer_run_queues(cpu_base, now, flags, HRTIMER_ACTIVE_SOFT)
          -> fn(timer)  [calls bpf_timer_cb]
              -> bpf_timer_cb(hrtimer)
                  -> callback_fn(map, key, value, 0, 0)
                      [this is the BPF program, e.g., fire_timer() in scx scheduler]
```

#### Exact Locking Sequence

1. **`hrtimer_cpu_base->lock`** -- the per-CPU hrtimer base lock, held during
   softirq processing of timer queues. This is a raw spinlock with IRQs
   disabled.
2. **No scheduler locks** -- the BPF timer callback (`bpf_timer_cb`) does NOT
   hold any `rq->lock` or `pi_lock`. The timer fires in softirq context on
   the timer's CPU.
3. The BPF callback (e.g., `fire_timer()`) can then call kfuncs that acquire
   scheduler locks internally (e.g., `scx_bpf_kick_cpu()` which may IPI another
   CPU).

#### CPU Selection Logic

- The timer fires on the **CPU where `bpf_timer_start()` was called**, unless:
  - `BPF_F_TIMER_CPU_PIN` was NOT set AND timer migration is enabled
    (`CONFIG_NO_HZ_COMMON`), in which case the timer may be migrated to a
    "timer target" CPU for power efficiency.
  - The original CPU went offline, in which case the timer migrates to another
    online CPU.
- With `BPF_F_TIMER_CPU_PIN` (which scx schedulers like scx_central use), the
  timer is pinned to the CPU and always fires there.
- The `HRTIMER_MODE_REL_SOFT` flag means the timer fires in softirq context (not
  hardirq), via `hrtimer_run_softirq()`.

#### Concurrency Properties

- **Two BPF timers on different CPUs**: Fully concurrent. Each CPU has its own
  `hrtimer_cpu_base` and runs softirqs independently.
- **Two BPF timers on the same CPU**: Serialized. Softirqs run sequentially on a
  given CPU. The `bpf_timer_cb` comment explicitly states: "It doesn't migrate
  and cannot be preempted by another bpf_timer_cb() on the same cpu."
- **BPF timer concurrent with scheduler callbacks**: The timer callback holds no
  scheduler locks, so it can run concurrently with any scheduler callback on
  another CPU. On the same CPU, the timer runs in softirq; if a scheduler
  callback (e.g., tick) is in progress with rq->lock held, the softirq would
  be deferred or nested depending on IRQ state.

---

### 2.3 CgroupMigrate

#### Kernel Code Path

```
Userspace: write(fd, "PID\n", ...)  [to /sys/fs/cgroup/.../cgroup.procs]
  -> cgroup_procs_write()
      -> [acquire cgroup_mutex]
      -> [acquire cgroup_threadgroup_rwsem]
      -> cgroup_attach_task(dst_cgrp, leader, threadgroup)
          -> cgroup_migrate_prepare_dst(&mgctx)
          -> cgroup_migrate(leader, threadgroup, &mgctx)
              -> cgroup_migrate_execute(&mgctx)
                  -> ss->can_attach(tset)  [for cpu subsystem:]
                      -> cpu_cgroup_can_attach(tset)
                          -> scx_cgroup_can_attach(tset)
                              -> SCX_CALL_OP_RET(sch, SCX_KF_UNLOCKED,
                                                  cgroup_prep_move, NULL, p, from, to)
                  -> [acquire css_set_lock] -- move tasks between css_sets
                  -> [release css_set_lock]
                  -> ss->attach(tset)  [for cpu subsystem:]
                      -> cpu_cgroup_attach(tset)
                          -> for each task: sched_move_task(task, false)
                              -> [acquire task_rq_lock: p->pi_lock + rq->lock]
                              -> sched_change_begin(tsk, DEQUEUE_SAVE|DEQUEUE_MOVE)
                                  -> dequeue_task(rq, p, flags)
                                      -> dequeue_task_scx(rq, p, deq_flags)
                                          -> ops_dequeue(rq, p, deq_flags)
                                              -> SCX_CALL_OP_TASK(sch, SCX_KF_REST,
                                                                   dequeue, rq, p, deq_flags)
                                          -> SCX_CALL_OP_TASK(sch, SCX_KF_REST,
                                                               quiescent, rq, p, deq_flags)
                              -> sched_change_group(tsk)  [update tsk->sched_task_group]
                              -> scx_cgroup_move_task(tsk)
                                  -> SCX_CALL_OP_TASK(sch, SCX_KF_UNLOCKED,
                                                       cgroup_move, NULL, p, from, to)
                              -> sched_change_end(ctx)
                                  -> enqueue_task(rq, p, flags)
                                      -> enqueue_task_scx(rq, p, enq_flags)
                                          -> SCX_CALL_OP_TASK(sch, SCX_KF_REST,
                                                               runnable, rq, p, enq_flags)
                                          -> do_enqueue_task(rq, p, enq_flags, sticky_cpu)
                                              -> SCX_CALL_OP_TASK(sch, SCX_KF_ENQUEUE,
                                                                   enqueue, rq, p, enq_flags)
                              -> [release task_rq_lock]
      -> [release cgroup_threadgroup_rwsem]
      -> [release cgroup_mutex]
```

#### Exact Locking Sequence

1. **`cgroup_mutex`** -- global mutex, acquired first. Serializes all cgroup
   topology changes.
2. **`cgroup_threadgroup_rwsem`** -- per-cgroup hierarchy rwsem, prevents fork/exit
   during migration.
3. **`scx_cgroup_can_attach` / `cgroup_prep_move`** -- called with
   `SCX_KF_UNLOCKED`, no rq lock held. The BPF callback is sleepable.
4. **`css_set_lock`** -- global spinlock, held briefly to atomically move tasks
   between css_sets.
5. Per-task migration via `sched_move_task`:
   - **`task_rq_lock()`** = `p->pi_lock` + `rq->lock` of the task's current CPU.
   - **`ops.dequeue`** -- called with `rq->lock` held (`SCX_KF_REST`).
   - **`ops.quiescent`** -- called with `rq->lock` held (`SCX_KF_REST`).
   - **`ops.cgroup_move`** -- called with `SCX_KF_UNLOCKED` and `rq=NULL`,
     meaning the kfuncs available are those that don't require rq lock.
     **However**, `rq->lock` IS still held at this point (from `task_rq_lock()`).
     The `NULL` rq parameter tells the SCX framework not to track this as an
     rq-locked operation, limiting which kfuncs can be called. This is a subtle
     but important detail: the BPF program cannot call rq-dependent kfuncs, but
     the rq lock IS held by the caller.
   - **`ops.runnable`** + **`ops.enqueue`** -- called with `rq->lock` held
     during re-enqueue.
   - `task_rq_lock()` released.

#### CPU Selection Logic

- The entire cgroup migration runs on **whichever CPU the userspace writer
  is executing on**. It is a syscall handler (process context).
- The `ops.cgroup_move`, `ops.dequeue`, `ops.runnable`, `ops.enqueue` callbacks
  for each migrated task are called while holding **that task's rq->lock**
  (the CPU where the task is currently queued). This may be a different CPU
  from the writer's CPU (remote rq lock acquisition).

#### Concurrency Properties

- **Two CgroupMigrate events**: Fully serialized by `cgroup_mutex`. Only one
  cgroup migration can proceed at a time.
- **CgroupMigrate concurrent with TaskWake**: The per-task `rq->lock` is held
  during `sched_move_task`, so if a wakeup targets the same CPU, it will
  contend on that rq lock. Wakeups targeting different CPUs proceed concurrently.
- **CgroupMigrate concurrent with Tick**: Similarly serialized by `rq->lock` if
  on the same CPU. Ticks on other CPUs proceed concurrently.
- The BPF callbacks `cgroup_prep_move` and `cgroup_cancel_move` are sleepable
  (`SCX_KF_UNLOCKED`) and run without rq lock, but under `cgroup_mutex`.

---

### 2.4 CgroupCreate

#### Kernel Code Path

```
Userspace: mkdir("/sys/fs/cgroup/.../new_group")
  -> cgroup_mkdir()
      -> [acquire cgroup_mutex]
      -> cgroup_create(parent, name, mode)
      -> online_css(css)
          -> ss->css_online(css)  [for cpu subsystem:]
              -> cpu_cgroup_css_online(css)
                  -> scx_tg_online(tg)
                      -> SCX_CALL_OP_RET(sch, SCX_KF_UNLOCKED,
                                          cgroup_init, NULL, tg->css.cgroup, &args)
      -> [release cgroup_mutex]
```

#### Exact Locking Sequence

1. **`cgroup_mutex`** -- global mutex, held throughout cgroup creation.
2. **`ops.cgroup_init`** -- called with `SCX_KF_UNLOCKED`, `rq=NULL`. The BPF
   callback is sleepable and holds no scheduler locks. Only `cgroup_mutex`
   serializes it.

#### CPU Selection Logic

- Runs on **whichever CPU the userspace task executing `mkdir(2)` is
  scheduled on**. This is ordinary process context.

#### Concurrency Properties

- **Two CgroupCreate events**: Fully serialized by `cgroup_mutex`.
- **CgroupCreate concurrent with scheduler ops**: `ops.cgroup_init` holds no
  scheduler locks (`rq->lock`, `pi_lock`), so it can in principle run
  concurrently with any scheduler callback. However, it does hold
  `cgroup_mutex`, which serializes it against all other cgroup operations.
- The `SCX_KF_UNLOCKED` context means the BPF program can call sleepable
  kfuncs but cannot call kfuncs that require rq lock.

---

### 2.5 CgroupDestroy

#### Kernel Code Path

```
Userspace: rmdir("/sys/fs/cgroup/.../group")  [or last ref dropped]
  -> cgroup_rmdir() / cgroup_destroy_locked()
      -> [acquire cgroup_mutex]
      -> offline_css(css)
          -> ss->css_offline(css)  [for cpu subsystem:]
              -> cpu_cgroup_css_offline(css)
                  -> scx_tg_offline(tg)
                      -> SCX_CALL_OP(sch, SCX_KF_UNLOCKED,
                                      cgroup_exit, NULL, tg->css.cgroup)
      -> [release cgroup_mutex]
```

#### Exact Locking Sequence

1. **`cgroup_mutex`** -- global mutex, held throughout cgroup destruction.
2. **`ops.cgroup_exit`** -- called with `SCX_KF_UNLOCKED`, `rq=NULL`. Sleepable,
   no scheduler locks.

#### CPU Selection Logic

- Same as CgroupCreate: runs on whatever CPU the process calling `rmdir(2)` is
  running on. For deferred cleanup (last reference drop), it runs on whatever
  CPU the last `put_css` happens on.

#### Concurrency Properties

- Identical to CgroupCreate. Serialized by `cgroup_mutex`, cannot run
  concurrently with any other cgroup operation. Can run concurrently with
  scheduler per-CPU operations (ticks, dispatches, etc.) since no scheduler
  locks are held.

---

### 2.6 CgroupCpusetChange

#### Kernel Code Path

```
Userspace: write(fd, "0-3\n", ...)  [to /sys/fs/cgroup/.../cpuset.cpus]
  -> cpuset_write_resmask()
      -> [acquire cpuset_mutex]
      -> update_cpumask(cs, ...)
          -> update_cpumasks_hier(cs, tmp, ...)
              -> for each cpuset in subtree:
                  -> cpuset_update_tasks_cpumask(cs, new_cpus)
                      -> for each task in cpuset:
                          -> set_cpus_allowed_ptr(task, new_cpus)
                              -> __set_cpus_allowed_ptr(p, &ac)
                                  -> [acquire task_rq_lock: p->pi_lock + rq->lock]
                                  -> __set_cpus_allowed_ptr_locked(p, ctx, rq, &rf)
                                      -> sched_change_begin / sched_change_end
                                          -> dequeue_task_scx / enqueue_task_scx
                                      -> set_cpus_allowed_scx(p, ac)
                                          -> SCX_CALL_OP_TASK(sch, SCX_KF_REST,
                                                               set_cpumask, NULL, p, ...)
                                  -> [release task_rq_lock]
      -> [release cpuset_mutex]
```

#### Exact Locking Sequence

1. **`cpuset_mutex`** -- cpuset-level mutex, serializes cpuset changes.
   (`cgroup_mutex` may also be held depending on the path.)
2. Per-task affinity update:
   - **`task_rq_lock()`** = `p->pi_lock` + `rq->lock`.
   - If the task is queued: **`ops.dequeue`** (with `rq->lock`, `SCX_KF_REST`).
   - **`ops.set_cpumask`** -- called with `SCX_KF_REST`, `rq=NULL`. This is
     rq-locked context but without tracking the specific rq (the `NULL` rq
     parameter). The task's `rq->lock` IS held.
   - If the task was queued: re-enqueue with **`ops.runnable`** + **`ops.enqueue`**.
   - Release `task_rq_lock()`.

#### CPU Selection Logic

- The cpuset write runs on **whichever CPU the writer is scheduled on**
  (process context syscall).
- Per-task affinity updates acquire **each task's current rq->lock**, which
  may be on different CPUs (remote lock acquisition).

#### Concurrency Properties

- **Two CgroupCpusetChange events**: Serialized by `cpuset_mutex`.
- **CgroupCpusetChange concurrent with TaskWake**: Per-task `rq->lock` contention
  if they target the same CPU. Otherwise concurrent.
- Note: A cpuset change touching N tasks will sequentially acquire N different
  `task_rq_lock()` instances. Each individual lock hold is brief, but the total
  operation can be lengthy for large cgroups.

---

## 3. Implications for the Simulator

### 3.1 Which "Global" Events Are Actually Per-CPU in the Kernel?

**TaskWake** is the most significant case. In the kernel, `TaskWake` is NOT
a global event. It is fundamentally a **per-CPU operation**:

- `ops.select_cpu()` runs on the **waker's CPU** (without rq lock, only
  `pi_lock`).
- `ops.runnable()` and `ops.enqueue()` run while holding the **target CPU's
  rq->lock** (which may be different from the waker's CPU).
- Two TaskWake events for tasks on different CPUs can proceed **fully in
  parallel**. They are only serialized when they contend on the same rq lock.

**TimerFired** is also per-CPU in the kernel. BPF timers fire in softirq
on a specific CPU. With `BPF_F_TIMER_CPU_PIN`, the timer is bound to the CPU
where `bpf_timer_start()` was called. Even without pinning, the timer fires on
a deterministic CPU. Two timers on different CPUs fire in parallel.

**CgroupMigrate, CgroupCreate, CgroupDestroy, CgroupCpusetChange** are
genuinely "global" events serialized by `cgroup_mutex`. However, the per-task
callbacks within `CgroupMigrate` and `CgroupCpusetChange` (dequeue, cgroup_move,
set_cpumask, enqueue) each acquire per-CPU rq locks, meaning they interact with
the per-CPU scheduler state in a fine-grained way.

### 3.2 Which Could Safely Participate in Concurrent Simulation?

**TaskWake: HIGH potential for concurrency.**

In the kernel, two `try_to_wake_up()` calls on different CPUs for different
tasks run fully in parallel. The only serialization point is contention on a
shared `rq->lock` when two wakeups target the same CPU. The simulator could
model this by:
- Assigning each TaskWake to the waker's CPU (or, if no waker, to any CPU).
- Processing TaskWake events on different CPUs concurrently.
- Using per-CPU locks to serialize access to shared DSQ state when needed.

Key challenges:
- `ops.select_cpu()` runs on the waker's CPU but chooses the target CPU.
  This means the event starts on one CPU and affects another. The simulator
  must handle cross-CPU dispatch atomically.
- If `select_cpu` performs direct dispatch to a remote CPU's local DSQ, that
  is a cross-CPU mutation that needs synchronization.

**TimerFired: MODERATE potential for concurrency.**

BPF timers could be associated with a specific CPU and processed as per-CPU
events. However, the timer callback typically accesses global scheduler state
(e.g., iterating over all tasks, accessing global DSQs). The scheduler BPF code
itself determines the concurrency-safety of the callback. In practice, most
scx schedulers use a single timer (e.g., scx_central's timer on CPU 0), making
this less impactful.

**CgroupCreate / CgroupDestroy: LOW potential for concurrency.**

These are serialized by `cgroup_mutex` in the kernel and hold no scheduler
locks. They modify global cgroup topology. The simulator can safely keep these
as sequential events. They are also typically rare (happen during initialization
or teardown).

**CgroupMigrate: LOW potential for concurrency (complex locking).**

While the per-task dequeue/enqueue within cgroup migration targets specific
CPUs' rq locks, the overall operation is serialized by `cgroup_mutex`. Making
this concurrent in the simulator would require modeling `cgroup_mutex`, which
adds complexity with little benefit since cgroup migrations are infrequent.

**CgroupCpusetChange: LOW potential for concurrency (complex locking).**

Similar to CgroupMigrate: serialized by `cpuset_mutex` but contains per-task
operations that hold per-CPU rq locks.

### 3.3 What Synchronization Would Need to Be Modeled?

To enable concurrent TaskWake processing, the simulator would need:

1. **Per-CPU rq lock analog**: Each simulated CPU would need a lock (or
   equivalent serialization) protecting its local DSQ and currently-running
   task state. Two wakeups targeting the same CPU must be serialized.

2. **Per-task pi_lock analog**: Each task needs serialization to prevent
   concurrent wakeups of the same task. In practice, the simulator already
   handles this via `TaskState` checks.

3. **Global DSQ lock**: The global DSQ (and per-cell DSQs) are shared across
   CPUs. In the kernel, `scx_bpf_dispatch()` to a non-local DSQ uses
   atomic operations and the DSQ's own lock. The simulator would need
   similar per-DSQ locking.

4. **Cross-CPU dispatch**: When `select_cpu` or `enqueue` dispatches to a
   remote CPU's local DSQ, the simulator must acquire that CPU's lock. The
   kernel handles this via `dispatch_to_local_dsq()` with careful lock
   ordering.

For concurrent TimerFired, the simulator would need:

1. **CPU association**: Each BPF timer must be associated with a specific CPU.
2. **BPF map access**: Timer callbacks access BPF maps (shared state). The
   kernel provides no synchronization here -- it is the BPF program's
   responsibility. The simulator would need to model this if it wants to
   detect races.

### 3.4 Where Does the Simulator's Current Model Deviate from Kernel Behavior?

1. **TaskWake has no CPU affinity**: The simulator treats TaskWake as a "global"
   event with no associated CPU. In the kernel, `try_to_wake_up()` always
   executes on a specific CPU (the waker's CPU). The waker's CPU determines:
   - What `bpf_get_smp_processor_id()` returns during `select_cpu`.
   - What `bpf_get_current_task_btf()` returns (the waker task).
   - The initial CPU for `prev_cpu` if no waker is specified.

   **Simulator behavior**: The simulator does set `wake_cpu` and `waker_raw`
   before calling the callbacks (lines 2214-2216 of `engine.rs`), partially
   modeling this. But the event itself has no CPU affinity in the event
   classification, preventing concurrent processing.

2. **TimerFired has no CPU affinity**: The simulator treats `TimerFired` as
   global. In the kernel, a BPF timer fires on a specific CPU in softirq
   context. The CPU where the timer fires matters because:
   - `bpf_get_smp_processor_id()` returns that CPU.
   - The timer callback cannot be preempted by another timer callback on the
     same CPU (but can run concurrently with timer callbacks on other CPUs).

   **Simulator behavior**: The simulator processes `TimerFired` with
   `set_sim_clock(state.clock, None)` (no CPU context), which means the timer
   callback sees no specific CPU.

3. **CgroupMigrate ops.cgroup_move runs without rq lock tracking**: The kernel
   calls `ops.cgroup_move` with `SCX_KF_UNLOCKED` and `rq=NULL`, but the
   task's `rq->lock` IS held at this point (via `task_rq_lock()` in
   `sched_move_task`). The `NULL` rq parameter limits which kfuncs the BPF
   program can call. The simulator should ensure that kfuncs requiring rq lock
   are rejected during `cgroup_move`.

4. **CgroupMigrate dequeue/enqueue context**: In the kernel, the dequeue and
   re-enqueue during cgroup migration use `sched_change_begin/end`, which is a
   specific dequeue-modify-enqueue pattern with `DEQUEUE_SAVE | DEQUEUE_MOVE`
   flags. The simulator currently uses plain dequeue (flags=0) and enqueue
   (flags=0), which may differ from kernel behavior:
   - The kernel uses `DEQUEUE_SAVE` meaning task state should be preserved.
   - The kernel uses `DEQUEUE_MOVE` meaning this is a migration context.
   - The enqueue does NOT set `SCX_ENQ_WAKEUP` since this is not a wakeup.

5. **ops.runnable call during cgroup re-enqueue**: In the kernel,
   `enqueue_task_scx` calls `ops.runnable` only when the task is being newly
   enqueued (not when `task_on_rq_migrating`). During `sched_change_end`,
   the task IS marked as migrating (`TASK_ON_RQ_MIGRATING`), so
   `ops.runnable` would be skipped (due to the `!task_on_rq_migrating(p)`
   check at ext.c line 1470). The simulator may incorrectly call `ops.runnable`
   during cgroup migration re-enqueue.

6. **ops.quiescent call during cgroup dequeue**: The kernel calls
   `ops.quiescent` during `dequeue_task_scx` only when
   `!task_on_rq_migrating(p)` (ext.c line 1562). During `sched_change_begin`,
   the task is NOT yet marked as migrating (that happens at
   `deactivate_task` -> `WRITE_ONCE(p->on_rq, TASK_ON_RQ_MIGRATING)`), so
   `ops.quiescent` IS called. However, for the cgroup migrate case
   `DEQUEUE_SAVE|DEQUEUE_MOVE` the code path goes through `dequeue_task()`
   not `deactivate_task()`, so `task_on_rq_migrating` may not be set.
   This is subtle and needs careful verification.

---

## 4. References

### Kernel Source Files (local tree)

- **`kernel/sched/ext.c`**: sched_ext implementation
  - `select_task_rq_scx()` (line 2530): CPU selection with `SCX_KF_ENQUEUE|SCX_KF_SELECT_CPU`
  - `enqueue_task_scx()` (line 1438): runnable + enqueue with `SCX_KF_REST`/`SCX_KF_ENQUEUE`
  - `dequeue_task_scx()` (line 1534): dequeue + quiescent with `SCX_KF_REST`
  - `task_tick_scx()` (line 2732): tick with `SCX_KF_REST`
  - `scx_tg_online()` (line 3119): cgroup_init with `SCX_KF_UNLOCKED`
  - `scx_tg_offline()` (line 3148): cgroup_exit with `SCX_KF_UNLOCKED`
  - `scx_cgroup_can_attach()` (line 3161): cgroup_prep_move with `SCX_KF_UNLOCKED`
  - `scx_cgroup_move_task()` (line 3210): cgroup_move with `SCX_KF_UNLOCKED`

- **`kernel/sched/core.c`**: Core scheduler
  - `try_to_wake_up()` (line 4072): Wake path with `p->pi_lock` + target `rq->lock`
  - `ttwu_queue()` (line 3888): Enqueue on target CPU
  - `ttwu_do_activate()` (line 3615): activate_task + wakeup_preempt
  - `sched_tick()` (line 5510): Timer tick with local `rq->lock`
  - `task_rq_lock()` (line 729): `p->pi_lock` + `rq->lock` acquisition
  - `sched_move_task()` (line 9119): Cgroup migration with `task_rq_lock` + `sched_change`
  - `cpu_cgroup_attach()` (line 9224): cgroup attach calling `sched_move_task`
  - `cpu_cgroup_css_online()` (line 9159): calls `scx_tg_online`
  - `cpu_cgroup_css_offline()` (line 9182): calls `scx_tg_offline`
  - `sched_change_begin()` (line 10795): dequeue-before-modify pattern

- **`kernel/sched/sched.h`**: Scheduler internals
  - `sched_change` guard definition (line 4042)
  - Lock ordering documentation

- **`kernel/bpf/helpers.c`**: BPF timer implementation
  - `bpf_timer_cb()` (line 1159): Timer callback in softirq, no scheduler locks
  - `bpf_timer_start()` (line 1417): Timer arming with CPU pinning support
  - Comment at line 1173: "runs in hrtimer_run_softirq. It doesn't migrate..."

- **`kernel/time/hrtimer.c`**: High-resolution timer infrastructure
  - `hrtimer_run_softirq()` (line 1848): per-CPU softirq processing
  - `get_target_base()` (line 215): CPU selection for timer (pinned vs. migrated)

- **`kernel/cgroup/cgroup.c`**: Cgroup core
  - `cgroup_mutex` (line 91): Global cgroup serialization
  - `cgroup_migrate_execute()` (line 2695): Migration commit with `css_set_lock`
  - `cgroup_attach_task()` (line 3022): "Call holding cgroup_mutex"
  - `online_css()` (line 5728): cgroup online under `cgroup_mutex`
  - `offline_css()` (line 5752): cgroup offline under `cgroup_mutex`

- **`kernel/cgroup/cpuset.c`**: Cpuset implementation
  - `cpuset_update_tasks_cpumask()` (line 1176): Per-task cpumask update
  - Uses `set_cpus_allowed_ptr()` which calls `task_rq_lock()` per task

- **`include/linux/sched/ext.h`**: SCX definitions
  - `enum scx_kf_mask` (line 120): Kfunc permission masks
  - `SCX_KF_UNLOCKED = 0`: sleepable, no rq lock
  - `SCX_KF_ENQUEUE = 1 << 2`: enqueue/select_cpu context
  - `SCX_KF_REST = 1 << 4`: other rq-locked operations

### External References

- [sched_ext upstream documentation](https://www.kernel.org/doc/html/next/scheduler/sched-ext.html)
- [sched_ext overview](https://sched-ext.com/docs/OVERVIEW)
- [sched_ext source on GitHub](https://github.com/torvalds/linux/blob/master/kernel/sched/ext.c)
- [Bootlin Elixir cross-reference for ext.c](https://elixir.bootlin.com/linux/v6.15.4/source/kernel/sched/ext.c)
- [BPF timer/workqueue implementation](https://eunomia.dev/tutorials/features/bpf_wq/)
- [hrtimer documentation](https://www.kernel.org/doc/html/latest/timers/hrtimers.html)

### Simulator Source Files

- `crates/scx_simulator/src/engine.rs`:
  - `EventKind` enum (line 212): Event classification
  - `EventKind::cpu()` (line 263): Global vs per-CPU classification
  - `group_events_by_cpu()` (line 287): Event partitioning
  - `handle_task_wake()` (line 2137): TaskWake processing
  - `handle_timer_fired()` (line 1446): TimerFired processing
  - `handle_cgroup_migrate()` (line 1777): CgroupMigrate processing
  - `handle_cgroup_create()` (line 1909): CgroupCreate processing
