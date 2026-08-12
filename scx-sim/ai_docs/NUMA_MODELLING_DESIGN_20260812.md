# Minimal NUMA modelling for scx-sim — design and scope

Date: 2026-08-12. Task: `design-numa-modelling-scope` (design only, nothing
implemented). Goal: `goal-simulated-numa-domains`.

**Sanitisation note.** The motivating production incident is cited in the tg
task `identify-layered-numa-sev`, not here. Both `sched-test` (via the
`rrnewton` mirror) and the parent harness repo are publicly readable, and the
handling constraint on that task forbids committing SEV numbers or internal
specifics to either. This document describes only the upstream-public
mechanism (`scx_layered`'s cross-NUMA token-bucket gate, upstream PR #3378,
already an ancestor of our scx pin) and refers to the incident generically.

---

## 1. What the engine models today

| concept | representation | who sets it | does it affect behaviour? |
|---|---|---|---|
| CPU | `SimCpu` in `safe/cpu.rs`, indexed by `CpuId` | engine | yes |
| SMT sibling | `SimCpu.siblings: Vec<CpuId>`, from `Scenario.smt_threads_per_core` | engine (`engine.rs` ~1400) | yes |
| LLC | `SimCpu.llc_id: u32`, from `Scenario.cpus_per_llc` | engine (`engine.rs:1413`) | **yes** — see below |
| NUMA node | *nothing* | — | no |

The LLC path is the one that matters, because it is the template:

```
Scenario.cpus_per_llc
  -> engine.rs:1413   cpus[i].llc_id = i / cpus_per_llc
  -> engine.rs:4710   cross_llc = migrated && llc_id(prev) != llc_id(new)
  -> engine.rs:4813   floor += overhead.cross_llc_migration_penalty_ns   (default 25 µs)
```

The penalty is added to the wakeup-latency floor in `start_running`, which
pushes `cpus[cpu].local_clock` forward. That is exactly the owner's framing:
topology manifests as **elapsed time**, not as a memory-system simulation.
There is no cache model anywhere, and none is needed.

**Nodes have no representation at any layer.** `Scenario` has `nr_cpus`,
`smt_threads_per_core`, `cpus_per_llc` — and no node field.

### The layering violation worth naming

`scx_bpf_cpu_node()` is a *kernel* kfunc (`scheds/include/scx/common.bpf.h:89`).
In scx-sim it is implemented **inside a scheduler wrapper**:
`schedulers/cosmos/wrapper.c:312` defines its own `scx_bpf_cpu_node()` reading
a wrapper-local `cpu_node_map`, populated by `cosmos_configure_numa()` (reached
from Rust via `DynamicScheduler::cosmos_with_numa`, `ffi.rs:1212`) using
sequential grouping. The engine never sees this topology and the `Scenario`
never sets it.

Under "Don't Model the Scheduler — Model the Kernel", the test is: in
production, who owns the CPU→node map? The kernel. So it belongs in the engine's
BPF substrate, derived from the `Scenario`, and every scheduler should see the
same answer. `cosmos_set_cpu_capacity` and `cosmos_enable_smt_siblings` are the
same shape — per-scheduler FFI setters the *caller* must manually keep
consistent with the `Scenario`.

This matters for scope: the cheapest NUMA change is also the one that pays down
this debt, and doing it any other way (a second per-wrapper node map for
layered) actively deepens it.

---

## 2. The minimum viable model

Two **independent** slices. Slice A is the SEV-critical one and can land alone.

### Slice A — node membership + per-node observability

Everything here is placement and elapsed time. No distance, no cost.

1. `Scenario.cpus_per_node: u32` (0 = single node), mirroring `cpus_per_llc`
   exactly, including the "nr_cpus must be divisible" validation at
   `scenario.rs:1463`, and a `.cpus_per_node(n)` builder method.
2. `SimCpu.node_id: u32`, assigned next to `llc_id` in `engine.rs:1413`.
   Invariant to assert at build time: **a node is a union of whole LLCs**
   (`cpus_per_node % cpus_per_llc == 0` when both are set). Real hardware never
   splits an LLC across nodes, and a scheduler that assumes it doesn't will
   misbehave in ways that are our bug, not its.
3. Engine-owned `scx_bpf_cpu_node()` in `csrc/sim_bpf_stubs.c`, reading the
   engine's map, plus `nr_node_ids`. Delete cosmos's private one and
   `cosmos_configure_numa`; `cosmos_with_numa(nr_cpus, nr_nodes)` becomes
   `Scenario::builder().cpus_per_node(nr_cpus / nr_nodes)`.
