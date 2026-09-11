# Arbitrary virtual topologies in scxsim: what it can express, and what it cannot

**Date:** 2026-09-11
**Base:** `integration@426d85e` (this work is the delta on top of it)
**Issue closed:** mb `sim-dox34` — *"multi-NUMA-node topologies confine all
execution to node 0"*
**Code:** `crates/scx_simulator/src/safe/topology.rs`,
`crates/scx_simulator/src/safe/layered_xnuma.rs`,
`schedulers/layered/wrapper.c`, `scxtest/scx_test_cpumask.c`
**Tests:** `crates/scx_simulator/tests/numa_topology.rs` (16),
`crates/scx_simulator/tests/layered_xnuma.rs` (7),
`crates/scx_simulator/tests/layered_large_topology.rs`
(`multi_node_topologies_reach_every_node`)

Every claim below is asserted by a named test, or marked **UNCOVERED** /
**STUBBED** where it is not. Nothing here is asserted from a reported
configuration value: coverage claims are measured from the CPUs the engine
recorded a `TaskScheduled` on.

---

## 1. The bug that had to go first

With `nr_nodes > 1`, scx_layered under scxsim ran on node 0 only.
`cpus_that_ran` was exactly `nr_cpus / nr_nodes`, always the lowest ids, at 2 /
4 / 8 nodes and from 32 CPUs up.

Two independent defects composed, and each is a real defect on its own.

### D1 — every task was born on CPU 0

`TaskDef::initial_cpu()` returned `CpuId(0)` for any task without an explicit
cpumask, and `SimTask::new` seeded `prev_cpu` from it. scx_layered's
`pick_idle_cpu()` searches the task's LOCAL node (`src_nid =
prev_cpuc->node_id`) and crosses a NUMA boundary only through the cross-NUMA
gate. Every task therefore had `src_nid == 0` forever.

That is *correct scheduler behaviour on a machine no fork ever produces*. A
384-CPU dual-socket box does not fork 768 threads from CPU 0.

### D2 — the cross-NUMA gate was never written

`layers[l].node[src].xnuma[dst].rate` and `.xnuma_is_mig_src` are written
**only** by userspace, from `refresh_xnuma()` in upstream
`scx_layered/src/main.rs`, on every control-loop iteration. scxsim's layered
control loop did not port it, so both fields sat at their BSS zero. In
`main.bpf.c`:

* `xnuma_gate()` reads `if (!bucket->rate) return false;` — *zero budget →
  deny*;
* `pick_idle_cpu()` skips its entire remote-node proximity walk unless
  `xnuma_is_mig_src(layer, src_nid)`;
* `try_consume_layer()` skips any LLC on another node under the same two
  conditions.

So the zero was not "no policy". It was *cross-NUMA migration is permanently
forbidden, in both directions, on every layer* — placement AND consumption.
The diagnostic that localised it: with 2 nodes, node-1 CPUs made 31 428
`scx_bpf_dsq_move_to_local` calls and succeeded 0 times.

### Measured, before and after

`layered_large_topology.rs::multi_node_topologies_reach_every_node`, the
inversion of the old `known_gap_multi_node_confines_all_work_to_node_zero`,
at the shapes the issue was filed against:

| shape | `cpus_that_ran` before | after |
|---|---|---|
| 64 CPUs, 2 nodes | 32 | 64 |
| 64 CPUs, 4 nodes | 16 | 64 |
| 384 CPUs, 2 nodes | 192 | 384 |

`numa_topology.rs` extends this to 32/64/128 CPUs at 2/4/8 nodes, with and
without the control loop, and to the 384-CPU dual socket with SMT2 and 24
EPYC-like CCXs (`the_384_cpu_dual_socket_machine_runs_on_both_sockets`:
192 CPUs on each socket, not 384 on one).

### Scope of the defect: layered only

`cosmos_reaches_every_node_too` runs scx_cosmos on 8 CPUs at 2 / 4 / 8 nodes
**under `ForkPlacement::FirstCpu`** — the pre-fix "everything born on CPU 0"
placement — and every CPU still runs. Cosmos reaches NUMA through
`scx_bpf_cpu_node()`, a per-node shared DSQ and the node-scoped idle kfuncs,
none of which touch `xnuma`. So sim-dox34 was specific to scx_layered, not a
general engine defect, and that test is deliberately pinned to the adversarial
placement to keep saying so.

---

## 2. What a topology can now express

`MachineTopology` is a per-CPU `(core_id, llc_id, node_id)` assignment. It is
built once and given to **both** the engine (`ScenarioBuilder::topology`) and
the scheduler (`DynamicScheduler::layered_for_topology`). Before this, the
shape was described twice, independently, with nothing checking that the two
descriptions agreed — and the scheduler's copy carried a NUMA partition the
engine did not model at all.

| Dimension | Expressible | Verified by |
|---|---|---|
| NUMA nodes / sockets | yes, any count up to the ceilings in §4 | `multi_node_static_allocation_reaches_every_cpu` |
| Sub-NUMA clustering (NPS2/NPS4, SNC) | yes — several nodes per socket is just a higher node count over the same LLCs | `sub_numa_clustering_reaches_every_node` |
| LLCs (CCX / ring stop / cluster) | yes, any partition of CPUs | `the_engine_and_the_scheduler_agree_about_the_machine` |
| SMT | yes, any threads-per-core, including >2 | `topology.rs::four_threads_per_core_wrap_in_core` |
| Asymmetric nodes (different CPU counts) | yes | `an_asymmetric_dual_socket_machine_runs_on_all_of_it` |
| Asymmetric LLCs (different core counts) | yes | same |
| SMT on part of the machine only | yes | `asymmetric_smt_runs_statically_and_is_refused_by_the_control_loop` |
| Where tasks are born | yes, `ForkPlacement` | `fork_placement_puts_tasks_where_it_says` |
| Cross-node migration cost | yes, one flat penalty | `crossing_a_node_costs_more_than_crossing_an_llc` |

`MachineTopology::uniform(nr_cpus, cpus_per_llc, nr_nodes, threads_per_core)`
reproduces exactly the division the old four-number APIs did, so an existing
scenario keeps its shape. Asymmetric machines go through
`MachineTopology::from_cpus`.

### Fork placement, and why the default is what it is

`ForkPlacement::Auto` (the default) is `FirstCpu` on a single-node machine and
`RoundRobinNodes` once there is more than one node. Every scenario in the tree
predates the knob and declares no nodes, so the default is a byte-for-byte
no-op for all of them — asserted by
`auto_fork_placement_changes_nothing_on_a_single_node_machine`, which compares
whole-trace event counts.

`RoundRobinCpus` exists and is **not** the default on purpose. Dealing task `i`
onto CPU `i % nr_cpus` would make `cpus_that_ran == nr_cpus` true *by
construction*: a scheduler that never migrated anything would pass every
coverage assertion in this work. `RoundRobinNodes` deals across nodes only, so
all the within-node placement is still the scheduler's job, which is what the
coverage assertions then actually measure.

---

## 3. What a topology cannot express — REFUSED at construction

These panic with a message naming the offending CPU, rather than being
silently mismodelled. Each has a `#[should_panic]` test in `topology.rs`.

