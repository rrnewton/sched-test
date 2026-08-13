# PR #3618: `cpu.max` Unbounded-Wait Reproduction Hypothesis

**Status:** hypothesis recorded before the first simulation run

**Simulator baseline:** `sched-test` `origin/integration` at `1fec4b4`, with
`scx` at `59c30ba`

**Upstream target:** [sched-ext/scx PR #3618](https://github.com/sched-ext/scx/pull/3618),
open as of 2026-08-12

## Claim under test

With a quota that is tight relative to runnable demand, LAVD's real
`cgroup_bw.bpf.c` library repeatedly parks tasks in its backup task queue
(BTQ). The unpatched BTQ is ordered only by scheduler vtime. A compute-heavy
task can therefore accumulate a large vtime and remain behind an ongoing
population of short, low-vtime tasks across replenishment periods. That wait
has no wall-clock bound and can eventually reach the kernel's 30-second
runnable-task-stall watchdog.

This is distinct from an earlier `period_budget` poisoning failure. PR #3618
addresses two mechanisms: pressure-scaled slices and a wall-clock component in
the BTQ key. This experiment primarily targets the latter ordering mechanism.

## Capability check

The baseline can express the causal scenario:

- LAVD links and executes the real `scx/lib/cgroup_bw.bpf.c` library.
- A scenario can assign a finite `cpu.max` directly to the cgroup containing
  the tasks and set LAVD's `enable_cpu_bw` global.
- Task behaviors can mix long CPU-bound phases with short run/sleep cycles and
  delayed starts.
- `LavdBailOnCgroupThrottle` identifies a successful per-PID park by the real
  library; `CbwPutAside`, `CbwDrainBtqBatch`, throttle transitions, and
  replenishment events expose aggregate cgroup/BTQ state; `TaskScheduled`
  identifies subsequent per-PID service.

Two limitations materially shape detection:

1. BTQ drain tracing is aggregate rather than per-PID. Per-task service must
   therefore be reconstructed from a task's successful bail and its next
   `TaskScheduled` event, joined to the aggregate BTQ events.
2. The throttle-aware simulator watchdog deliberately suppresses
   `ErrorStall` while the authoritative library snapshot reports
   `cgx->is_throttled`. That change is correct: legitimate quota throttling is
   not itself scheduler starvation. It also removes the most obvious signal
   for this bug, because the victim is parked precisely while the cgroup is
   throttled.

Detection is consequently **differential by necessity, not preference**. The
primary metric is maximum successful-bail-to-next-schedule latency, joined to
competitor progress, repeated replenishment, and BTQ activity. The absence of
`ErrorStall` is expected and **does not kill the hypothesis**.

## Confirmation criteria

A causal reproduction requires all of the following on the unpatched source:

1. A directly bandwidth-limited cgroup reaches the real library's throttle
   path and has nonzero BTQ activity.
2. A named victim has a successful `LavdBailOnCgroupThrottle` event.
3. Across repeated replenishment/drain opportunities, competing tasks continue
   to receive CPU time while the victim receives no `TaskScheduled` event.
4. The victim's bail-to-next-schedule latency grows to a watchdog-scale value,
   or grows monotonically with workload duration without a finite bound in the
   tested range.
5. A no-`cpu.max` control does not exhibit the wait.
6. With PR #3618's wall-clock BTQ-key patch applied, the same scenario bounds
   ordering delay to approximately one `2^32`-nanosecond epoch (about 4.29 s),
   subject to additional legitimate quota delay. This patch differential is
   the strongest evidence that the wait came from BTQ ordering rather than
   ordinary low priority.

## Falsification criteria

The hypothesis is killed for current scx-sim if any of these holds after a
documented sweep over task mix, quota, concurrency, seed, and duration:

- the simulator cannot naturally enter or observe the real LAVD BTQ path;
- every successfully parked task is serviced promptly across the sweep;
- a long wait appears without the throttle, replenish, and BTQ causal
  fingerprint;
- the same wait appears in the no-`cpu.max` control; or
- the PR #3618 wall-clock-key patch does not reduce the specific differential.

Artificially inserting a task into the parked state would reproduce only the
consequence. It is not evidence that `cpu.max` accounting and normal LAVD
callbacks naturally reach the upstream failure.

## Production corroboration boundary

The pinned 2026-08-05 through 2026-08-12 production query found 34,734 LAVD
`runnable task stall` exits (5.39% of LAVD BpfExit stops). That independently
corroborates urgency, not subtype: the bucket does not prove that every event,
or any particular event, was caused by tight `cpu.max`. A simulator result must
stand on the causal criteria above rather than borrowing causation from that
count.

---

## Result, 2026-08-12: NOT REPRODUCED. The wait is bounded at ~1.98s.

**Verdict against the criteria above: the confirmation criteria are NOT met.**
scx-sim produces a large, causally-attributed, bandwidth-induced wait — but a
**bounded** one, and #3618 is specifically about an *unbounded* wait.

Test: `crates/scx_simulator/tests/pr3618_cpumax_unbounded_wait.rs`.

### What the scenario does produce

Tight `cpu.max` on a directly-limited cgroup, LAVD `enable_cpu_bw=true`, a
compute hog plus repeatedly-waking short victims inside the cgroup, and an
unlimited competitor outside it. Worst bail-to-next-schedule, 600ms window:

| quota | victims | bails | throttled | worst wait | victims never rescheduled |
|---|---|---|---|---|---|
| 10ms/100ms | 1 | 8 | 4 | 190.4ms | 0 |
| 10ms/100ms | 4 | 15 | 3 | 280.2ms | 2 |
| 2ms/100ms | 4 | 5 | 1 | 480.0ms | 5 |
| 1ms/100ms | 8 | 9 | 1 | 480.0ms | 9 |
| 0.5ms/100ms | 8 | 9 | 1 | 480.0ms | 9 |

The **control with no `cpu.max`** shows 0 bails, 0 throttles, 0 wait, and
victims scheduled ~500 times each versus ~100 in the throttled runs. So the
wait is bandwidth-specific, not ordinary contention — that part of the
hypothesis holds, and the causal fingerprint (successful
`LavdBailOnCgroupThrottle`, throttle transitions, replenish records,
competitor still running) is present.

### The measurement artefact that nearly became a false positive

Three different quotas all reported **exactly 480.000ms**. A round number
identical across configurations differing tenfold in quota is not a measured
wait — it is `end_of_run - bail_time` for a victim still parked when the run
ended. The 600ms window minus a bail at 120ms is 480ms.

That is either the bug or an illusion, and the two are distinguishable: an
unbounded wait tracks the observation window, a bounded one converges. Result:

| window | worst wait | % of window |
|---|---|---|
| 2400ms | 1780.2ms | 74.2% |
| 4800ms | 1980.2ms | 41.3% |
| 9600ms | 1980.2ms | 20.6% |
| 19200ms | 1980.2ms | 10.3% |

**It converges, to 1980.2ms exactly, and stops.** Tripling the window past that
point adds nothing. The wait is bounded at ~1.98s.

### What this means

- **Not a #3618 reproduction.** A ~2s bounded worst case is a real and probably
  unacceptable latency, but it is categorically not "waits unboundedly until
  the 30-second runnable-stall watchdog fires".
- **Cause vs consequence:** the wait we DO produce is causal, not injected —
  the real `cgroup_bw.bpf.c` parked the victims and the accounting reached that
  state on its own. Nothing was forced.
- **The likely reading** is that the mechanism in #3618 spans the `ext.c`
  boundary and is therefore outside what scx-sim models. That would route this
  to hermit rather than to more simulator grinding, and it is consistent with
  scx-sim bounding the wait at ~2s while production reports waits reaching the
  30s watchdog.
- **A bound to watch.** `the_cpumax_wait_is_bounded_not_unbounded` asserts the
  plateau, so if scx-sim ever starts scaling this wait with the observation
  window, that test fails and #3618 should be reopened here.

### What was NOT established

The origin of the ~1.98s bound. It is stable to the tenth of a millisecond
across three window sizes, which suggests a specific mechanism rather than a
statistical ceiling, but this run did not identify it. Worth knowing before
anyone cites 1.98s as a property of LAVD rather than of this scenario.

**Production corroboration boundary, restated because it constrains the above:**
the 34,734 runnable-task-stall exits corroborate URGENCY, NOT SUBTYPE. They do
not establish that any particular event was caused by tight `cpu.max`, and this
negative result borrows no causation from them either.

---

## Follow-up: was the 1.98s convergence itself window-limited? (expectation stated before running)

The 480.0ms figure was already identified above as a window ceiling, and the
window sweep that followed converged at 1980.2ms with the window at 19.2s —
10.3% of the window, so 1980.2ms is not itself a ceiling of *that* window.

**But that sweep tested only one configuration: quota 2ms/100ms with 4
victims.** The two MOST SEVERE configurations — 1ms/100ms and 0.5ms/100ms,
both with 8 victims — were only ever run at 600ms, where all three hit the
480.0ms ceiling and were therefore indistinguishable. **Nothing establishes
that the most severe configurations also converge at ~1.98s.** They could
plausibly scale further: more victims contending for less quota is exactly the
direction in which a starvation effect would worsen.

Stated before the run:

- **If the most severe configs also converge near ~1.98s** with the window far
  above it, the ~2s bound is a property of the mechanism rather than of one
  quota setting, and the NOT-REPRODUCED verdict stands on firmer ground than it
  did.
- **If they keep growing with the window** — 6s, 60s — then the earlier
  convergence was specific to a mild configuration, the bound is not general,
  and **the negative is wrong**: severe `cpu.max` would be producing exactly
  the unbounded wait #3618 describes, and this becomes a reproduction.
- **If they land on a different, larger constant** the bound is real but
  quota-dependent, which is a third answer and would need its own explanation.

Windows: 6s and 60s, against the 600ms baseline.

### Answer: THE NEGATIVE WAS WRONG. #3618 REPRODUCES.

The second of the three stated outcomes. The ~1.98s bound was specific to a
mild quota; it is not general, and the wait keeps climbing as `cpu.max`
tightens until the watchdog fires.

| quota (per 100ms) | converged worst wait | 30s watchdog |
|---|---|---|
| 2ms | 1.98s | no |
| 1ms | 4.98s | no |
| 0.5ms | 10.98s | no |
| 0.25ms | 21.18s | no |
| **0.125ms** | **40.28s** | **`ErrorStall { pid: 3, runnable_for_ns: 40279800000 }`** |
| **0.062ms** | **81.18s** | **`ErrorStall { pid: 3, runnable_for_ns: 49379200000 }`** |

Each halving of quota roughly doubles the worst wait (~2.1x). The wait
converges *for a fixed quota* — 0.5ms gives 10.98s at 60s, 120s and 240s
windows alike — but there is no quota-independent ceiling. Below ~0.25ms the
wait exceeds 30s and **the runnable-stall watchdog fires**.

That is the #3618 failure mode: tight `cpu.max` leaves a task waiting until the
30-second runnable-stall watchdog. It reproduces in scx-sim, causally, with no
injection.

**Note what fires.** The watchdog trips *despite* the throttle-aware exemption
added on 2026-08-12 that forgives tasks whose cgroup reports `is_throttled`.
The victim is starved long enough that even a watchdog specifically taught not
to blame throttling reports a stall. The earlier expectation that `ErrorStall`
would be suppressed for this scenario holds only while the wait is short.

**The routing conclusion is withdrawn.** "The mechanism spans the `ext.c`
boundary, route to hermit" was inferred from a bound that does not exist. No
such inference is available: scx-sim reproduces this on its own.

### Why the first answer was wrong, since the lesson is the point

Both wrong verdicts came from the same defect at different scales. The 480.0ms
figure was the 600ms window's ceiling. The 1.98s figure was not a ceiling — but
it was measured on **one mild configuration**, and generalised to a claim about
the mechanism. The severe configurations were never extended past 600ms, where
the window ceiling had made all three look identical and therefore
uninteresting.

So the second error was subtler than the first: the measurement was no longer
window-limited, it was **configuration-limited**. The sweep that disproved the
ceiling did not also disprove the generalisation, and I treated it as though it
had.

**A negative result is only as strong as the range the experiment could have
observed — and "range" means every axis, not just the one you last checked.**

---

## Question B: does PR #3618's own fix stop it? Not in this scenario.

**PR #3618 does propose a fix** — 3 commits, 3 files, +332/-71, and its body
describes exactly the pathology reproduced above: *"that wait is not bounded:
it can grow until a task trips the 30-second SCX runnable-task-stall watchdog
and brings the scheduler down."* Two mechanisms are attacked: [2/3]
`scx_cgroup_bw_pressure()` plus LAVD shortening slices as pressure rises, and
[3/3] blending wall-clock into BTQ vtime so a task reaches the queue head
within a few seconds regardless of vtime.

Applied and re-run. **The gradient does not flatten and the watchdog still
fires:**

| quota /100ms | unpatched | with #3618 |
|---|---|---|
| 0.5ms | 10.98s | 10.98s |
| 1ms | 4.98s | 4.98s |
| 0.125ms | 40.28s, `ErrorStall` | **40.28s, `ErrorStall`** |
| 0.062ms | 81.18s, `ErrorStall` | **81.18s, `ErrorStall`** |

Patch liveness was verified rather than assumed: the `.so` mtime postdates the
patched source, `scx_cgroup_bw_pressure` appears in both patched files, and the
built binary carries 19 `pressure` strings.

### Two caveats that could each explain the null result

1. **The patch was rebased.** It targets scx merge-base `3aa52aaf` with head
   `a8f72d09` (2026-04-22); our pin is `59c30bae` (2026-05-13), three weeks
   later. `git apply -3` merged all three files cleanly, but a clean *textual*
   merge is not a clean *semantic* one. Testing against the PR's own base would
   settle it.
2. **The pressure path may not be active.** [2/3] works by LAVD shortening
   slices as pressure rises. I confirmed the API is compiled in; I did **not**
   confirm the slice-shortening actually engages in this scenario. If it needs
   a config knob we do not set, the run tested [3/3] alone.

**So this is not "the fix does not work".** It is: *as applied to our pin, with
liveness confirmed only at the API level, the fix does not change this
scenario's outcome.* Resolving caveat 2 is the obvious next step and is cheap —
instrument whether slices actually shorten under pressure.

---

## Caveat 2 resolved: the mechanism ENGAGES. It just does not help here.

The patch adds a `pressure:` field to the cgroup dump, which the existing
LAVD-PRINTK output already surfaces — so no new instrumentation was needed.
Measured, patch applied, at the two watchdog-tripping quotas:

| quota | `cgx->pressure` | expected slice scaling | outcome |
|---|---|---|---|
| 0.125ms | **2176** | `1024/2176` = 47% of base | 40.28s, `ErrorStall` |
| 0.062ms | **3323** | `1024/3323` = 31% of base | 81.18s, `ErrorStall` |

`CBW_PRESSURE_NORMAL` is 1024, so pressure is 2.1x and 3.2x normal. It is being
computed, it is nonzero, and LAVD is consuming it — `main.bpf.c:397` really does
`slice_wall = max((slice_wall * LAVD_SCALE) / pressure, 1)`, so slices genuinely
shorten to roughly a third under the severest quota.

**So the earlier "may not be engaging" caveat is wrong, and the answer is the
more interesting one:** part [2/3] works exactly as designed — pressure is
detected, propagated and applied — and the watchdog still fires at bit-identical
times. Shortening slices does not bound this wait.

That is consistent with the PR's own framing, which treats slice length and
queue ordering as *two separate* causes of unbounded wait. It suggests the
binding constraint here is [3/3] — the BTQ vtime ordering — not [2/3].

### What this changes about the message

Of the three possible messages, the evidence now supports the third, narrowed:

- ~~"your fix does not work"~~ — unsupported; [2/3] demonstrably does what it says.
- ~~"we could not make your fix engage"~~ — disproved; pressure 2176/3323.
- **"[2/3] engages correctly and does not bound the wait in this scenario;
  the watchdog still fires at identical times."** Whether [3/3] would, on its
  own base, is the remaining open question.

---

## Part [3/3] engages too, and the wait is ~9-19x its intended bound

[3/3] blends wall-clock into the BTQ sort key:
`btq_vtime = (scx_bpf_now() & UPPER_MASK) | (vtime & LOWER_MASK)` with
`CBW_BTQ_VTIME_MASK_SHIFT = 32`. Its own comment states the design intent:
*"bounding the maximum BTQ wait to ~4 seconds"* — one 2^32 ns epoch, which is
exactly what this investigation's original hypothesis predicted it would cap.

**The runs that produced 40.28s and 81.18s had [3/3] applied.** The diff fetched
from `pull/3618.diff` is the whole PR, all three commits, so every result
reported here for "with the fix" already included the blend. Engagement
evidence: the blend appears 5 times in the patched source, and the scenario
reaches the BTQ — `nr_pending` is 9 and 8 at the two severe quotas, so there
are real tasks queued in the BTQ for the blend to reorder.

So both parts are active and the measured wait is **~9x and ~19x** the ~4s
bound [3/3] is designed to enforce.

**This is a significant finding about the whole PR, and it needs saying
plainly: with both mechanisms verified active, the proposed fix does not
address the failure mode in this repro.** That is a claim about this scenario
in this simulator, not a claim that the PR is wrong in general — see Limits.

## Watchdog threshold: which was used, and does it matter

**The #104 runs used 30s** — `DEFAULT_WATCHDOG_TIMEOUT_NS = 30_000_000_000`
(`safe/scenario.rs:666`), applied by the builder at `:851`; the test never
overrode it. So the inference from the gradient was right.

Whether a 4s watchdog would do — tested, not argued:

| watchdog | quota | worst wait | exit |
|---|---|---|---|
| 30s | 1ms | 4.98s | Normal |
| 30s | 0.5ms | 10.98s | Normal |
| 30s | 0.125ms | 40.28s | `ErrorStall` |
| 4s | 1ms | 4.98s | `ErrorStall` @4.98s |
| 4s | 0.5ms | **9.98s** | `ErrorStall` @9.98s |
| 4s | 0.125ms | **40.28s** | `ErrorStall` @40.28s |

**The gradient survives.** I expected truncation — abort at 4s, measurement
censored, gradient flattened — and that did not happen. The watchdog reports
`runnable_for_ns` at the stall's full length, and the wait is one continuous
stall rather than an accumulation, so the measured value is intact at abort.

Two qualifications. The 0.5ms case moved 10.98s -> 9.98s, about 9%, so values
are not perfectly threshold-independent. And the cost saving is smaller than it
looks: at 0.125ms the run still proceeds to a 40.28s stall before aborting, so
only the mild configurations finish early.

**Conclusion: report wait times, which are near-threshold-independent, not
"the watchdog fired", which is a parameter.** 4s is fine for sweeping and
cheaper on the mild end. 30s is retained in the committed tests for fidelity to
scx#3618's own claim, which is specifically about *the 30-second SCX
runnable-task-stall watchdog*; a reader checking our result against the PR's
wording should not have to reconcile a different threshold.
