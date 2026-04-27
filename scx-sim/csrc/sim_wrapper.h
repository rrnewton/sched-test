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
/*
 * Return 0 so bpf_core_type_exists() reports types as absent.
 * This makes compat.bpf.h inline functions fall back to the older
 * (non-struct-args) kfunc variants that the simulator already supports.
 * bpf_core_field_exists() also uses this, so it returns 0 too — fine
 * because the simulator's task_struct fields are accessed differently.
 */
#define __builtin_preserve_type_info(x,y) 0
#define __builtin_preserve_field_info(x,y) 0

/* Include common.bpf.h to get type definitions and set the header guard.
 * When the scheduler .bpf.c re-includes it, it will be skipped. */
#include <scx/common.bpf.h>

/*
 * The simulator does not provide the kernel's numeric iterator kfuncs
 * (bpf_iter_num_new/next/destroy) that back bpf_for(). In userspace we do
 * not need verifier proofs, so translate bpf_for() into a plain C loop.
 */
#undef bpf_for
#define bpf_for(i, start, end) for ((i) = (start); (i) < (end); ++(i))

/*
 * CO-RE helper overrides for userspace compilation.
 *
 * bpf_core_read.h (included transitively via common.bpf.h) defines these
 * using __builtin_preserve_access_index / CO-RE builtins. In the simulator
 * we need plain userspace implementations. We #undef after include to
 * avoid redefinition warnings.
 */
/*
 * BPF helper overrides: bpf_helper_defs.h defines these as static function
 * pointers initialized to (void *)HELPER_NUMBER. Calling those addresses
 * SIGSEGVs. Override with safe userspace equivalents.
 */
#undef bpf_probe_read_kernel_str
#define bpf_probe_read_kernel_str(dst, sz, src) \
	({ long __ret = 0; if ((src)) __builtin_strncpy((dst), (src), (sz)); \
	   else __builtin_memset((dst), 0, (sz)); __ret; })

#undef bpf_probe_read_kernel
#define bpf_probe_read_kernel(dst, sz, src) \
	({ __builtin_memset((dst), 0, (sz)); (long)(-14); })

extern void *bpf_kptr_xchg_impl(void **kptr, void *new_val);
#undef bpf_kptr_xchg
#define bpf_kptr_xchg(kptr, val) \
	bpf_kptr_xchg_impl((void **)(kptr), (void *)(val))

/*
 * Time helpers: bpf_ktime_get_ns returns simulated clock (just 0 for now).
 * These are frequently used by schedulers for time comparisons.
 */
extern unsigned long long sim_bpf_ktime_get_ns(void);
#undef bpf_ktime_get_ns
#define bpf_ktime_get_ns() sim_bpf_ktime_get_ns()

/*
 * bpf_get_smp_processor_id: return current CPU id.
 * Already provided by sim_bpf_get_smp_processor_id in the simulator.
 */
extern unsigned int sim_bpf_get_smp_processor_id(void);
#undef bpf_get_smp_processor_id
#define bpf_get_smp_processor_id() sim_bpf_get_smp_processor_id()

/*
 * bpf_this_cpu_ptr / bpf_per_cpu_ptr: per-CPU variable access.
 * In simulation, return NULL (callers should check).
 */
#undef bpf_this_cpu_ptr
#define bpf_this_cpu_ptr(ptr) (ptr)
#undef bpf_per_cpu_ptr
#define bpf_per_cpu_ptr(ptr, cpu) ((typeof(ptr))0)

/*
 * bpf_get_current_task: return raw current task pointer.
 * Route through our kfunc implementation.
 */
#undef bpf_get_current_task
#define bpf_get_current_task() ((long)bpf_get_current_task_btf_kfunc())

#undef bpf_repeat
#define bpf_repeat(n) for (int ___i = 0; ___i < (n); ___i++)

#undef bpf_core_read
#define bpf_core_read(dst, sz, src) (__builtin_memcpy(dst, src, sz), (int)0)
#undef bpf_core_cast
#define bpf_core_cast(ptr, type) ((type *)(ptr))
#undef bpf_core_type_matches
#define bpf_core_type_matches(type) 1
#undef bpf_core_type_size
#define bpf_core_type_size(type) sizeof(type)

/*
 * bpf_get_current_task_btf: BPF helper returning current task_struct *.
 * In bpf_helper_defs.h, it's a static function pointer initialized to NULL.
 * Override to call the Rust kfunc via -rdynamic. The Rust binary exports
 * bpf_get_current_task_btf as #[no_mangle] extern "C".
 */
extern void *bpf_get_current_task_btf_kfunc(void);
#undef bpf_get_current_task_btf
#define bpf_get_current_task_btf() ((struct task_struct *)bpf_get_current_task_btf_kfunc())

/*
 * bpf_probe_read_kernel_str — userspace stub.
 *
 * BPF helper #115 is used by simple_exit() to copy the scheduler name.
 * Without this stub, the call goes through the raw BPF helper dispatch
 * table (slot 115), which is NULL in userspace → SIGSEGV.
 */
