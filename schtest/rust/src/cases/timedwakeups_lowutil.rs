// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! Tests for latency scenarios.

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::{util, workloads};

use anyhow::Result;
use rand::Rng;
use util::stats::Distribution;
use util::stats::ReservoirSampler;
use util::system::CPUMask;
use util::system::CPUSet;
use util::system::System;
use workloads::benchmark::converge;
use workloads::context::Context;
use workloads::spinner::Spinner;

use crate::process;

/// Inner loop that performs timed sleep/wakeup cycles and tracks queueing delays.
fn timed_wakeup_loop<F>(
    mut get_iters: F,
    expected_sleep: Duration,
    work_time: Duration,
    sampler: &ReservoirSampler<Duration, 1024>,
    early_wakeups: &AtomicU32,
    total_wakeups: &AtomicU32,
) where
    F: FnMut() -> u32,
{
    let spinner = Spinner::default();
    let mut rng = rand::rng();
    loop {
        let iters = get_iters();
        let start_time = Instant::now();
        let end_time = start_time + Duration::from_millis(10 * (iters as u64));
        loop {
            let now = Instant::now();
            if now >= end_time {
                break;
            }

            // Randomized sleep: uniform [0, 2 * expected_sleep]
            // Average will be expected_sleep
            let sleep_requested_ms =
                rng.random_range(0.0..(2.0 * expected_sleep.as_secs_f64() * 1000.0));
            let sleep_requested = Duration::from_secs_f64(sleep_requested_ms / 1000.0);

            let before_sleep = Instant::now();
            std::thread::sleep(sleep_requested);
            let after_sleep = Instant::now();

            // Measure actual elapsed time
            let actual_elapsed = after_sleep.duration_since(before_sleep);

            // Track total wakeups
            total_wakeups.fetch_add(1, Ordering::Relaxed);

            // Check if early or late wakeup
            if actual_elapsed < sleep_requested {
                // Early wakeup
                early_wakeups.fetch_add(1, Ordering::Relaxed);
                // Sample 0 for early wakeup (no queueing delay)
                sampler.sample(Duration::ZERO);
            } else {
                // Late wakeup - sample the queueing delay
                let queueing_delay = actual_elapsed - sleep_requested;
                sampler.sample(queueing_delay);
            }

            // Perform fixed amount of work
            spinner.spin(work_time);
        }
    }
}

