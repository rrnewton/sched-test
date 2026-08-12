/*
 * sim_bpf_ub_semantics_test.c — assert scxsim reproduces BPF division and
 * modulo semantics for the cases where native C says "undefined behaviour".
 *
 * Expected values come from the fixup patchlets the kernel verifier itself
 * inserts around every runtime-variable divisor (kernel/bpf/verifier.c,
 * chk_and_div / chk_and_mod / chk_and_sdiv / chk_and_smod):
 *
 *     x div  0  -> 0            x mod  0  -> x
 *     x sdiv 0  -> 0            x smod 0  -> x
 *     LLONG_MIN sdiv -1 -> LLONG_MIN
 *     INT_MIN   sdiv -1 -> INT_MIN
 *     x smod -1 -> 0
 *
 * See ai_docs/BPF_UB_FIDELITY_POLICY.md.
 *
 * Every operand is volatile so the compiler cannot constant-fold the UB away,
 * and every RESULT is routed through a volatile sink before being compared.
 * The sink is load-bearing, not decoration: at -O2 clang rewrites `a / b == 0`
 * into `b > a`, which is valid only under the C assumption that b is nonzero.
 * Without the sink this test would report a failure that is the optimiser's
 * doing rather than the handler's, and would obscure what it is measuring.
 */

#include <limits.h>
#include <stdint.h>
#include <stdio.h>

void sim_install_sigfpe_handler(void);

static volatile int32_t s32_zero = 0, s32_minus1 = -1, s32_val = 7;
static volatile int32_t s32_min = INT_MIN;
static volatile int64_t s64_zero = 0, s64_minus1 = -1, s64_val = 7;
static volatile int64_t s64_min = LLONG_MIN;
static volatile uint32_t u32_zero = 0, u32_val = 7;
static volatile uint64_t u64_zero = 0, u64_val = 7;

static volatile long long sink;
static int failures;

#define CHECK(label, expr, want)                                              \
	do {                                                                  \
		long long got_, want_;                                        \
		sink = (long long)(expr);                                     \
		got_ = sink;                                                  \
		want_ = (long long)(want);                                    \
		printf("  %-24s got=%-21lld want=%-21lld %s\n", (label), got_, \
		       want_, got_ == want_ ? "ok" : "FAIL");                 \
		if (got_ != want_)                                            \
			failures++;                                           \
	} while (0)

int main(void)
{
	sim_install_sigfpe_handler();

	printf("BPF division/modulo semantics under native C:\n");

	CHECK("u32 div 0 -> 0", u32_val / u32_zero, 0);
	CHECK("u32 mod 0 -> x", u32_val % u32_zero, 7);
	CHECK("u64 div 0 -> 0", u64_val / u64_zero, 0);
	CHECK("u64 mod 0 -> x", u64_val % u64_zero, 7);

	CHECK("s32 sdiv 0 -> 0", s32_val / s32_zero, 0);
	CHECK("s32 smod 0 -> x", s32_val % s32_zero, 7);
	CHECK("s64 sdiv 0 -> 0", s64_val / s64_zero, 0);
	CHECK("s64 smod 0 -> x", s64_val % s64_zero, 7);

	CHECK("INT_MIN sdiv -1", s32_min / s32_minus1, INT_MIN);
	CHECK("INT_MIN smod -1", s32_min % s32_minus1, 0);
	CHECK("LLONG_MIN sdiv -1", s64_min / s64_minus1, LLONG_MIN);
	CHECK("LLONG_MIN smod -1", s64_min % s64_minus1, 0);

	if (failures) {
		printf("FAIL: %d of 12 BPF division semantics not reproduced\n",
		       failures);
		return 1;
	}
	printf("PASS: 12/12 BPF division semantics reproduced\n");
	return 0;
}
