// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Tests for latency scenarios.

use std::thread;
use std::time::Duration;

use anyhow::Result;
use crate::{process, util, workloads};
use util::stats::Distribution;
use util::system::CPUMask;
use util::system::CPUSet;
use util::system::System;
use workloads::benchmark::converge;
use workloads::context::Context;
use workloads::semaphore::Semaphore;
use workloads::spinner::Spinner;

/// Test that verifies the scheduler favors threads with lower expected execution.
///
/// This test creates a two processes that are fighting for a single one: one has two
/// threads that each spin for 10ms, and then wake the other, while the other has two
/// threads that spin for only 10us.
///
/// Without any kind of adaptive priority, this has the tendency to favor the hogs.
fn adaptive_priority() -> Result<()> {
    let mut ctx = Context::create()?;
    let mask = CPUMask::new(
        System::load()?
            .cores()
            .first()
            .unwrap()
            .hyperthreads()
            .first()
            .unwrap(),
    );

    let slow_sem = ctx.allocate(Semaphore::<2, 1024>::new(1))?;
    let slow_sem_ret = ctx.allocate(Semaphore::<2, 1024>::new(1))?;
    process!(
        &mut ctx,
        None,
        (mask, slow_sem, slow_sem_ret),
        move |mut get_iters| {
            thread::scope(|s| {
                let mask_copy = mask.clone();
                let slow_sem_copy = slow_sem.clone();
                let slow_sem_ret_copy = slow_sem_ret.clone();
                s.spawn(move || {
                    let spinner = Spinner::default();
                    mask_copy.run(move || {
                        loop {
                            slow_sem_copy.consume(1, 1, None);
                            // Spin for a full 10ms, consuming CPU.
                            spinner.spin(Duration::from_millis(10));
                            slow_sem_ret_copy.produce(1, 1, None);
                        }
                    })
                });
                mask.run(move || {
                    loop {
                        let iters = get_iters();
                        for _ in 0..iters {
                            slow_sem.produce(1, 1, None);
                            slow_sem_ret.consume(1, 1, None);
                        }
                    }
                })
            })
        }
    );
    let fast_sem = ctx.allocate(Semaphore::<2, 1024>::new(1))?;
    let fast_sem_ret = ctx.allocate(Semaphore::<2, 1024>::new(1))?;
    process!(
        &mut ctx,
        None,
        (mask, fast_sem, fast_sem_ret),
        move |mut get_iters| {
            thread::scope(|s| {
                let mask_copy = mask.clone();
                let fast_sem_copy = fast_sem.clone();
                let fast_sem_ret_copy = fast_sem_ret.clone();
                s.spawn(move || {
                    let spinner = Spinner::default();
                    mask_copy.run(move || {
                        loop {
                            fast_sem_copy.consume(1, 1, None);
                            // Still take a millisecond, but yield after 100us.
                            spinner.spin(Duration::from_nanos(100_000));
                            thread::sleep(Duration::from_nanos(10_000_000 - 100_000));
                            fast_sem_ret_copy.produce(1, 1, None);
                        }
                    })
                });
                mask.run(move || {
                    loop {
                        let iters = get_iters();
                        for _ in 0..iters {
                            fast_sem.produce(1, 1, None);
                            fast_sem_ret.consume(1, 1, None);
                        }
                    }
                })
            })
        }
    );

    let percentile = 50;
    let metric = move |iters| {
        ctx.start(iters);
        ctx.wait()?;
        // See if the fast semaphore has a lower p90 than the slow semaphore,
        // which would indicate that in general it has a tighter deadline. Note
        // that we only collect the one way semaphore, not the return.
        let mut d_fast = Distribution::<Duration>::default();
        fast_sem.collect_wake_stats(&mut d_fast);
        let mut d_slow = Distribution::<Duration>::default();
        slow_sem.collect_wake_stats(&mut d_slow);
        let fast_est = d_fast.estimates();
        let slow_est = d_slow.estimates();
        let percentile_frac = percentile as f64 / 100.0;
        let pxx_fast = fast_est
            .percentile(percentile_frac)
            .ok_or_else(|| anyhow::anyhow!("No p{percentile} for fast semaphore"))?;
        let pxx_slow = slow_est
            .percentile(percentile_frac)
            .ok_or_else(|| anyhow::anyhow!("No p{percentile} for slow semaphore"))?;
        eprintln!("Fast semaphore wake estimates (p{percentile} = {:.3}ms):", pxx_fast.as_secs_f64() * 1000.0);
        eprintln!("{}", fast_est.visualize(None, None));
        eprintln!("Slow semaphore wake estimates (p{percentile} = {:.3}ms):", pxx_slow.as_secs_f64() * 1000.0);
        eprintln!("{}", slow_est.visualize(None, None));
        let ratio = pxx_slow.as_secs_f64() / pxx_fast.as_secs_f64();
        Ok(ratio)
    };

    // If this ratio converges below 1.0 after the mininum time, that means that
    // we don't really have a preference for waking the process on the fast
    // semaphore (at the p90), and therefore it fails the test.
    let target = 1.00; // Slow semaphore is slower than fast semaphore.
    let final_value = converge(
        Some(Duration::from_secs_f64(5.0)),
        Some(Duration::from_secs_f64(10.0)),
        Some(target),
        metric,
    )?;
    if final_value < target {
        Err(anyhow::anyhow!(
            "Failed to achieve target: got {:.2}, expected {:.2}",
            final_value,
            target
        ))
    } else {
        Ok(())
    }
}

test!("adaptive_priority", adaptive_priority);