| Shape | Why refused |
|---|---|
| An SMT core spanning two LLCs | No such hardware. Both scx_layered and scx_cosmos index a CPU's LLC through its core. |
| An SMT core spanning two nodes | Same. |
| An LLC spanning two nodes | `llc_numa_id_map[llc]` is a single value in scx_layered's ABI. A split LLC has no representation to publish. |
| Sparse / non-dense core, LLC or node ids | Every consumer indexes fixed-size arrays by these ids; a gap reads another entry's slot. |
| More nodes than LLCs | `MachineTopology::uniform` refuses; the C `layered_set_topology()` *clamps* silently, which is how the engine and the scheduler end up on different machines. The Rust side names the knob to move instead. |
| Offline CPUs at construction | Bring the machine up whole and use `HotplugEvent` during the run. |

---

## 4. Ceilings

| Limit | Value | Where |
|---|---|---|
| CPUs | 512 | `NR_CPUS` (`scxtest/kern_types.h`), `LAYERED_MAX_SIM_CPUS` (`schedulers/layered/wrapper.c`), scx_layered's own `MAX_CPUS`. All three at 512 simultaneously; `the_ceiling_is_512_and_it_refuses_rather_than_truncating` pins that 513 is refused with a message naming the constant, not silently truncated. |
| LLCs | 64 | `MAX_LLCS`, scx_layered `intf.h` |
| NUMA nodes, scheduler side | 32 | `MAX_NUMA_NODES`, scx_layered `intf.h` |
| NUMA nodes, substrate side | 64 | `MAX_SIM_NUMA_NODES`, `scxtest/kern_types.h`. Above layered's 32, below scx_cosmos's `MAX_NODES = 1024`. Bounds a per-node `struct cpumask` array. |

`DynamicScheduler::layered_for_topology` **asserts** that the node count the
wrapper published equals the one requested, rather than adopting a clamped
value. A silently reduced node count is precisely the divergence the shared
`MachineTopology` exists to prevent.

---

## 5. Expressible but NOT MODELLED — the limits left in place

