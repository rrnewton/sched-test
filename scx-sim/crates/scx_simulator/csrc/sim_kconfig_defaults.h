/*
 * sim_kconfig_defaults.h - default values for the kernel-config / version
 * scalars the SCX schedulers read as __kconfig externs.
 *
 * These are the userspace stand-ins for the kernel's __kconfig globals
 * (LINUX_KERNEL_VERSION, CONFIG_PREEMPT_RCU, CONFIG_HZ, CONFIG_NO_HZ_IDLE). The
 * standalone build compiles them with these defaults; an embedder driving a real
 * kernel passes the kernel-under-test's values via build_schedulers'
 * KernelConfig, which emits -DSIM_<NAME>=<value> so the #ifndef guards below
 * yield to the override. Defaults are integer-encoded (1/0 for the bools) so the
 * header and the -D flag share one encoding regardless of whether <stdbool.h> is
 * in scope at the include site.
 *
 * Single source of truth: LINUX_KERNEL_VERSION is defined in BOTH a .so TU
 * (sim_bpf_stubs.c) and the host static lib (sim_task.c); both include this
 * header so the two definitions cannot diverge.
 *
 * No includes, no types: this header is included from both kern_types.h-world
 * TUs (sim_bpf_stubs.c, sim_task.c) and sim_wrapper.h-world TUs (the scheduler
 * wrapper.c files), which have incompatible include topologies.
 */
#ifndef SIM_KCONFIG_DEFAULTS_H
#define SIM_KCONFIG_DEFAULTS_H

/*
 * Kernel version the migrate-disable / compat logic in scx common.bpf.h branches
 * on, encoded major<<16 | minor<<8 | patch. 0x061200 == 6.18.0, deliberately >=
 * KERNEL_VERSION(6,18,0) so the un-shadowed is_migration_disabled fast path
 * treats migration_disabled==1 as a real disable.
 */
#ifndef SIM_LINUX_KERNEL_VERSION
#define SIM_LINUX_KERNEL_VERSION 0x061200
#endif

/* CONFIG_PREEMPT_RCU: paired with LINUX_KERNEL_VERSION in is_migration_disabled. */
#ifndef SIM_CONFIG_PREEMPT_RCU
#define SIM_CONFIG_PREEMPT_RCU 0
#endif

/* CONFIG_HZ: tick frequency tickless falls back to when tick_freq is 0. */
#ifndef SIM_CONFIG_HZ
#define SIM_CONFIG_HZ 250
#endif

/*
 * CONFIG_NO_HZ_IDLE (gates lavd's sys_stat idle-drift branch) is NOT given a
 * default here: its historical definition in lavd/wrapper.c is a TENTATIVE
 * definition (`bool CONFIG_NO_HZ_IDLE;`, 0-init in .bss). An `= <default>` would
 * reorder lavd's .bss and change the .so bytes, so the wrapper keeps the
 * tentative form by default and switches to `= SIM_CONFIG_NO_HZ_IDLE` only when
 * an embedder defines it (-DSIM_CONFIG_NO_HZ_IDLE=1). Byte-neutral standalone.
 */

#endif /* SIM_KCONFIG_DEFAULTS_H */
