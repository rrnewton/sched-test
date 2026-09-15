---
title: 'scxsim: sim_arena_mark_persistent() ratchets the arena floor on every scheduler load, exhausting the 32 MiB arena'
status: open
priority: 1
issue_type: bug
created_at: 2026-09-10T21:15:25.789164718+00:00
updated_at: 2026-09-10T21:15:25.789164718+00:00
---

# Description

csrc/sim_arena.c:

  void sim_arena_mark_persistent(void)
  {
      sim_arena_floor = sim_arena_offset;
  }

Its own declaration in sim_arena.h says 'Called by the engine immediately after <name>_setup(). Idempotent.' **It is not idempotent.** It assigns unconditionally, so the floor is raised to the current watermark on EVERY call.

unsafe_impl/ffi.rs::load() calls it after every <prefix>_setup(), i.e. once per DynamicScheduler construction -- not once per dlopen. The .so is cached, but <prefix>_setup() re-runs and re-allocates on each construction, so:

  load #1: setup allocates A bytes -> floor = A
  load #2: setup allocates A more  -> floor = 2A   (load #1's A bytes are now unreachable garbage below the floor)
  load #N: floor = N*A

sim_arena_reset() can never reclaim below the floor, so a process that constructs many schedulers monotonically consumes SIM_ARENA_SIZE (32 MiB, sim_arena.h:46) until bpf_cpumask_create() returns NULL and the scheduler fails to init.

SYMPTOM, as seen: 'scheduler init failed with rc=-12' (engine.rs:1852) and 'init_task failed for pid=42 rc=-12'. ENOMEM, in a test that passes in isolation. It is order-dependent and looks like cross-test contamination, which is a misleading place to start debugging.

REPRO on this branch: crates/scx_simulator/tests/layered_large_topology.rs -- every one of its 10 #[ignore]d measurement tests passes when run one-per-process, and 3 of them fail with rc=-12 when the same set is run in a single process via '-- --ignored --test-threads=1'. Verified both ways.

WHY IT HAS NOT BITTEN BEFORE: existing test files construct few schedulers per binary. The layered topology sweeps construct dozens, several at 384 CPUs, so they reach the ceiling.

WHY THE OBVIOUS FIX IS WRONG: making it idempotent by only setting the floor when it is currently 0 would leave load #2's setup allocations ABOVE the floor, where the next sim_arena_reset() zeroes them -- which reintroduces mb sim-hfvmf exactly (tickless's primary-CPU bpf_cpumask zeroed, is_primary_cpu() false forever, init_timer() never reached). The floor exists for that reason.

LIKELY CORRECT FIX: rewind the arena to 0 BEFORE re-running <prefix>_setup() on a reload, then mark. That keeps the header's determinism guarantee (a run's allocations start at the same address every time) and stops the ratchet. It is an engine change in ffi.rs::load() and needs a full validate.sh plus a tickless/cosmos check for sim-hfvmf regression, which is why it is filed rather than folded into the topology work.

# Acceptance Criteria

A single test process can construct and run many schedulers without ENOMEM; the layered_large_topology measurement tests pass under a single 'cargo test -- --ignored --test-threads=1' invocation. sim-hfvmf does not regress (tickless reaches init_timer).
