/*
 * sim_wrapper.h - Wrapper header for compiling BPF schedulers as userspace C
 *
 * This header must be included BEFORE the scheduler's .bpf.c file.
 * It sets up the test infrastructure from lib/scxtest/, includes
 * common.bpf.h (to set its header guard), then overrides BPF macros
 * to produce regular C functions callable from the simulator.
 */
#pragma once

/* Pull in the unit test infrastructure (overrides, map emulation, cpumask) */
#include <scx_test.h>
#include <scx_test_map.h>
#include <scx_test_cpumask.h>

/*
 * Override BPF section attributes that cause linker conflicts in userspace.
 * __kconfig puts variables in a special .kconfig section; without override,
 * weak __kconfig declarations (like CONFIG_NO_HZ_IDLE) resolve to address
 * 0 in -nostdlib .so files, causing SIGSEGV on access.
 */
#undef __kconfig
#define __kconfig

/*
 * CO-RE type-info builtin stub — must be defined BEFORE including
 * common.bpf.h, because bpf_core_read.h and compat.bpf.h use
 * __builtin_preserve_type_info() in bpf_core_type_exists() macros.
 * Without this, the builtin is undefined during header parsing,
 * producing implicit-function-declaration errors (clang 21+ hard
 * error) and returning an undefined value instead of the intended 1.
 */
#define __builtin_preserve_type_info(x,y) 1

/* Include common.bpf.h to get type definitions and set the header guard.
 * When the scheduler .bpf.c re-includes it, it will be skipped. */
#include <scx/common.bpf.h>

/*
 * CO-RE helper overrides for userspace compilation.
 *
 * bpf_core_read.h (included transitively via common.bpf.h) defines these
 * using __builtin_preserve_access_index / CO-RE builtins. In the simulator
 * we need plain userspace implementations. We #undef after include to
 * avoid redefinition warnings.
 */
#undef bpf_core_read
#define bpf_core_read(dst, sz, src) (__builtin_memcpy(dst, src, sz), (int)0)
#undef bpf_core_cast
#define bpf_core_cast(ptr, type) ((type *)(ptr))
#undef bpf_core_type_matches
#define bpf_core_type_matches(type) 1
#undef bpf_core_type_size
#define bpf_core_type_size(type) sizeof(type)

/*
 * Undo BPF CO-RE enum variable macros from enums.autogen.bpf.h.
 *
 * In BPF programs, these constants are resolved at load time via CO-RE
 * relocation. enums.autogen.bpf.h redefines each SCX_* enum constant as
 * a weak variable (__SCX_*) which the BPF loader patches. In userspace,
 * these weak variables default to 0 — breaking the scheduler logic.
 *
 * By undefining the macros, the compiler falls back to the real enum
 * values from vmlinux.h.
 */
#undef SCX_OPS_NAME_LEN
#undef SCX_SLICE_DFL
#undef SCX_SLICE_INF
#undef SCX_RQ_ONLINE
#undef SCX_RQ_CAN_STOP_TICK
#undef SCX_RQ_BAL_PENDING
#undef SCX_RQ_BAL_KEEP
#undef SCX_RQ_BYPASSING
#undef SCX_RQ_CLK_VALID
#undef SCX_RQ_IN_WAKEUP
#undef SCX_RQ_IN_BALANCE
#undef SCX_DSQ_FLAG_BUILTIN
#undef SCX_DSQ_FLAG_LOCAL_ON
#undef SCX_DSQ_INVALID
#undef SCX_DSQ_GLOBAL
#undef SCX_DSQ_LOCAL
#undef SCX_DSQ_LOCAL_ON
#undef SCX_DSQ_LOCAL_CPU_MASK
#undef SCX_TASK_QUEUED
#undef SCX_TASK_RESET_RUNNABLE_AT
#undef SCX_TASK_DEQD_FOR_SLEEP
#undef SCX_TASK_STATE_SHIFT
#undef SCX_TASK_STATE_BITS
#undef SCX_TASK_STATE_MASK
#undef SCX_TASK_CURSOR
#undef SCX_TASK_NONE
#undef SCX_TASK_INIT
#undef SCX_TASK_READY
#undef SCX_TASK_ENABLED
#undef SCX_TASK_NR_STATES
#undef SCX_TASK_DSQ_ON_PRIQ
#undef SCX_KICK_IDLE
#undef SCX_KICK_PREEMPT
#undef SCX_KICK_WAIT
#undef SCX_ENQ_WAKEUP
#undef SCX_ENQ_HEAD
#undef SCX_ENQ_PREEMPT
#undef SCX_ENQ_REENQ
#undef SCX_ENQ_LAST
#undef SCX_ENQ_CLEAR_OPSS
#undef SCX_ENQ_DSQ_PRIQ

/*
 * Undo compat macros from compat.bpf.h.
 *
 * compat.bpf.h wraps kfunc calls like scx_bpf_dsq_insert() with
 * bpf_ksym_exists() ternary expressions that fall back to ___compat
 * variants for older kernels. In the simulator, we provide the kfuncs
 * directly as #[no_mangle] Rust functions — no compat indirection needed.
 */
#undef scx_bpf_dsq_insert
#undef scx_bpf_dsq_insert_vtime
#undef scx_bpf_dsq_move_to_local
#undef scx_bpf_task_cgroup
#undef scx_bpf_now

/*
 * Forward-declare kfuncs provided by the Rust binary via #[no_mangle].
 * After the #undef above removes compat macro wrappers, C callers need
 * plain function declarations; symbols are resolved from the Rust binary
 * at dlopen time via -rdynamic.
 */
extern bool scx_bpf_dsq_move_to_local(u64 dsq_id);
extern struct cgroup *scx_bpf_task_cgroup(void *p, int subsys_id);

/*
 * Legacy compat alias used by older scheduler snapshots (e.g. mitosis).
 * __COMPAT_scx_bpf_task_cgroup(p) was the old 1-arg compat wrapper;
 * the Rust kfunc takes (task, subsys_id) — default subsys_id=0.
 */
#ifndef __COMPAT_scx_bpf_task_cgroup
#define __COMPAT_scx_bpf_task_cgroup(p) scx_bpf_task_cgroup((p), 0)
#endif

/*
 * Override BPF_STRUCT_OPS to produce regular C functions.
 * In BPF mode, BPF_STRUCT_OPS wraps functions with SEC annotations and
 * BPF_PROG argument unpacking. In simulator mode, we just want plain
 * C functions with typed arguments.
 */
#undef BPF_STRUCT_OPS
#define BPF_STRUCT_OPS(name, args...) \
    __attribute__((used)) name(args)

#undef BPF_STRUCT_OPS_SLEEPABLE
#define BPF_STRUCT_OPS_SLEEPABLE(name, args...) \
    __attribute__((used)) name(args)

/* SCX_OPS_DEFINE creates a struct_ops registration - not needed in simulator */
#undef SCX_OPS_DEFINE
#define SCX_OPS_DEFINE(name, ...)