These are real gaps. They are listed as UNCOVERED (the simulator has no model)
rather than STUBBED (the simulator has a fake that answers plausibly), because
that distinction is the one that matters when reading a result.

### 5.1 UNCOVERED — no memory model at all

`node_id` costs a task exactly one thing: extra latency when it migrates
across a node boundary (`OverheadConfig::cross_node_migration_penalty_ns`).
There is:

* no per-node memory, no page placement, no first-touch, no migration of
  pages;
* no memory bandwidth or capacity per node;
* no distinction between a task's compute node and its memory node.

**Consequence:** a scheduler policy whose *point* is memory locality cannot be
evaluated for the thing it optimises. It can be evaluated for whether it
*makes the placements it intends to*, which is what the tests here do.

### 5.2 UNCOVERED — no distance matrix

Inter-node cost is one flat number. There is no ACPI SLIT, so on a 4-node
machine the near hop and the far hop cost the same. Real 4-socket and
NPS4 machines have non-uniform inter-node distance.

**Consequence:** any policy that ranks remote nodes by distance will be
exercised (scx_layered's `node_ctx->prox_map` is built and walked) but its
*ordering* has no cost consequence, so a wrong ordering is invisible in the
timing.

The proximity map itself is built in ascending node id order
(`layered_fill_node_prox_map`), not by distance, because there is no distance
to sort by.

### 5.3 UNCOVERED — the cross-node penalty default is not calibrated

`cross_node_migration_penalty_ns` defaults to 25 000 ns, equal to the
cross-LLC penalty, so a cross-socket migration costs about twice a cross-CCX
one (10 µs base + 25 µs LLC + 25 µs node). **This is a modelling choice, not a
measurement.** Unlike `cross_llc_migration_penalty_ns`, it has not been
calibrated against a traced workload. A scenario whose conclusion depends on
the absolute number must set it explicitly.

### 5.4 UNCOVERED — no heterogeneous cores

No big.LITTLE / P-core-E-core. `has_little_cores` is published `false` and
every core is `CoreType::Big`. `MachineTopology` has no core-type field, so
this is not merely unset — it is inexpressible.

### 5.5 UNCOVERED — no cache hierarchy below the LLC

No L1/L2 sharing relationships, no cache sizes, no cache-pressure model. The
LLC is an identity used for grouping and for a migration penalty, not a
modelled resource.

### 5.6 UNCOVERED — no socket concept beneath the node

A socket presented as several NUMA nodes (NPS2/NPS4, SNC) is expressed by the
node count alone. Nothing records that nodes 0 and 1 share a package while
node 2 does not. A scheduler that wanted to prefer a same-package remote node
could not learn that here.

### 5.7 The control loop needs a uniform machine

`layered_enable_control_loop` **refuses** an asymmetric machine, with a message
saying why. Upstream's core-growth allocator is reached through
`Topology::simulated(nr_cpus, cpus_per_llc, nr_nodes, threads_per_core)`, which
takes exactly two scalars that an asymmetric machine does not have. Averaging
one out would be a fake approximation of upstream's allocator, which the
No-Stub Rule forbids; refusing is the honest option. Asymmetric machines run
under the static (Tier-2) allocation, which
`asymmetric_smt_runs_statically_and_is_refused_by_the_control_loop` exercises
on both halves.

### 5.8 FIXED, was STUBBED — the node-scoped idle kfuncs

`scx_bpf_pick_idle_cpu_node`, `scx_bpf_pick_any_cpu_node`,
`scx_bpf_get_idle_cpumask_node` and `scx_bpf_get_idle_smtmask_node` all took
`int node __attribute__((unused))` and answered machine-wide — silently, with
no marker and no `DANGER TODO`. They could not have done better before the
engine had a NUMA model.

They are now node-scoped, driven from a partition the engine publishes into the
substrate at run setup (`scx_test_set_cpu_node`), and asserted by
`the_node_scoped_idle_kfuncs_are_node_scoped`. When no topology has been
published they fall back to the machine-wide answer rather than to an empty
mask, so a caller with no scenario keeps working.

scx_layered does **not** call them — it uses `nodec->cpumask` and
`lookup_layer_node_cpumask` — so this was never the mechanism behind
sim-dox34, and fixing it is a separate correctness win that no other test in
the tree would have caught.

---

## 5b. What it costs now, measured

All from `layered_large_topology.rs`, run one test per process on this dev box
(`--release --features standalone -- --ignored --nocapture`). The `#[ignore]`
sweeps are measurements, not assertions; the wall-clock column is a shared
machine and moves.

### 384 CPUs, 768 tasks, 200 ms simulated — `sweep_384_shapes`

| shape | LLCs | nodes | SMT | `ran_on` | LLCs used | events | wall |
|---|---|---|---|---|---|---|---|
| flat | 1 | 1 | 1 | **384/384** | 1/1 | 101 105 | 0.088 s |
| dual socket | 24 | 2 | 2 | **384/384** | 24/24 | 103 252 | 0.138 s |
| dual socket, no SMT | 24 | 2 | 1 | **384/384** | 24/24 | 103 252 | 0.132 s |
| NPS2 | 24 | 4 | 2 | **384/384** | 24/24 | 109 400 | 0.131 s |
| 8 nodes, 8c LLC | 48 | 8 | 2 | **384/384** | 48/48 | 115 249 | 0.194 s |

**The 33x event blow-up in mb sim-dox34 is gone.** The issue measured 101 k
events for 1 node and **3.39 M** for 2 nodes at this exact point — half the
machine idle while the other half churned. The 1-node figure reproduces
exactly (101 105); the 2-node figure is now 103 252, i.e. 1.02x rather than
33x. So the confinement was also the dominant wall-clock cost of any
multi-node simulation, and removing it removes that too.

### Growing the simulated duration — `sweep_duration_at_384`

`vs_vm` compares against a ktstr-style VM test's measured baseline of ~7 s
flat plus the declared duration.

| sim_ms | flat wall | dual-socket wall | flat events | dual events | `ran_on` (both) |
|---|---|---|---|---|---|
| 50 | 0.057 s | 0.048 s | 34 190 | 35 509 | 384/384 |
| 200 | 0.092 s | 0.125 s | 101 105 | 103 252 | 384/384 |
| 1000 | 0.281 s | 0.533 s | 476 999 | 481 222 | 384/384 |
| 5000 | 1.270 s | 2.610 s | 2 347 117 | 2 348 941 | 384/384 |

The dual-socket line is now readable as the cost of the NUMA *shape* — 24
LLCs instead of 1, and real cross-node traffic — at roughly 2x the flat line's
wall clock for the same work. Before the fix it was not readable at all, and
the file said so.

### The diagnostics, post-fix

* `diag_node_confinement_matrix`: every row is `ran_on=64/64`, and the 2-node
  event counts (9199 / 9167 / 9125) now sit alongside the 1-node ones
  (9118 / 9110 / 9076) instead of at 27 k–49 k.
* `diag_where_does_node1_work_go`: both the 1-node and 2-node arms report 64
  enqueue-from CPUs, all 8 per-LLC DSQs inserted into and consumed, and 64
  consumer CPUs. The 2-node arm previously reported 32 / 4 / 32.

---

## 6. Two more things worth knowing

### 6.1 `tests/topology.rs::assert_spread` cannot detect node-0 confinement

It accepts `used.len() >= nr_cpus / 2`, which is *exactly* the value a 2-node
confinement produces. That test suite passed throughout sim-dox34. The
assertions in `numa_topology.rs` require every CPU, and name the pre-fix value
in the failure message so the next occurrence is recognisable on sight.

### 6.2 The xnuma policy is upstream's code, not a re-implementation

`xnuma_check_active` and `xnuma_compute_rates` are vendored token-for-token
from upstream `main.rs` into `safe/layered_xnuma.rs` — the same treatment
`alloc.rs`'s `largest_remainder` gets in `safe/layered_alloc.rs`, and for the
same reason: they carry policy, and a re-implementation would agree on the
easy cases and diverge on the corner cases. They cannot be `#[path]`-included
the way `alloc.rs` is, because they sit in the middle of a 5000-line file that
pulls in the generated BPF skeleton.

`tests/layered_xnuma.rs` re-reads upstream at test time and fails if either
body, either constant, or either of the two `config.rs` defaults has drifted —
plus a meta-test that the drift guard can actually fail, so it is not vacuous.

The surrounding `refresh_xnuma()` glue *is* reproduced (it is a method on
upstream's `Scheduler` that reaches into the libbpf skeleton), in
`LayeredControl::refresh_xnuma`, with upstream's branch structure annotated
line by line — including the two behaviours easiest to lose in a rewrite: the
`nr_nodes <= 1` early return that writes nothing, and the "gating off" branch
resetting the hysteresis state to all-false so a later switch back to gating
starts closed.

Its input, `layer_duty_sum`, is accumulated by the real BPF
`layered_stopping()`; scxsim only sums it per node, exactly as upstream's
`read_layer_node_duty_raw()` does. `the_gates_input_counts_runnable_time_not_cpu_time`
asserts it exceeds CPU time under a 2-hogs-per-CPU load, which is the property
that distinguishes it from `layer_usages` and would fail if the probe were
wired to the wrong field.