4. `Trace::node_busy_ns(&self, node_of: impl Fn(CpuId) -> u32) -> Vec<TimeNs>`
   — or simpler, expose the node map on the trace and add
   `Trace::cpu_busy_ns(CpuId)`. **This needs no engine change at all**: every
   `TraceEvent` already carries `cpu` and `time_ns`, and `total_runtime()`
   already walks `TaskScheduled` intervals. Grouping that same walk by CPU and
   folding through the node map yields per-node utilisation.

Item 4 is the whole point. It is what turns "the scheduler made a cross-node
decision" from invisible into assertable.

### Slice B — cross-node migration cost

1. `OverheadConfig.cross_node_migration_penalty_ns` (+ `SCX_SIM_CROSS_NODE_PENALTY_NS`).
2. In `start_running`, extend the existing `cross_llc` computation to a
   three-level ladder: same-LLC → cross-LLC → cross-node, applying the largest
   applicable penalty rather than summing (a cross-node migration *is* a
   cross-LLC migration; charging both double-counts the same cache refill).

That is roughly 15 lines in `engine.rs` plus a config field, in a code path
that already exists and is already exercised.

### What is deliberately NOT in scope

- **No distance matrix.** A full N×N node-distance table (SLIT/`numa_distance`)
  buys nothing until some scheduler reads it. Two nodes are symmetric; four-node
  asymmetry is a later increment, and the design does not preclude it —
  `cross_node_migration_penalty_ns` becomes `distance[a][b]`-scaled when it is
  needed.
- **No memory model.** No page placement, no bandwidth, no first-touch. The
  owner's constraint is that scx-sim owes elapsed time and scheduler state only.
- **No autonuma / page migration.**

---

## 3. What the six schedulers would see

Grepped against our current scx pin:

| scheduler | NUMA use in its BPF source | wrapped in scx-sim? | effect of Slice A |
|---|---|---|---|
| `layered` | heaviest — cross-NUMA gate, node masks, per-node duty sums | **no** (in flight on `feat/layered-support`) | the target; see §6 |
| `cosmos` | `__COMPAT_scx_bpf_cpu_node`, `nr_node_ids` | yes | real topology instead of a private map; behaviour becomes *derived* |
| `lavd` | `cpdom_ctx.numa_id` | yes | today set to `0` by `lavd_setup`, or to the domain index by `lavd_setup_multi_domain` — i.e. harness-invented. Slice A lets it be derived from the scenario |
| `mitosis` | none | yes | none |
| `tickless` | none | yes | none |
| `simple` | none | yes | none |

So of six schedulers, exactly **two** would behave differently today, and the
one that matters most is not wrapped yet. That is a scoping fact worth stating
plainly: Slice A is cheap, but its payoff is gated on the layered wrapper
landing.

---

## 4. Existing tests that would change

Nothing should break. Several tests should get *stronger*, and two are currently
misleading:

- `cpu_migration.rs:243 test_cross_numa_migration_cosmos` — computes
  "cross-node" by dividing CPU id by `cpus_per_node` **in the test**, then
  asserts `cross > 0`. The nodes are a cosmos-wrapper fiction the engine knows
  nothing about, so this test passes identically whether NUMA does anything or
  not. Slice A lets it assert against engine-owned node ids; Slice B lets it
  assert a cost.
- `cosmos_llc.rs:567 test_cosmos_cross_llc_migration_cost` — named `..._cost`,
  but runs `.instant_timing()`, which is `.noise(false).overhead(false)`. The
  `cross_llc_migration_penalty_ns` path it claims to exercise is **disabled**;
  it only counts migrations. This is a pre-existing bug in an existing test, in
  the same family as the other "asserts nothing" findings this session. Worth
  fixing on its own merits, and it is a direct warning for how *not* to write
  the NUMA cost test.
- `topology.rs` sweeps (`test_cosmos_numa_node_sweep`,
  `test_cosmos_numa_per_node_affinity`) and `cosmos.rs:125 test_numa_topology`
  call `cosmos_with_numa`; they need mechanical porting to `.cpus_per_node()`.
- `select_cpu_correctness.rs:325 test_select_cpu_numa_awareness`,
  `cosmos_llc.rs:312 test_cosmos_numa_domain_load_balance` — same porting.

Estimate: ~6 test call-sites, mechanical.

---

## 5. Proposed new tests

### A. Placement (Slice A) — fails on bad cross-node placement, passes on good

Two nodes, and a workload whose total demand fits comfortably inside one node's
CPU count but is *offered* to both. Assert on per-node utilisation:

