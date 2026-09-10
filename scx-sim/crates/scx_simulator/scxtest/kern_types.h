#pragma once

#include <linux/types.h>

/*
 * NR_CPUS — the largest machine the simulator will model.
 *
 * SINGLE SOURCE OF TRUTH. Three translation units implement pieces of the
 * cpumask substrate — `csrc/sim_bpf_stubs.c` (the strong `bpf_cpumask_*`
 * kfuncs linked into every scheduler `.so`), `scxtest/overrides.c` (weak
 * fallbacks for the same), and `scxtest/scx_test_cpumask.c` (the engine's
 * idle/all-CPU masks and `scx_bpf_pick_*`). All three include this header.
 * They previously each carried their own `#ifndef NR_CPUS / #define 128`,
 * which is how a 384-CPU run came up, exited Normal, populated both layers
 * and still placed every task on CPUs 0-127: two of the three were raised
 * and the third was missed. Every guard here is `if (cpu < NR_CPUS)` with no
 * else, so the failure mode is a silently wrong answer, not an error.
 *
 * 512 is not arbitrary. It is simultaneously:
 *   - scx_layered's own `MAX_CPUS = 1 << MAX_CPUS_SHIFT` (its intf.h), the
 *     bound on the `u16 cpus[MAX_CPUS]` proximity maps it indexes by CPU id;
 *   - the `CONFIG_NR_CPUS=512` of the fbk kernel that upstream scx PR 3718's
 *     384-CPU repro boots on.
 * Raising it past 512 requires re-checking the scheduler side first.
 *
 * The `struct cpumask` layouts in those TUs are deliberately `bits[128]`
 * (8192 bits) to match vmlinux.h, which every scheduler is compiled against;
 * only the first NR_CPUS bits are ever populated. So NR_CPUS bounds the
 * GUARDS, not the allocation, and may be raised without a layout change.
 */
#ifndef NR_CPUS
#define NR_CPUS 512
#endif

typedef __u64 u64;
typedef __u32 u32;
typedef __u16 u16;
typedef __u8 u8;
typedef __s64 s64;
typedef __s32 s32;
typedef __s16 s16;
typedef __s8 s8;
