//! Does the scheduler still connect to the simulator at all?
//!
//! This is a GATE, not a test suite. It runs before the real scheduler tests
//! and decides whether running them is even meaningful. It is deliberately
//! narrow: a narrow gate that is trustworthy beats a broad one that is noisy.
//!
//! # The drift it exists to catch
//!
//! `sched-test` submodules `scx`. The schedulers under `schedulers/*/wrapper.c`
//! `#include` the genuine upstream BPF C from that submodule and compile it as
//! userspace C into `libscx_<name>.so`, which the Rust side `dlopen`s with
//! `RTLD_NOW`. That arrangement is the "connection".
//!
//! When the pin moves forward, the scheduler can gain code the simulator does
//! not yet support — a new kfunc, a new kernel struct field, a new helper. The
//! connection then breaks, and **the scheduler is not at fault: scx-sim is
//! behind.** Attributing such a failure to the scheduler import is the specific
//! outcome this gate exists to prevent, because a signal that blames the wrong
//! party gets ignored, and an ignored gate is worse than no gate.
//!
//! # What this actually exercises
//!
//! The real upstream scheduler C, compiled natively and executed. Not a stub,
//! not a Rust reimplementation. Each case constructs the scheduler (which
//! `dlopen`s the `.so` with `RTLD_NOW`, so an unresolved symbol fails here),
//! runs `ops.init` plus a small workload, and requires that **real scheduling
//! decisions were observed**. A scheduler that loaded but did nothing fails.
//!
//! # What it does NOT cover — read this before trusting a green
//!
//! * **No kernel. At all.** This is the scheduler-C-compiled-natively path.
//!   There is no BPF verifier, no real `sched_ext`, no real preemption, IRQ
//!   timing, topology or contention. A green here says the connection works; it
//!   says nothing about whether the scheduler is correct under a real kernel.
//! * **ABI/semantic drift is only partly covered.** Upstream can change a
//!   struct layout or an enum value in a way that still compiles, still links,
//!   still runs, and is silently wrong. That class is the reason the real test
//!   suite exists downstream of this gate; the gate cannot replace it.
//! * **Only the paths a tiny workload reaches.** Cgroup bandwidth, hotplug,
//!   multi-node topologies, layered match rules and similar are downstream
//!   concerns by design.
//! * **x86_64 only**, like the rest of scx-sim CI.

use scx_simulator::*;
use std::collections::BTreeSet;

#[macro_use]
mod common;

/// Every scheduler the build manifest produces a `.so` for.
///
/// Kept as an explicit list rather than derived, so that a scheduler ADDED
/// upstream without being wired in here shows up as a deliberate edit to this
/// file rather than silently going ungated. `all_built_schedulers_are_gated`
/// checks the list against what actually got built.
const GATED: &[&str] = &["simple", "tickless", "cosmos", "lavd", "mitosis", "layered"];

fn construct(name: &str, nr_cpus: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "tickless" => DynamicScheduler::tickless(nr_cpus),
        "cosmos" => DynamicScheduler::cosmos(nr_cpus),
        "lavd" => DynamicScheduler::lavd(nr_cpus),
        "mitosis" => DynamicScheduler::mitosis(nr_cpus),
        "layered" => DynamicScheduler::layered(nr_cpus),
        other => panic!("{other} is in GATED but has no constructor here"),
    }
}

/// A workload small enough to be a gate and busy enough to force real
/// decisions: more runnable tasks than CPUs, each alternating run and sleep so
/// the scheduler must enqueue, select a CPU, dispatch, and stop repeatedly.
fn smoke_scenario(nr_cpus: u32) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).detect_bpf_errors();
    for i in 0..(nr_cpus * 2) {
        b = b.add_task(
            &format!("smoke{i}"),
            0,
            TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(500_000)],
                repeat: RepeatMode::Forever,
            },
        );
    }
    b.duration_ms(60).build()
}

/// Evidence that the scheduler actually scheduled, rather than loading and
/// doing nothing. Returns (distinct pids run, distinct CPUs used, slice count).
fn scheduling_observed(trace: &Trace) -> (usize, usize, usize) {
    let mut pids = BTreeSet::new();
    let mut cpus = BTreeSet::new();
    let mut slices = 0usize;
    for e in trace.events() {
        if let TraceKind::TaskScheduled { pid } = e.kind {
            pids.insert(pid);
            cpus.insert(e.cpu.0);
            slices += 1;
        }
    }
    (pids.len(), cpus.len(), slices)
}