```
imbalance = |busy(node0) - busy(node1)| / (busy(node0) + busy(node1))
```

- PASS: a scheduler that balances across nodes keeps `imbalance` below a
  threshold derived from the run (see below).
- FAIL: a scheduler that refuses cross-node work leaves one node saturated and
  the other idle — `imbalance` approaches 1.0.

The threshold must **not** be a hand-tuned constant. Derive it from the same
run: compute the imbalance a work-conserving placement would produce given the
observed total busy time and the node CPU counts, and require the measured
imbalance to be within a stated factor of it. A test that hardcodes
`assert!(imbalance < 0.3)` is a magic number that will be tuned until green.

### B. Cost (Slice B) — cross-node migration is observably slower

The A/B shape that `test_cosmos_cross_llc_migration_cost` should have used:
run the *same* scenario twice with `cpus_per_node` set so that the identical
migration is intra-node in one run and cross-node in the other, with overhead
**enabled** (not `.instant_timing()`), and assert the cross-node run's wakeup
latency is higher by approximately `cross_node_migration_penalty_ns`. Assert on
the *difference between two runs*, never on an absolute latency, so the test is
insensitive to unrelated overhead retuning.

---

## 6. Reconciliation with the incident we actually want to reproduce

The `identify-layered-numa-sev` task landed while this was being written. Its
verdict and mine agree, and it sharpens the scope in one important way.

The pathology is **not** "cross-node migration became expensive". It is
"cross-node migration was refused, and the condition to un-refuse it was
unreachable". The observable is one node saturated while the other idles — pure
placement and elapsed time.

Consequences for this design:

- **Slice B is not on the critical path.** You could reproduce that incident
  with zero distance cost. Slice A alone is the cheapest useful step. Slice B
  remains right for the goal in general (cache-affinity pathologies), but it
  should not gate Slice A.
- **Slice A is necessary but not sufficient.** Per that task's analysis, the
  repro additionally needs: a userspace control loop (the gate rates are
  written from userspace Rust into BPF bss every refresh, not computed in BPF);
  the layer growth/allocation pass that produces the `growth_denied` input; per
  layer-per-node duty-cycle accounting; and time-driven token-bucket refill on
  the simulated clock. None of those are NUMA-topology work — they are layered
  wrapper and userspace-loop work, and they belong on the layered goal, not
  this one.
- **Dependency order:** layered wrapper lands → Slice A (engine node topology)
  → userspace control loop → repro. Slice A can start now; it is on the
  critical path and blocks nothing else.

### The false-repro trap, restated as a design constraint

BPF bss is zero-initialised. The moment scx-sim runs layered with `nr_nodes >= 2`
and nothing populates the gate state, the gate reads a zero rate and denies —
producing *exactly* the incident's utilisation fingerprint while running none of
the logic that caused it. A test that asserts only on the utilisation shape
would go green on a scheduler that never executed the interesting code.

This is the `cgroup_bw` `BandwidthManager` antipattern with a new face. Design
constraint: **any repro test must assert on the mechanism** — that rates were
written, that the migration-source flag transitioned, that the growth-denied
input was computed — and not merely on the utilisation shape. Test A above is a
legitimate *engine* test of node placement; it is explicitly **not** a repro of
the incident, and must not be labelled as one.

---

## 6b. Narrowed target: is Slice A sufficient? No — and the obvious success criterion is a trap

The target is now pinned: the three cross-NUMA functions our coverage run lists
as unreachable — `xnuma_gate`, `xnuma_gate_charge`, `xnuma_bucket_refill` — are
exactly where the incident lives. Read against the source, here is what each
increment actually buys.

### Why they are unreachable today — first-order cause is the node count

`main.bpf.c:1170`:

```c
static bool xnuma_gate(u32 layer_id, u32 src_nid, u32 dst_nid, s64 cost)
{
        if (src_nid == dst_nid || src_nid >= MAX_NUMA_NODES ||
            dst_nid >= MAX_NUMA_NODES)
                return true;
```

With one node, `src_nid == dst_nid` always, so the body is dead code. The
userspace side agrees: `refresh_xnuma()` opens with `if nr_nodes <= 1 { return; }`.
**The functions are unreachable because scx-sim has one node.** Two-node
topology is therefore genuinely necessary, and it is the first thing that moves.

### But two-node topology alone produces a FALSE repro

Follow the same function down with zero-initialised BPF bss, which is what you
get if nothing writes the gate state:

