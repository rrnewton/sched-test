---
title: 'Unsound safe public API: free_raw, task_get_utime/stime and scx_bpf_task_cgroup crash a #![forbid(unsafe_code)] caller'
status: open
priority: 1
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.620260036+00:00
updated_at: 2026-09-25T03:43:12.620260036+00:00
---

# Description

Four functions reachable from scx_simulator's crate root take a raw pointer and dereference or free it, and none of them is an `unsafe fn`. A safe Rust function must not cause undefined behaviour for any argument. A consumer file under `#![forbid(unsafe_code)]` crashes the process five ways through them.

Measured on the release-candidate branch (integration 24d864c6) with Rust 1.97.1, built against the extracted .crate files:

| call (no `unsafe` anywhere in the caller) | result |
|---|---|
| `reg.free_raw(reg.root_raw())` | SIGABRT, `free(): invalid pointer` |
| `reg.free_raw(reg.get_raw(id).unwrap())`, then `reg` drops | SIGABRT, `free(): double free detected in tcache 2` |
| `reg.destroy_by_name("a")`, then `free_raw` on the result twice | SIGABRT, `free(): double free detected in tcache 2` |
| `task_get_utime(std::ptr::null_mut())` | SIGSEGV |
| `scx_bpf_task_cgroup(std::ptr::dangling_mut(), 0)` | SIGSEGV |

The shortest reproducer uses only values the API hands out:

```rust
#![forbid(unsafe_code)]
fn main() {
    let reg = scx_simulator::CgroupRegistry::new(4, 16);
    reg.free_raw(reg.root_raw()); // free(): invalid pointer, SIGABRT
}
```

The four functions:
1. `CgroupRegistry::free_raw(&self, raw: *mut c_void)`. It calls `free_cgroup_raw`, which calls the C `sim_cgroup_free`. That frees `cgrp->kn`, the task_group, the cpuset if there is one, and `cgrp`. The doc comment states a precondition, 'Must only be called with a pointer returned from `destroy_by_name`, and only after `cgroup_exit` has been called for that cgroup', and nothing enforces it. `root_raw()` and `get_raw()` on the same type hand out pointers that break it. The root cgroup, its kernfs node and its task_group are C statics (`sim_root_cgroup`, `sim_root_kn`, `sim_root_task_group` in csrc/sim_task.c), so the first row passes static storage to `free()`.
2. `task_get_utime` and `task_get_stime`. Each body is `unsafe { sim_task_get_utime(raw) }` (or the stime equivalent), and the C side is `return p->utime;`.
3. `scx_bpf_task_cgroup(p, subsys_id)`, a `#[no_mangle] pub extern "C"` kfunc. It returns NULL for a NULL `p` and dereferences any other value through `sim_task_get_cgroup`.

The crate's own reasoning holds only for callers inside the crate:
- The block comment above ffi.rs's safe wrappers says they suppress `clippy::not_unsafe_ptr_arg_deref` because they are 'intentionally safe wrappers', and that 'the engine guarantees that all raw pointers passed to these functions point to valid, live C structs'.
- kfuncs.rs allows the same lint for the whole module because its callers are C code and 'marking them `unsafe` in Rust would be meaningless'.
- Both arguments are true while the engine and the BPF code are the only callers. Three root re-exports add Rust callers outside the crate: `pub use ffi::{task_get_stime, task_get_utime}`, `pub use unsafe_impl::kfuncs::scx_bpf_task_cgroup`, and `CgroupRegistry` itself with its pub `free_raw`. For those callers, neither argument holds.
- The crate has 25 per-function allows plus kfuncs.rs's module-wide one. Three of the 25 are on the reachable paths: task_get_utime, task_get_stime, and free_cgroup_raw under free_raw. The other 22 (18 in ffi.rs, 3 in task_wrapper.rs, 1 in scheduler_wrapper.rs) sit in the pub(crate) `safe` and `unsafe_impl` modules and are not reachable from outside. The public `Scheduler` trait already declares its pointer-taking callbacks, such as select_cpu, enqueue, dispatch, running and stopping, as `unsafe fn`, so the crate follows the right pattern where it thought about external callers.

In-repo callers of the three root re-exports: tests/cputime_accounting.rs (task_get_utime, task_get_stime) and the NULL-contract case in tests/scx_bpf_helpers.rs (scx_bpf_task_cgroup). `free_raw`'s only caller is engine.rs.

Fix before publishing. After publication every one of these is a breaking change.
- `free_raw`: make it pub(crate), since its only caller is the engine, or make it `unsafe fn` with a `# Safety` section. The clean end state is for `destroy_by_name` to return an owning handle that frees on drop. Most of that RAII work is already done: `CgroupInfo` holds a private `CgroupAlloc`, whose `Owned` variant is a `SimCgroupHandle` that frees in `Drop`. `destroy_by_name` is where ownership leaks back out, because it calls `into_raw()` so the engine can run `cgroup_exit` before the free. Returning the `SimCgroupHandle` instead, with a `raw()` accessor for the `cgroup_exit` call, closes the last gap and removes the need for `free_raw`.
- `task_get_utime`, `task_get_stime`: declare them `pub unsafe fn`, and the one test gains an `unsafe` block. Or drop the root re-export.
- `scx_bpf_task_cgroup`: declare it `pub unsafe extern "C" fn`, or drop the root re-export. BPF code binds the symbol by name, so C callers are unaffected. The NULL-contract test gains an `unsafe` block.
- Drop the allows from the functions that stay reachable, and replace kfuncs.rs's module-wide allow with per-function ones. clippy can then flag the next pointer-taking wrapper that gets re-exported.

sim-0qpv9 is the same class of defect one level up: `SIM_LOCK` is a precondition of the safe `DynamicScheduler`/`Simulator` API that only a doc comment states, and breaking it corrupts the heap from code with no `unsafe` in it.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
