/*
 * sim_cgroup.c - Cgroup CSS iterator for the simulator
 *
 * Implements the CSS-iter machinery that backs `bpf_for_each(css, pos,
 * root, flags)` for scheduler/library code compiled into scxsim.
 *
 * The Rust side (`safe::cgroup::CgroupRegistry::prepare_css_iter`)
 * pre-populates two flat arrays of cgroup pointers --
 *   * `sim_css_list_pre[]`  in pre-order  (parent before children)
 *   * `sim_css_list_post[]` in post-order (children before parent)
 * -- before the BPF callback fires. C-side iteration then walks the
 * appropriate array based on the flags passed to
 * `bpf_iter_css_new(it, start, flags)`.
 *
 * Phase 1 BPF infra scale-up (tg `scxsim-bpf-infra-scale-up-phase1`,
 * design doc `experiments/lavd_cpubw_stalls_202604/SCXSIM_REAL_CGROUP_BW_LIBRARY_DESIGN.md`
 * section Phase 1 item 3):
 *   1. Bumped `MAX_CSS_ITER_CGROUPS` 256 -> 2 048 to cover the
 *      production cgroup_bw `CBW_NR_CGRP_MAX` ceiling and the
 *      cpu-bw-stall-bug high-cgroup-count stress matrix.
 *   2. Added `sim_css_list_post[]` + `sim_css_iter_add_post()` so
 *      Phase 2's compiled-in `cgroup_bw.bpf.c` can iterate
 *      `BPF_CGROUP_ITER_DESCENDANTS_POST` (used at lib/cgroup_bw.bpf.c
 *      lines 1186, 1318, 1874 to walk the bandwidth-control tree
 *      bottom-up during charge / replenish).
 *   3. Added `sim_bpf_iter_css_new/_next/_destroy` -- a flags-aware
 *      CSS iterator that picks the right pre-populated array. It is
 *      invoked transparently by the BPF macro
 *      `bpf_for_each(css, pos, root, flags)` once a scheduler wrapper
 *      installs `#define bpf_iter_css_new(...) sim_bpf_iter_css_new(...)`
 *      (see `schedulers/lavd/wrapper.c` for the LAVD wiring; mitosis
 *      keeps its bespoke single-element iterator for backward
 *      compatibility).
 *
 * The legacy single-buffer `sim_css_next(root, prev)` API is kept
 * working for any existing call sites; it iterates the pre-order list.
 */
#include "sim_wrapper.h"

/* Forward declaration for libc functions */
extern void *memset(void *s, int c, unsigned long n);

/*
 * CSS iterator capacity.
 *
 * Bumped from 256 to 2 048 as part of Phase 1 (see file header). Sized
 * to the production cgroup_bw library's `CBW_NR_CGRP_MAX = 2048`
 * ceiling so any cgroup that the library can register has a slot in
 * the iterator.
 *
 * Memory cost: 2 * 2 048 * sizeof(struct cgroup *) = 32 KiB BSS for
 * the two per-mode buffers. Negligible vs the 32 MiB arena.
 */
#define MAX_CSS_ITER_CGROUPS 2048

/*
 * Iteration mode flags. Match the kernel BPF UAPI values exactly so
 * scheduler / library code that passes
 * `BPF_CGROUP_ITER_DESCENDANTS_PRE` etc. flows through unmodified.
 *
 * From `include/uapi/linux/bpf.h`:
 *   BPF_CGROUP_ITER_ORDER_UNSPEC      = 0,
 *   BPF_CGROUP_ITER_SELF_ONLY         = 1,
 *   BPF_CGROUP_ITER_DESCENDANTS_PRE   = 2,
 *   BPF_CGROUP_ITER_DESCENDANTS_POST  = 3,
 *   BPF_CGROUP_ITER_ANCESTORS_UP      = 4,
 *
 * We honor PRE and POST today; UNSPEC and ANCESTORS_UP fall through
 * to PRE (matches kernel default behavior). SELF_ONLY is a one-element
 * walk handled by sim_bpf_iter_css.
 */