| line | behaviour with zero bss | consequence |
|---|---|---|
| `if (bucket->rate == (u64)-1) return true;` | not taken (rate is 0) | — |
| `if (!bucket->rate) return false;` | **taken** | every cross-node consideration DENIED |
| `xnuma_bucket_refill(...)` below it | **never reached** | stays at 0 coverage |
| `xnuma_gate_charge` (`:1201`) | early-returns on `!bucket->rate` | no-op, stays at 0 coverage |
| site `:2628` `(!xnuma_is_mig_src(..) \|\| !xnuma_gate(..))` | `is_mig_src` false → `\|\|` short-circuits | `xnuma_gate` **not even called** at this site |

Net result of Slice A alone: one node saturates, the other idles — the exact
fingerprint — with `xnuma_gate` newly covered at two of three sites,
`xnuma_bucket_refill` and `xnuma_gate_charge` still at zero, and **none of the
logic that caused the incident having run.** The coverage number would go up,
which makes it look like progress.

**So the proposed success criterion — "pathology appears with the gate defaulted
ON, vanishes with it OFF" — is satisfiable by a stub.** "OFF" in the real
scheduler means userspace writing `rate = u64::MAX` (`main.rs:4169`), which
early-returns `true` and *also* never runs refill or charge. So a contrast
between "zero bss" and "write U64_MAX" gives a perfect switchable
pathology while executing neither of the two functions we care about. That
contrast must not be accepted as the deliverable.

### The discriminator that makes it honest

`xnuma_bucket_refill` and `xnuma_gate_charge` are **unreachable under the stub
by construction** — both are gated behind `bucket->rate` being nonzero and not
`U64_MAX`. So:

> **Acceptance test: the repro is real only if `xnuma_bucket_refill` and
> `xnuma_gate_charge` both execute during the run.** Assert on that directly,
> not on the utilisation shape.

That is a mechanism assertion the false repro provably cannot pass, and it
happens to be exactly the coverage signal that started this.

### What is actually required to get there

To make `rate` finite and nonzero, someone must run what userspace runs:

| input | where it comes from | status in scx-sim |
|---|---|---|
| `rate` per (layer, src, dst) | `xnuma_compute_rates(duty_sums, nr_node_cpus)` — a **pure function**, `main.rs:1927`, unit-tested upstream | needs porting/calling |
| `xnuma_is_mig_src` | `xnuma_check_active(duty_sums, allocs, thresholds, growth_denied, prev)` — also **pure**, `main.rs:1872` | needs porting/calling |
| `duty_sums` | folded per node from `cpu_ctxs[cpu].layer_duty_sum[layer]`, which **the BPF side already accumulates** (`main.bpf.c:2952`); userspace only reads, diffs and EWMA-decays it (`main.rs:971`, `:1235`) | data already produced by code scx-sim runs; needs the read + fold + decay |
| `growth_denied` | the layer growth/allocation pass | **missing** — the in-flight layered wrapper holds allocation static, so this pass does not run |
| `scx_bpf_now()` for refill | per-CPU simulated clock | **already provided** — `kfuncs.rs:2440`, already traced. Not a blocker (this corrects an earlier assumption that it needed building) |

Two useful facts fall out. The duty data is already being produced *inside the
BPF that scx-sim executes* — this is a read-and-fold problem, not a modelling
problem. And the two decision functions are pure, so porting them is
transcription of real upstream logic, not invention: No-Stub-compliant by
construction, and divergence from upstream is diffable.

### Two tiers, and only the second is the incident

- **Tier 1 — the machinery, live.** Two-node topology + per-node observability
  + a periodic simulated-clock "userspace step" that reads `layer_duty_sum`,
  folds by node, decays, calls the two pure functions, writes bss. This makes
  all three functions execute for real, with rates derived from measured duty,
  and placement observably changing. **It does not reproduce the incident** — it
  proves the machinery runs. Label it that way.
- **Tier 2 — the incident.** Tier 1 + `growth_denied`, which requires the layer
  growth/allocation pass to actually run. The incident's cause was that the
  gate's opening condition was *unreachable* because `growth_denied` never
  became true; a simulator that cannot compute `growth_denied` can only show
  the gate stuck, which is a different and weaker claim.

Tier 2's extra cost is layered-wrapper work (undoing the static-allocation
divergence), not NUMA-topology work. It should sit on the layered goal.

### Revised answer to the question asked

Plainly: **the minimal model I first proposed is not sufficient to express
one-node-saturated-while-other-idle in a way that means anything.** It is
sufficient to make that *picture* appear, which is worse than not reaching the
target, because the picture is convincing and wrong. What is additionally
needed, in order:

