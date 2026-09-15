IRQ Stress Testing: Background and Implementation Reference
================================================================

This document is required reading for agents working on IRQ stress testing,
scheduler interrupt handling, or the scx_simulator's IRQ modeling. It covers
hardware interrupts at the processor level, Linux kernel interrupt handling,
BPF scheduler context, and our two concrete IRQ generation implementations.

Table of Contents
-----------------

1. Hardware Interrupts at the Processor Level
2. Linux Kernel Interrupt Handling: Top Half and Bottom Half
3. BPF Scheduler Context: hardirq and softirq
4. CPU Time Stolen by IRQs
5. IRQ Pressure and Scheduler Decisions
6. schtest IRQ Disruption Framework (irq.staging branch)
7. rt-app softirq Workload Generator (David Dai's Fork)
8. Environment Variables and Configuration
9. Quick Reference: IRQ Generation Parameters

------------------------------------------------------------------------

## 1. Hardware Interrupts at the Processor Level

### Interrupt Descriptor Table (IDT)

x86-64 processors use a 256-entry Interrupt Descriptor Table (IDT) to map
interrupt vectors to handler functions. Each entry contains a gate descriptor
pointing to the handler's code segment and instruction pointer.

- **Vectors 0-31**: CPU exceptions (page fault #14, general protection #13,
  divide error #0, etc.)
- **Vectors 32-255**: Available for external interrupts and software use
- **Vector 0x80**: Traditional Linux system call entry (int 0x80)
- **Vectors 0xEF-0xFF**: Local APIC vectors (timer, thermal, PMI, spurious)

### IRQ Controllers

Modern x86 systems use a two-level interrupt controller hierarchy:

**I/O APIC** (one per chipset): Routes external device interrupts (network
cards, disk controllers, USB) to specific CPUs. Supports interrupt routing
via destination mode (physical CPU APIC ID or logical group) and delivery
mode (fixed, lowest priority, NMI, SMI).

**Local APIC** (one per CPU): Handles local interrupts (timer, thermal,
performance monitoring) and inter-processor interrupts (IPIs). Each CPU has
a unique APIC ID.

### Inter-Processor Interrupts (IPIs)

IPIs are interrupts sent from one CPU to another via the local APIC. Linux
uses IPIs extensively:

- **Reschedule IPI** (`RES` in /proc/interrupts): Asks target CPU to run
  the scheduler. Sent by `resched_curr()` when a higher-priority task is
  enqueued on a remote CPU's runqueue. This is the IPI most relevant to
  sched_ext schedulers.
- **Function Call IPI** (`CAL`): `smp_call_function_single()` / `_many()`.
  Used for TLB shootdowns, cache flushes, and general cross-CPU work.
- **TLB Shootdown IPI** (`TLB`): Subset of function call IPIs specifically
  for invalidating TLB entries after page table modifications.

### Performance Monitoring Interrupts (PMIs)

PMIs fire when a hardware performance counter overflows. The APIC routes
them to vector 0xF5 (typically). Linux's perf subsystem uses PMIs for
sampling-based profiling: configure a counter to overflow after N events,
handle the interrupt, record the sample, reset, and re-arm.

PMIs are non-maskable on many processors (delivered as NMI on some
configurations), making them useful for profiling code that runs with
interrupts disabled.

------------------------------------------------------------------------

## 2. Linux Kernel Interrupt Handling: Top Half and Bottom Half

### Top Half (hardirq context)

When an interrupt arrives, the CPU saves state and jumps to the IDT handler.
The kernel's top-half handler runs with interrupts disabled on the local CPU
(on x86, the processor clears IF automatically on entry). Key constraints:

- **Cannot sleep**: No blocking operations, no mutex acquisition
- **Cannot allocate with GFP_KERNEL**: Only GFP_ATOMIC
- **Should be fast**: Prolonged hardirq context delays all other interrupts
  on this CPU
- **Preemption disabled**: `preempt_count` has hardirq bit set

The top half typically acknowledges the hardware, copies essential data to
memory, and schedules bottom-half processing.

### Bottom Half: softirqs

Softirqs are the highest-priority deferred work mechanism. There are 10
statically defined softirq types:

```
HI_SOFTIRQ          0   High-priority tasklets
TIMER_SOFTIRQ        1   Timer callbacks (hrtimer, timer_list)
NET_TX_SOFTIRQ       2   Network transmit completion
NET_RX_SOFTIRQ       3   Network receive processing (NAPI)
BLOCK_SOFTIRQ        4   Block device completion
IRQ_POLL_SOFTIRQ     5   IRQ polling
TASKLET_SOFTIRQ      6   Normal-priority tasklets
SCHED_SOFTIRQ        7   Scheduler load balancing
HRTIMER_SOFTIRQ      8   High-resolution timer (some configs)
RCU_SOFTIRQ          9   RCU callbacks
```

Softirqs run with interrupts enabled but preemption disabled. They execute
on the CPU that raised them (unlike workqueues). Multiple softirqs of the
same type can run concurrently on different CPUs.

**NET_TX_SOFTIRQ and NET_RX_SOFTIRQ** are particularly relevant for IRQ
stress testing because sending UDP packets to loopback triggers both on
predictable CPUs (especially with RPS steering; see Section 7).

### Bottom Half: tasklets

Tasklets are built on top of softirqs (HI_SOFTIRQ and TASKLET_SOFTIRQ).
Unlike raw softirqs, a given tasklet is serialized: it never runs
concurrently on two CPUs. Most drivers use tasklets for deferred interrupt
handling.

### Bottom Half: workqueues

Workqueues defer work to kernel threads (kworkers). Unlike softirqs and
tasklets, workqueue handlers run in process context and can sleep. They are
the lowest-priority deferred work mechanism but the most flexible.

### ksoftirqd

If softirqs re-raise themselves too many times (typically > 10 iterations),
the kernel defers remaining work to the per-CPU `ksoftirqd/N` kernel thread.
This prevents softirq storms from starving userspace indefinitely. The
ksoftirqd threads run at nice 19 (lowest priority) and are visible in
`ps aux | grep ksoftirqd`.

------------------------------------------------------------------------

## 3. BPF Scheduler Context: hardirq and softirq

### BPF Context Detection

BPF programs in sched_ext can detect their execution context:

```c
bool in_hardirq = bpf_in_hardirq();
bool in_softirq = bpf_in_serving_softirq();
```

**`bpf_in_hardirq()`** returns true when the BPF program is executing in
hardware interrupt context (top half). In sched_ext, this can happen when:
- A timer interrupt fires and calls `ops.tick()` from hardirq context
- An IPI triggers a reschedule from hardirq context

**`bpf_in_serving_softirq()`** returns true when executing in softirq
context (bottom half). This matters because:
- Some scheduler callbacks may be called from softirq context
- Softirq context has different preemption and sleeping constraints
- LAVD uses this to adjust scheduling decisions

### Implications for sched_ext Schedulers

When `bpf_in_hardirq()` is true:
- The scheduler callback must not perform heavy computation
- Per-CPU data access is safe (no preemption)
- Global lock acquisition should be avoided (can't sleep in hardirq)
- Timer-based tick callbacks (`ops.tick()`) commonly run here

When `bpf_in_serving_softirq()` is true:
- Preemption is disabled but interrupts are enabled
- Moderate computation is acceptable
- The callback is on the CPU that raised the softirq

------------------------------------------------------------------------

## 4. CPU Time Stolen by IRQs

### Kernel Accounting

The kernel tracks interrupt time in `cpu_stat`:
- **`irq`** (hardirq): Time spent in hardware interrupt handlers
- **`softirq`**: Time spent in softirq handlers

These are visible in `/proc/stat` as fields 6 (irq) and 7 (softirq) in
the per-CPU lines. The values are in USER_HZ ticks (typically 100 Hz,
so each tick = 10ms).

### /proc/interrupts

`/proc/interrupts` shows per-CPU interrupt counts for each interrupt source.
Key rows for IRQ stress testing:

```
           CPU0       CPU1       CPU2       CPU3
  LOC:   1234567    1234568    1234569    1234570   Local timer interrupts
  RES:      1234       5678       9012       3456   Rescheduling interrupts
  CAL:       234        567        890        123   Function call interrupts
  TLB:       100        200        300        400   TLB shootdowns
  PMI:         0          0      50000          0   Performance monitoring
```

The schtest `InterruptSnapshot` struct (Section 6) parses this file to
measure IRQ rates during stress tests.

### irq_stolen_ns in the Simulator

The scx_simulator tracks IRQ-stolen time per CPU via `irq_stolen_ns` in
`SimCpu`. When an `IrqStart` event fires, the simulator records the
timestamp. When `IrqEnd` fires, the elapsed time is added to
`irq_stolen_ns`. This stolen time is then subtracted from the CPU's
available scheduling quantum:

```rust
// In handle_slice_expired:
let stolen = s.sim.cpus[cpu.0 as usize].irq_stolen_ns;
if stolen > 0 {
    s.sim.cpus[cpu.0 as usize].irq_stolen_ns = 0;
    // Re-schedule the slice expiry later by the stolen amount
    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
    s.events.push(local_t + stolen, EventKind::SliceExpired { cpu });
}
```

------------------------------------------------------------------------

## 5. IRQ Pressure and Scheduler Decisions

### LAVD: Latency-Critical Scheduling Under IRQ Pressure

LAVD (Latency-Aware Virtual Deadline) is a sched_ext scheduler that
explicitly accounts for IRQ pressure in scheduling decisions:

**`lat_cri` (latency criticality)**: LAVD computes a per-task latency
criticality score. Tasks with high `lat_cri` are prioritized. IRQ
pressure on a CPU increases the perceived latency of tasks running
there, affecting their `lat_cri` scores.

**Stolen time accounting**: LAVD tracks how much CPU time was stolen by
interrupts. When a CPU has high IRQ load, tasks running on it appear to
have longer scheduling latencies (because their time slices are
interrupted). LAVD uses this to:
1. Boost the priority of tasks that lost time to IRQs
2. Consider migrating latency-critical tasks away from IRQ-heavy CPUs

**Task migration under IRQ pressure**: When one CPU is heavily loaded
with interrupts while another is quiet, a good scheduler should migrate
latency-sensitive tasks to the quiet CPU. This is exactly what the
`irq_migration` test (Section 6) verifies.

### General Scheduler Concerns

- **Time slice extension**: If a task's time slice was interrupted by IRQs,
  the scheduler may need to extend it (EEVDF does this natively via
  vruntime accounting)
- **CPU selection**: `ops.select_cpu()` should consider IRQ load when
  choosing which CPU to run a task on
- **Load balancing**: The load balancer should account for IRQ time as
  "unavailable" CPU capacity

------------------------------------------------------------------------

## 6. schtest IRQ Disruption Framework (irq.staging Branch)

The `irq.staging` branch of the schtest repository provides a comprehensive
framework for generating controlled IRQ load on specific CPUs and measuring
its impact on scheduler behavior.

**Branch**: `origin/irq.staging` in the sched-test repo
**Key files**: `src/cases/irq_common.rs`, `src/cases/irq_accounting.rs`,
`src/cases/irq_migration.rs`

### IRQ Disruption Modes

The framework supports four IRQ generation strategies, selected via the
`SCHTEST_IRQ_MODE` environment variable:

#### Timer Mode (default)

Creates `NUM_TIMERS = 4` POSIX interval timers per victim CPU, each firing
at the configured rate. Uses real-time signals (`SIGRTMIN+0` through
`SIGRTMIN+3`) to avoid signal coalescing (standard signals are not queued;
real-time signals are).

```
Effective rate = timer_hz * NUM_TIMERS
Default: 140,000 Hz * 4 = 560,000 interrupts/sec
```

**Implementation details**:
- `timer_create(CLOCK_MONOTONIC, SIGEV_SIGNAL)` for each timer
- Timers are staggered: timer i starts at offset `(i * interval) / N`
  to spread interrupts evenly across the period
- A spinner process on the victim CPU calls `nanosleep(10us)` in a loop,
  allowing signals to interrupt it
- Signal handler is minimal: `SIGNAL_COUNT.fetch_add(1, Relaxed)`

**Why 4 timers**: A single POSIX timer at very high frequency (>100kHz)
may coalesce signals if the handler doesn't return fast enough. Using
4 timers with staggered phases achieves higher effective rates.

#### Futex IPI Mode

Generates cross-core Inter-Processor Interrupts (IPIs) via the
`futex(FUTEX_WAKE)` syscall. A waker thread on one CPU repeatedly wakes
a receiver thread on the victim CPU.

```
Architecture:
  Waker CPU: spin-wait → futex_wake(futex_word) → repeat at irq_hz
  Victim CPU: futex_wait(futex_word) → woken → loop
```

**Why this generates IPIs**: When the waker calls `futex(FUTEX_WAKE)`, the
kernel needs to wake a thread blocked on a different CPU. This requires
sending a reschedule IPI to the victim CPU, which shows up as `RES`
(Rescheduling interrupts) in `/proc/interrupts`.

**Instrumentation**: The receiver tracks three counters:
- `futex_wait_calls`: Total `futex_wait` syscalls
- `futex_wait_blocks`: Times the call actually blocked
- `futex_wait_eagain`: Times EAGAIN was returned (value already changed)

#### PMU Mode

Uses `perf_event_open` with hardware cycle counter in frequency mode to
generate Performance Monitoring Interrupts (PMIs) at the target rate.

```c
struct perf_event_attr attr = {
    .type = PERF_TYPE_HARDWARE,
    .config = PERF_COUNT_HW_CPU_CYCLES,
    .sample_type = PERF_SAMPLE_IP,
    .freq = 1,
    .sample_freq = target_hz,
    .disabled = 1,
};
perf_event_open(&attr, 0, cpu, -1, 0);
ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
```

**Note**: The kernel limits the maximum sample rate via
`/proc/sys/kernel/perf_event_max_sample_rate` (typically 100,000 Hz).
The function `get_max_perf_sample_rate()` reads this limit and caps the
requested rate accordingly.

**Why PMU mode is useful**: PMIs are delivered via NMI or APIC interrupt,
which exercises a different interrupt delivery path than timers or IPIs.
This can expose scheduler bugs that only manifest under PMI-specific
interrupt timing.

#### Combined Mode

Launches all three strategies simultaneously on the victim CPU. Used for
maximum interrupt pressure testing.

### Test: irq_accounting (`irq_disruption_targeted`)

**Purpose**: Verify that IRQ-stolen CPU time is correctly accounted and
affects throughput proportionally.

**Setup**:
- CPU 1 (victim): cgroup-limited to 50% CPU, under IRQ disruption
- CPU 2 (control): cgroup-limited to 50% CPU, no IRQ disruption
- Both run `cpu_hog_workload` (tight loop counting bogo_ops)

**Measurement**: Compare `bogo_ops/ms` between victim and control. The
victim should show lower throughput proportional to IRQ overhead. Reports
skew percentage.

**Assertion**: Victim throughput < control throughput (within tolerance).

### Test: irq_migration (`irq_migration`)

**Purpose**: Verify that the scheduler migrates latency-sensitive tasks
away from IRQ-heavy CPUs to quiet CPUs.

**Setup**:
1. Select all physical cores (deduplicated from hyperthreads)
2. Reserve one core as the quiet "control" core
3. Optionally reserve a tracing core (`SCHTEST_IRQ_RESERVE_TRACING_CORE`)
4. Apply timer IRQ disruption to ALL victim cores
5. Launch `lat_cap_workers` on all CPUs (low duty-cycle spinners for LAVD
   stolen_time sampling)
6. Launch a `spinner_probe` (nice -10, high priority) on a victim CPU
7. Unpin the spinner probe (remove CPU affinity)
8. Wait for the scheduler to migrate it

**Measurement**: Track all CPU transitions of the spinner probe via
`CpuTransitionLog` (shared memory, up to 1024 transitions). Record
which CPU the probe ends up on and how long migration took.

**Expected behavior**: A good scheduler (LAVD) should migrate the
high-priority, unpinned spinner to the quiet control core within a few
scheduling periods.

### Supporting Infrastructure

#### LatCapWorkers

Low duty-cycle (5%) background workers pinned to each CPU. They spin for
`DUTY_CYCLE * PERIOD` then sleep for `(1 - DUTY_CYCLE) * PERIOD`.
Default: 5ms spin, 95ms sleep per 100ms period.

**Purpose**: LAVD requires periodic userspace execution on each CPU to
sample stolen_time. Without these workers, a fully idle CPU would never
have its stolen_time sampled, and LAVD wouldn't know the CPU is
IRQ-heavy.

#### PingPongProbes

Two futex-based ping-pong workers with nice -10 priority. They alternate:
one waits on `futex_a`, the other on `futex_b`. Each wakes the other
at `PING_PONG_HZ = 10,000` round-trips/sec.

**Purpose**: Measures scheduling latency under IRQ pressure. The
round-trip time reflects how quickly the scheduler context-switches
between the two high-priority tasks.

#### SpinnerProbe

A single high-priority (nice -10) spinner that records every CPU
migration event. Uses shared memory (`CpuTransitionLog`) with atomic
counters to track up to `MAX_CPU_TRANSITIONS = 1024` transitions.

Each transition records:
- `from_cpu`: CPU before migration
- `to_cpu`: CPU after migration
- `timestamp_ns`: `CLOCK_MONOTONIC` time of detection

#### InterruptSnapshot

Parses `/proc/interrupts` to capture per-CPU interrupt counts. Supports
delta computation between two snapshots to measure IRQ rates during a
test window. Tracks total, reschedule (RES), function call (CAL), and
TLB shootdown interrupts separately.

------------------------------------------------------------------------

## 7. rt-app Softirq Workload Generator (David Dai's Fork)

**Repository**: `~/work/multi_sched-test/rt-app/`
**Branch**: `irq_workload`
**Author**: David Dai
**Commit**: `9eedd75` (2026-02-20)

### Overview

David Dai's rt-app fork adds a `softirq` event type that generates
realistic soft-IRQ interference using UDP loopback traffic with Linux RPS
(Receive Packet Steering) to steer interrupt processing to specific CPUs.

Unlike the schtest IRQ framework (which generates hardirq-level
interrupts via timers/IPIs/PMU), this approach generates **softirq-level**
interrupts (`NET_TX_SOFTIRQ` and `NET_RX_SOFTIRQ`), exercising a
different part of the kernel's interrupt handling path.

### Mechanism

```
Task on CPU A:  sendto(loopback, UDP packet)
                  -> NET_TX softirq on CPU A (transmit completion)
                  -> Loopback driver "receives" packet
                  -> NET_RX softirq on CPU B (via RPS steering)
```

The key innovation is using **RPS (Receive Packet Steering)** to control
which CPU processes the `NET_RX` softirq. Without RPS, the RX softirq
runs on the same CPU as the sender. With RPS configured, the kernel hashes
the packet and routes the RX processing to a CPU in the RPS mask.

### Configuration (JSON)

#### Per-task event

```json
{
  "phases": {
    "phase0": {
      "loop": 100,
      "softirq": 1000
    }
  }
}
```

The `"softirq": <duration_usec>` field causes the task to call
`softirqload()` which sends UDP packets to loopback in a tight loop
for the specified duration in microseconds.

#### Global parameters

```json
{
  "global": {
    "softirq_packet_size": 64,
    "softirq_target_cpus": [0, 2, 4]
  }
}
```

- **`softirq_packet_size`** (default: 64): Size of each UDP packet payload
  in bytes. Larger packets = more processing time per softirq.
- **`softirq_target_cpus`**: Array of CPU indices. Converted to a bitmask
  and written to `/sys/class/net/lo/queues/rx-0/rps_cpus` to steer
  `NET_RX` softirq processing to those specific CPUs.

#### Complete example (`doc/examples/softirq.json`)

The example configures:
- 6 worker tasks (`worker0`-`worker5`) on CPUs 0-5, each doing 10ms
  compute every 10ms (100% utilization)
- 3 IRQ generator tasks (`irq_gen0`-`irq_gen2`) on CPUs 6-8, each
  sending UDP packets for 1ms every 10ms
- RPS targets CPUs 0, 2, 4 (so `NET_RX` softirqs land on alternating
  worker CPUs, creating asymmetric interference)

### Implementation Details

#### `softirqload()` function (`src/rt-app.c`)

```c
static void softirqload(int duration_usec,
                        struct _rtapp_softirq *sirq,
                        int packet_size) {
    char buf[packet_size];
    struct timespec start, now;
    clock_gettime(CLOCK_MONOTONIC, &start);

    while (elapsed_us < duration_usec) {
        sendto(sirq->fd, buf, packet_size, 0,
               (struct sockaddr*)&sirq->addr,
               sizeof(sirq->addr));
        clock_gettime(CLOCK_MONOTONIC, &now);
        // ... compute elapsed_us
    }
}
```

#### Resource initialization

```c
void init_softirq_resource(rtapp_resource_t *res, rtapp_options_t *opts) {
    // Create UDP socket
    res->res.softirq.fd = socket(AF_INET, SOCK_DGRAM, 0);
    // Target: 127.0.0.1:39154
    res->res.softirq.addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    res->res.softirq.addr.sin_port = htons(39154);

    // If RPS mask configured, write to sysfs
    if (opts->softirq_rps_mask) {
        // Write to /sys/class/net/lo/queues/rx-0/rps_cpus
        fprintf(rps_file, "%lx", opts->softirq_rps_mask);
    }
}
```

**Note**: Writing to `/sys/class/net/lo/queues/rx-0/rps_cpus` requires
root privileges and kernel `CONFIG_RPS` support.

### Comparison: schtest vs rt-app IRQ Generation

| Aspect | schtest (irq_common.rs) | rt-app (softirq) |
|--------|------------------------|-------------------|
| **IRQ type** | Hardirq (timer/IPI/PMU) | Softirq (NET_TX/NET_RX) |
| **Target CPU control** | CPU affinity | RPS steering |
| **Rate control** | Hz parameter | Duration-based |
| **Kernel mechanism** | timer_create, futex, perf_event | sendto(loopback) |
| **Requires root** | No (except PMU) | Yes (for RPS config) |
| **What it tests** | Hardirq stolen time, IPI storms | Softirq processing overhead |
| **BPF visibility** | `bpf_in_hardirq() = true` | `bpf_in_serving_softirq() = true` |

------------------------------------------------------------------------

## 8. Environment Variables and Configuration

### schtest IRQ test environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SCHTEST_IRQ_MODE` | `timer` | IRQ disruption strategy: `none`, `futex`, `pmu`, `timer`, `combined` |
| `SCHTEST_IRQ_HZ` | `140000` | Target interrupt rate in Hz |
| `SCHTEST_DURATION` | `5` | Test duration in seconds |
| `SCHTEST_IRQ_RESERVE_TRACING_CORE` | (none) | CPU index to exclude from IRQ storm (for perf/tracing) |

### Kernel sysctl parameters

| Sysctl | Typical Value | Effect |
|--------|--------------|--------|
| `/proc/sys/kernel/perf_event_max_sample_rate` | 100000 | Max PMU sample rate; caps PMU mode |
| `/proc/sys/kernel/sched_rt_runtime_us` | 950000 | RT bandwidth; affects timer thread scheduling |
| `/sys/class/net/lo/queues/rx-0/rps_cpus` | 0 | RPS steering mask for loopback; controls NET_RX CPU |

------------------------------------------------------------------------

## 9. Quick Reference: IRQ Generation Parameters

### Timer mode (recommended for most tests)

```
Rate: DEFAULT_IRQ_HZ = 140,000 Hz per timer
Timers: NUM_TIMERS = 4
Effective: 560,000 interrupts/sec per victim CPU
Signal: SIGRTMIN+0 through SIGRTMIN+3 (queued, no coalescing)
Kernel mechanism: timer_create(CLOCK_MONOTONIC, SIGEV_SIGNAL)
```

### Futex IPI mode

```
Rate: configurable via SCHTEST_IRQ_HZ
Mechanism: futex_wake from waker CPU -> reschedule IPI -> victim CPU
IPI type: RES (Rescheduling interrupts) in /proc/interrupts
Waker: spin-waits to achieve target rate (CPU-intensive)
```

### PMU mode

```
Rate: min(SCHTEST_IRQ_HZ, perf_event_max_sample_rate)
Mechanism: perf_event_open(PERF_COUNT_HW_CPU_CYCLES, freq=target_hz)
Interrupt: PMI via APIC (may be NMI on some configurations)
Limitation: Capped by kernel sysctl, may not achieve target rate
```

### LatCap workers

```
Duty cycle: LAT_CAP_WORKER_DUTY_CYCLE_PCT = 5%
Period: LAT_CAP_WORKER_PERIOD_MS = 100ms
Spin: 5ms per 100ms period
Purpose: Ensure LAVD samples stolen_time on each CPU
```

### PingPong probes

```
Rate: PING_PONG_HZ = 10,000 round-trips/sec
Priority: nice -10
Mechanism: Alternating futex_wait/futex_wake between two threads
Purpose: Measure scheduling latency under IRQ pressure
```

### rt-app softirq

```
Packet size: 64 bytes (configurable via softirq_packet_size)
Port: 127.0.0.1:39154 (UDP loopback)
CPU steering: RPS mask via /sys/class/net/lo/queues/rx-0/rps_cpus
Requires: root, CONFIG_RPS
Softirq types: NET_TX (sender CPU) + NET_RX (RPS target CPU)
```