/// Test that ensures basic fairness for affinitized vs. non-affinitized tasks.
///
/// We create one task per logical CPU, and affinitize each one. Then we create the
/// same number of floating tasks, and ensure that the affinitized tasks do not have
/// materially lower total execution time than the non-affinity tasks.
///
/// This test runs at moderate utilization (50% by default) to ensure queueing delays
/// are minimal and both task groups receive fair scheduling treatment.
fn timedwakeups_lowutil() -> Result<()> {
    // Constants for workload configuration
    const TARGET_EPOCH_MS: f64 = 100.0; // Average duration of one sleep+wake cycle
    const TARGET_SYSTEM_UTILIZATION: f64 = 0.5; // 50% total CPU utilization target
    const NUM_TASK_GROUPS: f64 = 2.0; // Affinitized + Floating

    // Final check: Ensure p99 delays are under 10ms for both groups
    const MAX_P99_DELAY_MS: f64 = 10.0;

    // Calculate per-task duty cycle
    // With N CPUs, N affinitized tasks, and N floating tasks (2N total):
    // per_task_duty_cycle = target_utilization / num_groups
    const PER_TASK_DUTY_CYCLE: f64 = TARGET_SYSTEM_UTILIZATION / NUM_TASK_GROUPS;

    // Calculate work and sleep times from epoch and duty cycle
    let work_time = Duration::from_secs_f64((PER_TASK_DUTY_CYCLE * TARGET_EPOCH_MS) / 1000.0);
    let expected_sleep =
        Duration::from_secs_f64(((1.0 - PER_TASK_DUTY_CYCLE) * TARGET_EPOCH_MS) / 1000.0);
    let mut ctx = Context::create()?;
    let mut proc_affinitized = vec![];
    let mut proc_floating = vec![];

    let affinitized_sampler = ctx.allocate(ReservoirSampler::<Duration, 1024>::new())?;
    let floating_sampler = ctx.allocate(ReservoirSampler::<Duration, 1024>::new())?;

    // Atomic counters for tracking early wakeups (allocated in shared memory)
    let affinitized_early_wakeups = ctx.allocate(AtomicU32::new(0))?;
    let affinitized_total_wakeups = ctx.allocate(AtomicU32::new(0))?;
    let floating_early_wakeups = ctx.allocate(AtomicU32::new(0))?;
    let floating_total_wakeups = ctx.allocate(AtomicU32::new(0))?;

    // Affinitized tasks.
    for core in System::load()?.cores().iter() {
        for hyperthread in core.hyperthreads().iter() {
            let mask = CPUMask::new(hyperthread);
            let affinitized_sampler = affinitized_sampler.clone();
            let early_wakeups = affinitized_early_wakeups.clone();
            let total_wakeups = affinitized_total_wakeups.clone();
            proc_affinitized.push(process!(
                &mut ctx,
                None,
                (
                    mask,
                    expected_sleep,
                    work_time,
                    early_wakeups,
                    total_wakeups
                ),
                move |get_iters| {
                    mask.run(move || {
                        timed_wakeup_loop(
                            get_iters,
                            expected_sleep,
                            work_time,
                            &affinitized_sampler,
                            &early_wakeups,
                            &total_wakeups,
                        )
                    })
                }
            ));
        }
    }

    // Floating tasks.
    for _ in 0..System::load()?.logical_cpus() {
        let floating_sampler = floating_sampler.clone();
        let early_wakeups = floating_early_wakeups.clone();
        let total_wakeups = floating_total_wakeups.clone();
        proc_floating.push(process!(
            &mut ctx,
            None,
            (expected_sleep, work_time, early_wakeups, total_wakeups),
            move |get_iters| {
                timed_wakeup_loop(
                    get_iters,
                    expected_sleep,
                    work_time,
                    &floating_sampler,
                    &early_wakeups,
                    &total_wakeups,
                );
                Ok(())
            }
        ));
    }

    eprintln!(
        "Queueing delays: Affinitized tasks vs Floating tasks ({:.2}ms work)",
        work_time.as_secs_f64() * 1000.0
    );
    eprintln!(
        "Target CPU utilization: {:.0}% (per-task: {:.0}%)",
        TARGET_SYSTEM_UTILIZATION * 100.0,
        PER_TASK_DUTY_CYCLE * 100.0
    );
    eprintln!(
        "Epoch: {:.2}ms, Expected sleep: {:.2}ms, Work time: {:.2}ms",
        TARGET_EPOCH_MS,
        expected_sleep.as_secs_f64() * 1000.0,
        work_time.as_secs_f64() * 1000.0
    );

    let metric = |iters| {
        eprintln!();
        ctx.start(iters);
        ctx.wait()?;

        // Collect queueing delay distributions for both task groups
        let mut affinitized = Distribution::<Duration>::new();
        affinitized.add_all(&affinitized_sampler);

        let mut floating = Distribution::<Duration>::new();
        floating.add_all(&floating_sampler);

        let r1 = affinitized.estimates().range();
        let r2 = floating.estimates().range();
        let range = std::cmp::min(*r1.start(), *r2.start())..=std::cmp::max(*r1.end(), *r2.end());
        eprintln!(
            "Affinitized: {}",
            affinitized.estimates().visualize(None, Some(range.clone()))
        );
        eprintln!(
            "Floating:    {}",
            floating.estimates().visualize(None, Some(range.clone()))
        );

        // Calculate fairness ratio based on median queueing delays
        let affinitized_p50 = affinitized
            .estimates()
            .percentile(0.5)
            .unwrap()
            .as_secs_f64();
        let affinitized_p99 = affinitized
            .estimates()
            .percentile(0.99)
            .unwrap()
            .as_secs_f64();
        let floating_p50 = floating.estimates().percentile(0.5).unwrap().as_secs_f64();
        let floating_p99 = floating.estimates().percentile(0.99).unwrap().as_secs_f64();

        eprintln!(
            "Affinitized - p50: {:.3}ms, p99: {:.3}ms",
            affinitized_p50 * 1000.0,
            affinitized_p99 * 1000.0
        );
        eprintln!(
            "Floating    - p50: {:.3}ms, p99: {:.3}ms",
            floating_p50 * 1000.0,
            floating_p99 * 1000.0
        );

        // Report early wakeup statistics
        let aff_early = affinitized_early_wakeups.load(Ordering::Relaxed);
        let aff_total = affinitized_total_wakeups.load(Ordering::Relaxed);
        let float_early = floating_early_wakeups.load(Ordering::Relaxed);
        let float_total = floating_total_wakeups.load(Ordering::Relaxed);

        eprintln!("Wakeup Statistics:");
        eprintln!(
            "Affinitized - Early wakeups: {} / {} ({:.2}%)",
            aff_early,
            aff_total,
            if aff_total > 0 {
                (aff_early as f64 / aff_total as f64) * 100.0
            } else {
                0.0
            }
        );
        eprintln!(
            "Floating    - Early wakeups: {} / {} ({:.2}%)",
            float_early,
            float_total,
            if float_total > 0 {
                (float_early as f64 / float_total as f64) * 100.0
            } else {
                0.0
            }
        );

        if affinitized_p50 < floating_p50 {
            Ok(affinitized_p50 / floating_p50)
        } else {
            Ok(floating_p50 / affinitized_p50)
        }
    };

    // If we can achieve 0.95 fairness, we're happy.
    let target = 0.95;
    let final_value = converge(
        Some(Duration::from_secs_f64(30.0)),
        Some(Duration::from_secs_f64(60.0)),
        Some(target),
        metric,
    )?;
    if final_value < target {
        return Err(anyhow::anyhow!(
            "Failed to achieve target: got {:.2}, expected {:.2}",
            final_value,
            target
        ));
    }

    let mut affinitized = Distribution::<Duration>::new();
    affinitized.add_all(&affinitized_sampler);
    let mut floating = Distribution::<Duration>::new();
    floating.add_all(&floating_sampler);

    let affinitized_p99 = affinitized
        .estimates()
        .percentile(0.99)
        .unwrap()
        .as_secs_f64()
        * 1000.0;
    let floating_p99 = floating.estimates().percentile(0.99).unwrap().as_secs_f64() * 1000.0;

    eprintln!();
    eprintln!("Final p99 delay check:");
    eprintln!(
        "  Affinitized p99: {:.3}ms (threshold: {:.1}ms)",
        affinitized_p99, MAX_P99_DELAY_MS
    );
    eprintln!(
        "  Floating p99:    {:.3}ms (threshold: {:.1}ms)",
        floating_p99, MAX_P99_DELAY_MS
    );

    if affinitized_p99 > MAX_P99_DELAY_MS {
        return Err(anyhow::anyhow!(
            "Affinitized p99 delay {:.3}ms exceeds threshold {:.1}ms",
            affinitized_p99,
            MAX_P99_DELAY_MS
        ));
    }

    if floating_p99 > MAX_P99_DELAY_MS {
        return Err(anyhow::anyhow!(
            "Floating p99 delay {:.3}ms exceeds threshold {:.1}ms",
            floating_p99,
            MAX_P99_DELAY_MS
        ));
    }

    eprintln!("✓ All checks passed!");
    Ok(())
}

test!("timedwakeups_lowutil", timedwakeups_lowutil);