#define SIM_CSS_ITER_ORDER_UNSPEC     0
#define SIM_CSS_ITER_SELF_ONLY        1
#define SIM_CSS_ITER_DESCENDANTS_PRE  2
#define SIM_CSS_ITER_DESCENDANTS_POST 3
#define SIM_CSS_ITER_ANCESTORS_UP     4

/*
 * Two pre-populated buffers: one in pre-order, one in post-order.
 * Rust owns the populator (`prepare_css_iter*`) and writes BOTH lists
 * before invoking the BPF callback.
 */
static struct cgroup *sim_css_list_pre[MAX_CSS_ITER_CGROUPS];
static struct cgroup *sim_css_list_post[MAX_CSS_ITER_CGROUPS];
static int sim_css_count_pre;
static int sim_css_count_post;

/*
 * Legacy iteration cursor for `sim_css_next(root, prev)`. The flags-
 * aware iterator (`sim_bpf_iter_css_*`) keeps its own cursor in the
 * passed-in `struct bpf_iter_css *`, so multiple iterators can be
 * open simultaneously without aliasing this state.
 */
static int sim_css_index;
static struct cgroup *sim_css_root;

/*
 * Reset the CSS iterator state (called from Rust before populating).
 *
 * Clears BOTH pre-order and post-order buffers and resets the legacy
 * cursor. Safe to call between consecutive `prepare_css_iter*` cycles
 * even if no iteration happened in between.
 */
void sim_css_iter_reset(void)
{
	memset(sim_css_list_pre, 0, sizeof(sim_css_list_pre));
	memset(sim_css_list_post, 0, sizeof(sim_css_list_post));
	sim_css_count_pre = 0;
	sim_css_count_post = 0;
	sim_css_index = 0;
	sim_css_root = (void *)0;
}

/*
 * Append `cgrp` to the pre-order iteration list.
 *
 * Called from Rust in pre-order. Silently drops cgroups beyond
 * `MAX_CSS_ITER_CGROUPS` -- callers that care about the overflow
 * should bump that constant.
 */
void sim_css_iter_add(void *cgrp)
{
	if (sim_css_count_pre < MAX_CSS_ITER_CGROUPS && cgrp)
		sim_css_list_pre[sim_css_count_pre++] = (struct cgroup *)cgrp;
}

/*
 * Append `cgrp` to the post-order iteration list.
 *
 * Called from Rust in post-order. Same overflow semantics as
 * `sim_css_iter_add`.
 */
void sim_css_iter_add_post(void *cgrp)
{
	if (sim_css_count_post < MAX_CSS_ITER_CGROUPS && cgrp)
		sim_css_list_post[sim_css_count_post++] = (struct cgroup *)cgrp;
}

/*
 * Set the root cgroup for the current iteration. Stored only for the
 * legacy `sim_css_next(root, prev)` API (which ignores its `root`
 * argument).
 */
void sim_css_iter_set_root(void *root)
{
	sim_css_root = (struct cgroup *)root;
}

/*
 * Legacy pre-order iterator.
 *
 * Existing callers (mitosis init / the scxsim test harness) walk the
 * pre-order list one element at a time via this function. Phase 1
 * keeps it working unchanged on top of the new `sim_css_list_pre[]`
 * buffer.
 *
 * Arguments:
 *   - root: the root cgroup's CSS (ignored, we use sim_css_root).
 *   - prev: the previously returned CSS, or NULL to start iteration.
 *
 * Returns the next cgroup's &self (CSS), or NULL when done.
 */
struct cgroup_subsys_state *sim_css_next(
	struct cgroup_subsys_state *root,
	struct cgroup_subsys_state *prev)
{
	struct cgroup *cgrp;

	(void)root; /* We use sim_css_root set by Rust */

