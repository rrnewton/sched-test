//! In-crate relocation of the former `tests/pending_dispatch.rs`.
//!
//! This is an end-to-end engine test: a custom [`Scheduler`](crate::Scheduler)
//! whose `dispatch` callback calls the kfunc `scx_bpf_dsq_insert` twice, driven
//! through `Simulator::run`, asserting both tasks schedule. The kfuncs are the
//! sim's in-crate default implementations (crate-internal, not public API), so
//! the test lives in-crate to reach `crate::kfuncs::scx_bpf_dsq_insert`. It is
//! integration-shaped and would move back to `tests/` once an ergonomic
//! test-time kfunc-override surface exists.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;

use crate::{
    DsqId, Phase, Pid, RepeatMode, Scenario, Scheduler, Simulator, TaskBehavior, TaskDef, TraceKind,
};

#[derive(Default)]
struct MultiInsertDispatchScheduler {
    queued: RefCell<VecDeque<*mut c_void>>,
}

impl Scheduler for MultiInsertDispatchScheduler {
    unsafe fn init(&self) -> i32 {
        0
    }

    unsafe fn select_cpu(&self, _p: *mut c_void, prev_cpu: i32, _wake_flags: u64) -> i32 {
        prev_cpu
    }

    unsafe fn enqueue(&self, p: *mut c_void, _enq_flags: u64) {
        self.queued.borrow_mut().push_back(p);
    }

    unsafe fn dispatch(&self, _cpu: i32, _prev: *mut c_void) {
        if self.queued.borrow().len() < 2 {
            return;
        }

        let mut queued = self.queued.borrow_mut();
        for _ in 0..2 {
            let p = queued.pop_front().unwrap();
            crate::kfuncs::scx_bpf_dsq_insert(p, DsqId::LOCAL.0, 1_000_000, 0);
        }
    }

    unsafe fn running(&self, _p: *mut c_void) {}

    unsafe fn stopping(&self, _p: *mut c_void, _runnable: bool) {}

    unsafe fn enable(&self, _p: *mut c_void) {}
}

fn one_shot_task(pid: i32, name: &str) -> TaskDef {
    TaskDef {
        name: name.to_owned(),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(10_000)],
            repeat: RepeatMode::Once,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

#[test]
fn dispatch_callback_preserves_multiple_dsq_inserts() {
    let scenario = Scenario::builder()
        .cpus(1)
        .fixed_priority(true)
        .no_watchdog()
        .task(one_shot_task(1, "first"))
        .task(one_shot_task(2, "second"))
        .duration_ms(1)
        .build();

    let trace = Simulator::new(MultiInsertDispatchScheduler::default()).run(scenario);
    let scheduled: Vec<_> = trace
        .events()
        .iter()
        .filter_map(|event| match event.kind {
            TraceKind::TaskScheduled { pid } => Some(pid),
            _ => None,
        })
        .collect();

    assert_eq!(scheduled, vec![Pid(1), Pid(2)]);
    assert!(trace.total_runtime(Pid(1)) > 0);
    assert!(trace.total_runtime(Pid(2)) > 0);
}