static __always_inline long sim_bpf_probe_read_kernel_str(void *dst, u32 sz,
							  const void *src)
{
	char *out = dst;
	const char *in = src;
	u32 i;

	if (!dst || sz == 0)
		return 0;

	__builtin_memset(dst, 0, sz);
	if (!src)
		return -14;

	for (i = 0; i + 1 < sz && in[i]; i++)
		out[i] = in[i];

	return i + 1;
}

#undef bpf_probe_read_kernel_str
#define bpf_probe_read_kernel_str(dst, sz, src) \
	sim_bpf_probe_read_kernel_str((dst), (sz), (src))

static __always_inline void *sim_bpf_kptr_xchg(void **kptr, void *new_val)
{
	void *old = *kptr;
	*kptr = new_val;
	return old;
}

#undef bpf_kptr_xchg
#define bpf_kptr_xchg(kptr, val) \
	sim_bpf_kptr_xchg((void **)(kptr), (void *)(val))

extern void *sim_bpf_get_current_task_btf(void);
#undef bpf_get_current_task_btf
#define bpf_get_current_task_btf() sim_bpf_get_current_task_btf()

extern u32 sim_bpf_get_smp_processor_id(void);
#undef bpf_get_smp_processor_id
#define bpf_get_smp_processor_id() sim_bpf_get_smp_processor_id()

extern u64 sim_bpf_ktime_get_ns(void);
#undef bpf_ktime_get_ns
#define bpf_ktime_get_ns() sim_bpf_ktime_get_ns()

extern void *sim_dsq_iter_begin(u64 dsq_id, u64 flags);
extern void *sim_dsq_iter_next(void);

static __always_inline int sim_bpf_iter_scx_dsq_new(struct bpf_iter_scx_dsq *it,
						    u64 dsq_id, u64 flags)
{
	u64 *opaque = (u64 *)it;

	opaque[0] = (u64)(unsigned long)sim_dsq_iter_begin(dsq_id, flags);
	opaque[1] = 1;
	return 0;
}

static __always_inline struct task_struct *
sim_bpf_iter_scx_dsq_next(struct bpf_iter_scx_dsq *it)
{
	u64 *opaque = (u64 *)it;

	if (opaque[1]) {
		opaque[1] = 0;
		return (struct task_struct *)(unsigned long)opaque[0];
	}

	return (struct task_struct *)sim_dsq_iter_next();
}

static __always_inline void
sim_bpf_iter_scx_dsq_destroy(struct bpf_iter_scx_dsq *it)
{
	while (sim_bpf_iter_scx_dsq_next(it))
		;
}

#undef bpf_iter_scx_dsq_new
#define bpf_iter_scx_dsq_new(it, dsq_id, flags) \
	sim_bpf_iter_scx_dsq_new((it), (dsq_id), (flags))

#undef bpf_iter_scx_dsq_next
#define bpf_iter_scx_dsq_next(it) sim_bpf_iter_scx_dsq_next((it))

#undef bpf_iter_scx_dsq_destroy
#define bpf_iter_scx_dsq_destroy(it) sim_bpf_iter_scx_dsq_destroy((it))

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
extern bool __sim_dsq_move_to_local(u64 dsq_id);
/* v2 compat: upstream now passes (dsq_id, enq_flags); ignore enq_flags in sim */
#define scx_bpf_dsq_move_to_local(dsq_id, ...) __sim_dsq_move_to_local(dsq_id)
extern struct cgroup *__sim_task_cgroup(void *p, int subsys_id);
/*
 * Newer compat headers route scx_bpf_select_cpu_and() through the
 * __scx_bpf_select_cpu_and() ABI. The simulator only exports the legacy
 * 5-argument form, so bridge the new ABI back onto that symbol.
 */
extern s32 sim_scx_bpf_select_cpu_and(struct task_struct *p, s32 prev_cpu,
				      u64 wake_flags,
				      const struct cpumask *cpus_allowed,
				      u64 flags)
    __asm__("scx_bpf_select_cpu_and");
#undef __scx_bpf_select_cpu_and
#define __scx_bpf_select_cpu_and(p, cpus_allowed, args) \
	sim_scx_bpf_select_cpu_and((p), (args)->prev_cpu, (args)->wake_flags, \
				 (cpus_allowed), (args)->flags)

/*
 * Upstream moved from __COMPAT_scx_bpf_task_cgroup(p) to a 1-arg
 * scx_bpf_task_cgroup(p) compat macro. Our Rust kfunc takes (task,
 * subsys_id). Default subsys_id=0 for the 1-arg callers.
 */
#define scx_bpf_task_cgroup(p) __sim_task_cgroup((void *)(p), 0)
#define __COMPAT_scx_bpf_task_cgroup(p) __sim_task_cgroup((void *)(p), 0)

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