	if (prev == (void *)0) {
		/* Start of iteration */
		sim_css_index = 0;
	} else {
		/* Continue iteration */
		sim_css_index++;
	}

	if (sim_css_index >= sim_css_count_pre)
		return (void *)0;

	cgrp = sim_css_list_pre[sim_css_index];
	if (!cgrp)
		return (void *)0;

	/* Return the cgroup's self CSS */
	return &cgrp->self;
}

/*
 * Flags-aware CSS iterator backing `bpf_for_each(css, pos, root, flags)`.
 *
 * The BPF kernel API uses an opaque `struct bpf_iter_css *` whose
 * concrete kernel-side layout is `struct bpf_iter_css_kern { void
 * *start; void *pos; unsigned int flags; }` (see
 * `kernel/bpf/cgroup_iter.c`). The mitosis wrapper exposes that struct
 * via `scx_test_map.h`. We match the same layout below so that
 * scheduler / library code does not need to know which iterator
 * implementation is wired up.
 *
 * Cursor encoding: `pos` is reused as a small integer (cast through
 * uintptr_t) holding the next-index into the chosen buffer. `pos = 0`
 * means "iteration not started"; the iterator treats the start CSS
 * itself as the first emitted element under SELF_ONLY semantics, and
 * for DESCENDANTS_{PRE,POST} the buffer already includes the start
 * cgroup at the correct position.
 */
struct sim_bpf_iter_css_kern {
	void		 *start;
	void		 *pos;
	unsigned int	  flags;
};

int sim_bpf_iter_css_new(struct bpf_iter_css *it,
			 struct cgroup_subsys_state *start,
			 unsigned int flags)
{
	struct sim_bpf_iter_css_kern *iter =
		(struct sim_bpf_iter_css_kern *)it;
	if (!iter)
		return -1;
	iter->start = start;
	iter->pos = (void *)0; /* 0 == "not started yet" */
	iter->flags = flags;
	return 0;
}

struct cgroup_subsys_state *sim_bpf_iter_css_next(struct bpf_iter_css *it)
{
	struct sim_bpf_iter_css_kern *iter =
		(struct sim_bpf_iter_css_kern *)it;
	struct cgroup *cgrp;
	unsigned long idx;

	if (!iter)
		return (void *)0;

	/*
	 * SELF_ONLY: emit the start CSS exactly once.
	 */
	if (iter->flags == SIM_CSS_ITER_SELF_ONLY) {
		if (iter->pos != (void *)0)
			return (void *)0;
		iter->pos = (void *)1; /* mark "consumed" */
		return (struct cgroup_subsys_state *)iter->start;
	}

	/*
	 * For PRE and POST modes the Rust populator has already laid out
	 * the buffer in the correct traversal order. We just walk the
	 * matching one until exhausted. UNSPEC and ANCESTORS_UP fall
	 * through to PRE (matches the kernel's "default to PRE if mode
	 * was unset" behavior; ANCESTORS_UP is not yet modeled here --
	 * Phase 2 follow-up if cgroup_bw needs it).
	 */
	idx = (unsigned long)iter->pos;
	if (iter->flags == SIM_CSS_ITER_DESCENDANTS_POST) {
		if ((int)idx >= sim_css_count_post)
			return (void *)0;
		cgrp = sim_css_list_post[idx];
	} else {
		if ((int)idx >= sim_css_count_pre)
			return (void *)0;
		cgrp = sim_css_list_pre[idx];
	}
	iter->pos = (void *)(idx + 1);
	if (!cgrp)
		return (void *)0;
	return &cgrp->self;
}

void sim_bpf_iter_css_destroy(struct bpf_iter_css *it)
{
	(void)it; /* nothing to free -- iterator state is inline */
}

/*
 * Check if a cgroup is dying (percpu_count_ptr has the dying bit set).
 * In our simulator, cgroups are never dying, so this always returns false.
 */
bool sim_cgroup_is_dying(void *cgrp)
{
	(void)cgrp;
	return false;
}