/// THE GATE. For every scheduler: load the real `.so`, run `ops.init` and a
/// small workload, and require observable scheduling.
///
/// One test over all six rather than six tests, on purpose. The consumer of
/// this gate is a CI job whose only question is "may the real tests run?", and
/// a single pass/fail with every scheduler's verdict in one message answers
/// that better than six results a reader has to assemble. It also means the
/// report names *every* broken scheduler in one run instead of stopping at the
/// first.
#[test]
fn schedulers_still_connect_to_the_simulator() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    let mut failures: Vec<String> = Vec::new();
    let mut report: Vec<String> = Vec::new();

    for name in GATED {
        // Construction `dlopen`s the `.so` with `RTLD_NOW`, so an upstream
        // change that calls a kfunc scx-sim does not export fails right here,
        // as an unresolved symbol, before any scheduling happens.
        //
        // That is the single most likely drift class, and left alone it aborts
        // the test with a raw FFI panic carrying no attribution — the reader
        // sees "failed to load libscx_lavd.so: undefined symbol: ..." and has
        // to already know whose fault that is. Catching it here is what lets
        // the gate say *scx-sim is behind* instead, and lets one run name every
        // broken scheduler rather than stopping at the first.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sched = construct(name, NR_CPUS);
            let sim = Simulator::new(sched);
            let trace = sim.run(smoke_scenario(NR_CPUS));
            let exit = trace.exit_kind().clone();
            let counts = scheduling_observed(&trace);
            (exit, counts)
        }));

        let (exit, (pids, cpus, slices)) = match outcome {
            Ok(v) => v,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                let first = msg.lines().next().unwrap_or(&msg).to_string();
                let stage = if msg.contains("undefined symbol") {
                    "LOAD (unresolved symbol)"
                } else if msg.contains("failed to load") {
                    "LOAD"
                } else {
                    "RUN"
                };
                report.push(format!("  {name:<9} FAILED at {stage}"));
                failures.push(format!("{name}: failed at {stage}: {first}"));
                continue;
            }
        };

        // A scheduler that loads, inits, and then schedules nothing is a broken
        // connection wearing a green mask. This is the no-stub guard: a stubbed
        // or no-op scheduler cannot produce these events.
        let ok_exit = matches!(exit, ExitKind::Normal);
        let ok_work = pids >= 2 && slices >= 4;

        report.push(format!(
            "  {name:<9} exit={exit:?} pids_run={pids} cpus_used={cpus} slices={slices}"
        ));
        if !ok_exit {
            failures.push(format!("{name}: exited {exit:?}, expected Normal"));
        }
        if !ok_work {
            failures.push(format!(
                "{name}: loaded but scheduled almost nothing \
                 (pids_run={pids}, slices={slices}); the .so linked but the \
                 scheduler is not making decisions"
            ));
        }
    }

    let detail = report.join("\n");
    assert!(
        failures.is_empty(),
        "SCHEDULER<->SIMULATOR CONNECTION IS BROKEN.\n\n\
         This is very likely NOT a fault in the scheduler source or in an scx \
         import. The usual cause is that scx-sim is BEHIND the scheduler: the \
         pinned scx revision contains code this simulator does not yet support. \
         Update scx-sim to match, and do not treat this as a defect in the \
         import.\n\n\
         Failures:\n  {}\n\nPer-scheduler:\n{}",
        failures.join("\n  "),
        detail
    );

    println!("scheduler<->simulator connection OK\n{detail}");
}

/// Guard against a scheduler being added to the build without being gated.
///
/// `GATED` is hand-written so that wiring a new scheduler into the gate is a
/// conscious act. That only works if forgetting is detected, which is what this
/// does: it compares the list against the `.so` the build actually produced.
#[test]
fn all_built_schedulers_are_gated() {
    let so_dir = std::path::Path::new(env!("SCHEDULER_SO_DIR"));
    let mut built: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(so_dir)
        .unwrap_or_else(|e| panic!("cannot read SCHEDULER_SO_DIR {}: {e}", so_dir.display()))
    {
        let f = entry.expect("readdir").file_name();
        let f = f.to_string_lossy();
        // `libscx_<name>.so`, excluding the optional `_e9` instrumented variants.
        if let Some(rest) = f.strip_prefix("libscx_") {
            if let Some(name) = rest.strip_suffix(".so") {
                if !name.ends_with("_e9") {
                    built.insert(name.to_string());
                }
            }
        }
    }
    let gated: BTreeSet<String> = GATED.iter().map(|s| s.to_string()).collect();
    let ungated: Vec<_> = built.difference(&gated).collect();
    assert!(
        ungated.is_empty(),
        "these schedulers are built but not covered by the connection gate: \
         {ungated:?}. Add them to GATED in this file (and give them a \
         constructor arm), or the gate will green-light tests for a scheduler \
         whose connection was never checked."
    );
    // The converse is a hard error too: a name in GATED with no .so means the
    // gate would silently cover nothing.
    let missing: Vec<_> = gated.difference(&built).collect();
    assert!(
        missing.is_empty(),
        "GATED names schedulers with no built .so: {missing:?} in {}",
        so_dir.display()
    );
}
