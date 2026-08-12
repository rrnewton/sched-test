/*
 * Concrete DSQ-iterator glue for bpf_for_each(scx_dsq, ...).
 *
 * bpf_for_each(scx_dsq) references bpf_iter_scx_dsq_destroy's ADDRESS via the
 * cleanup() attribute (a no-paren reference a function-like macro cannot
 * satisfy), so the iterator triple must be concrete, address-takeable
 * functions. This translation unit is compiled into every scheduler .so
 * (build.rs full_srcs) -- per-.so source compilation, so each .so gets its own
 * addressable symbols (source dedup, not link dedup; the cleanup-attr
 * address-take resolves per-.so).
 *
 * The bodies wrap the simulator's sim_dsq_iter_begin/next kfuncs:
 * opaque[0] holds the cursor (begin()'s first task), opaque[1] is a first-element
 * flag so the first next() returns begin()'s result before advancing.
 */
#include "sim_wrapper.h"

int bpf_iter_scx_dsq_new(struct bpf_iter_scx_dsq *it, u64 dsq_id, u64 flags)
{
	u64 *opaque = (u64 *)it;

	opaque[0] = (u64)(unsigned long)sim_dsq_iter_begin(dsq_id, flags);
	opaque[1] = 1;
	return 0;
}

struct task_struct *bpf_iter_scx_dsq_next(struct bpf_iter_scx_dsq *it)
{
	u64 *opaque = (u64 *)it;

	if (opaque[1]) {
		opaque[1] = 0;
		return (struct task_struct *)(unsigned long)opaque[0];
	}

	return (struct task_struct *)sim_dsq_iter_next();
}

void bpf_iter_scx_dsq_destroy(struct bpf_iter_scx_dsq *it)
{
	while (bpf_iter_scx_dsq_next(it))
		;
}