1. two-node topology (Slice A1–A3) — engine, ~1.5 d
2. per-node observability (A4) — trace, ~0.5 d
3. the simulated-clock userspace step porting `refresh_xnuma`'s read/fold/decay
   and the two pure functions — layered wrapper, ~2–3 d
4. `growth_denied` via the real layer growth pass — layered wrapper, **not
   estimated here**; it is the divergence the layered work already has on its
   list, and it is the item most likely to dominate

Slice B (cross-node migration cost) is not required for any of this and should
not gate it.

The No-Stub Rule applies to the design, not just the code.

- **Node membership is declared input, not invented behaviour.** `cpus_per_node`
  is a scenario parameter, exactly like `cpus_per_llc` and
  `smt_threads_per_core`. The engine models the machine; declaring the machine's
  shape is the user's job. That is not a stub — a stub would be the engine
  *inferring* plausible nodes.
- **The consequence is derived, not asserted.** Per-node utilisation is computed
  from the trace of decisions the scheduler actually made. Nothing writes a
  "node imbalance" value; it falls out of where tasks ran. If the scheduler
  balances, the numbers balance.
- **The cost is a kernel cost, not a scheduler cost.** Cache/TLB refill on
  migration is charged by the hardware and the kernel, which is the engine's
  side of the line. It is the same category as the already-accepted
  `migration_penalty_ns` and `cross_llc_migration_penalty_ns`.
- **The one genuine constant is `cross_node_migration_penalty_ns`, and it is
  honest about it.** It is a tunable in `OverheadConfig` alongside a dozen
  peers, env-overridable, with a documented provenance — not a value picked to
  make a test pass. The guard is that no test may assert an absolute latency
  against it; tests assert *differences between two runs* that differ only in
  topology. Calibrating the default against real hardware is a separate
  measurement task, and until it is done the default should be labelled as
  order-of-magnitude.
- **Removing cosmos's private `cpu_node_map` is a de-stubbing.** It replaces a
  scheduler-side reimplementation of a kernel facility with the real one.

---

## 8. Effort estimate

| item | est. | notes |
|---|---|---|
| A1–A2 `cpus_per_node` + `SimCpu.node_id` + validation | 0.5 d | direct copy of the `cpus_per_llc` path |
| A3 engine-owned `scx_bpf_cpu_node` / `nr_node_ids`; delete cosmos's | 1.0 d | the only real unknown — touches the C substrate and one wrapper's map registration |
| A4 per-node utilisation on `Trace` | 0.5 d | no engine change; interval walk + node fold |
| port ~6 existing test call-sites | 0.5 d | mechanical |
| new placement test (§5A) | 0.5 d | most of the cost is deriving the threshold honestly |
| **Slice A total** | **~3 d** | |
| B1–B2 cross-node penalty + three-level ladder | 0.5 d | ~15 lines in an existing path |
| new A/B cost test (§5B) | 0.5 d | |
| fix `test_cosmos_cross_llc_migration_cost` while there | 0.25 d | pre-existing bug, same code path |
| **Slice B total** | **~1.25 d** | |
| **Both slices** | **~4.25 d** | excludes layered wrapper and userspace control loop |

Against the narrowed target (§6b), the path to a *meaningful* repro is:

| step | est. | owner |
|---|---|---|
| two-node topology (A1–A3) | ~1.5 d | this goal |
| per-node observability (A4) | ~0.5 d | this goal |
| simulated-clock userspace step (read/fold/decay + the two pure functions) | ~2–3 d | layered goal |
| `growth_denied` via the real layer growth pass | not estimated | layered goal — likely dominates |

Only the first two belong to this goal. Slice B is not on that path.

Confidence: moderate-to-high on Slice A items 1, 2, 4 and all of Slice B — they
are copies of paths that already work. **Lower on A3**, which is the one place
this could overrun: moving `scx_bpf_cpu_node` into the shared substrate means
every scheduler's `.so` picks it up, and cosmos currently registers its map
through `scx_test_map_register`. If a conflict appears there, A3 could double.
A3 is also the item that can be deferred — Slices A1/A2/A4 deliver engine-owned
nodes and observability on their own, with cosmos temporarily keeping its
private map, at the cost of leaving the layering violation in place for one
more increment.

---

## 9. Open question to check before implementing

Upstream scx PR #3718 reports that `scx_layered` **crashes** under certain NUMA
emulation. Before building a layered NUMA repro on top of this, confirm whether
that failure mode also triggers on scx-sim's harness-supplied node grouping. If
it does, it blocks the repro before any gating logic is reached, and fixing it
becomes a prerequisite rather than a footnote.
