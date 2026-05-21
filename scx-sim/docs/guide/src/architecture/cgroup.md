# Cgroup Modeling

> **Status — stub.** Will describe the cgroup hierarchy modeling — the
> `scx_cgroup_tree` crate, the cgroup-bandwidth accounting timer, and
> the replenishment top-half / throttle bottom-half split.

Key sources:

- [`crates/scx_cgroup_tree/`](https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/crates/scx_cgroup_tree)
- [`safe/cgroup.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/cgroup.rs)
- [`unsafe_impl/cgroup_ffi.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl/cgroup_ffi.rs)
- [`unsafe_impl/cgroup_wrapper.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl/cgroup_wrapper.rs)
