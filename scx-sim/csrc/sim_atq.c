/*
 * sim_atq.c -- userspace impl of the scx_atq_* API used by Phase 2's
 * compiled-in `scx/lib/cgroup_bw.bpf.c`.
 *
 * Phase 1 BPF infra scale-up item 7 (tg
 * `scxsim-bpf-infra-scale-up-phase1`, design doc
 * `experiments/lavd_cpubw_stalls_202604/SCXSIM_REAL_CGROUP_BW_LIBRARY_DESIGN.md`
 * section Phase 1 item 7).
 *
 * Storage: per-atq dynamic array of `(vtime, taskc *)` pairs kept
 * sorted ascending by vtime. Backed by glibc malloc / realloc / free
 * (this file is compiled into the main scxsim binary, NOT into the
 * `-nostdlib` scheduler .so files, so glibc is fully available);
 * the bump arena (`sim_arena.h`) is reserved for .so allocations.
 *
 * The simulator is single-threaded -- the `scx_atq_lock` /
 * `scx_atq_unlock` pair from the production header expand to no-ops
 * (or to RBC_GUARD pairs if needed); the `scx_atq_t::lock` field is
 * present for ABI compatibility but never inspected.
 *
 * Full public API implemented (matches `scx/scheds/include/lib/atq.h`
 * one-for-one; nothing stubbed out). Phase 2's compiled-in
 * `cgroup_bw.bpf.c` exercises a subset (`_vtime`, `_pop`, `_peek`,
 * `_nr_queued`, `_cancel`, `_create_internal`, `_destroy`); the FIFO
 * `_insert` / `_insert_unlocked` and arbitrary `_remove` /
 * `_remove_unlocked` are also implemented for future Phase-3
 * consumers (no-stub policy in `scx-sim/CLAUDE.md`).
 *
 *   scx_atq_init             (no-op)
 *   scx_atq_create_internal  (malloc + zero)
 *   scx_atq_destroy          (drain + free)
 *   scx_atq_insert,
 *   scx_atq_insert_unlocked
 *   scx_atq_insert_vtime,
 *   scx_atq_insert_vtime_unlocked
 *   scx_atq_remove,
 *   scx_atq_remove_unlocked
 *   scx_atq_pop              (smallest-vtime; taskc->atq cleared)
 *   scx_atq_peek             (smallest-vtime; non-destructive)
 *   scx_atq_nr_queued
 *   scx_atq_cancel           (find taskc->atq; remove from it)
 *
 * Symbol resolution: this file compiles into the main scxsim binary
 * via `crates/scx_simulator/build.rs` and is exported via `-rdynamic`,
 * so the scheduler `.so` files (lavd, mitosis, cosmos) can resolve
 * `scx_atq_*` references at dlopen time. Phase 2's compiled-in
 * `cgroup_bw.bpf.c` will use the same path.
 *
 * scx_task_common back-pointer: the production
 * `struct scx_task_common { struct rbnode node; scx_atq_t *atq; ... }`
 * carries an `atq` field at a known offset. We can't include
 * `lib/atq.h` (BPF-only) here, so we use a runtime-computed offset
 * registered via `sim_atq_set_taskc_atq_offset()`. Phase 2's wrapper.c
 * will register the offset before any `scx_atq_insert*` fires
 * (compute via `offsetof(struct scx_task_common, atq)` against the
 * production header). Default offset is the kernel's value at the
 * pinned scx submodule SHA -- see `scx/scheds/include/lib/atq.h` plus
 * `lib/rbtree.h::struct rbnode`. Mismatches manifest as a NULL-deref
 * or a silent corruption; the runtime registration is the safety belt.
 */

#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>

/*
 * Internal atq state. The scx_atq_t opaque pointer returned to BPF
 * code is just `(uintptr_t)(struct sim_atq *)`. We never dereference
 * the production scx_atq_t fields (lock / tid / tree / etc.) -- the
 * single-threaded simulator has no need for them.
 */
struct sim_atq_entry {
	unsigned long long	vtime;
	void			*taskc;
};

struct sim_atq {
	int		fifo;	/* 1 == FIFO mode (vtime arg must be SCX_ATQ_FIFO) */
	unsigned long long	capacity;	/* SCX_ATQ_INF_CAPACITY = (u64)-1 */
	unsigned long long	size;
	unsigned long long	seq;	/* monotonic key for FIFO entries */
	unsigned long long	cap;	/* allocated entries[] capacity */
	struct sim_atq_entry	*entries;
};

/*
 * SCX_ATQ_FIFO sentinel from `scheds/include/lib/atq.h::enum
 * scx_atq_consts`. Bit-identical so passing-through scx_atq_insert_vtime
 * with `vtime == SCX_ATQ_FIFO` flips to FIFO insertion semantics.
 */
#define SCX_ATQ_FIFO_SENTINEL ((unsigned long long)-1)

/*
 * Default offset of `atq` field in `struct scx_task_common`. Computed
 * by hand from `scx/scheds/include/lib/atq.h` and
 * `scx/scheds/include/lib/rbtree.h` at the pinned scx submodule SHA:
 *
 *   struct rbnode {
 *     union sdt_id tid;       // 8
 *     rbnode_t *parent;       // 8
 *     union { rbnode_t *left, *right; rbnode_t *child[2]; }; // 16
 *     uint64_t key;           // 8
 *     union { rbnode_t *next; uint64_t value; }; // 8
 *     bool is_red;            // 1 + 7 padding
 *   };  // 56 bytes
 *
 *   struct scx_task_common {
 *     struct rbnode node;     // 56
 *     int holdcnt;            // OFFSET 56 (upstream cd9c4600)
 *     scx_atq_t *atq;         // OFFSET 64 (was 56 before holdcnt)
 *     enum scx_task_throttle state;
 *   };
 *
 * wrapper.c calls `sim_atq_set_taskc_atq_offset(offsetof(struct
 * scx_task_common, atq))` and `sim_atq_set_taskc_holdcnt_offset(
 * offsetof(struct scx_task_common, holdcnt))` from lavd_register_cbw_maps
 * (post-include, so the production struct is in scope) to keep these
 * offsets honest across struct-layout changes. The defaults below match
 * the pinned scx submodule SHA as a safety net.
 */
static unsigned long sim_taskc_atq_offset = 64;
static unsigned long sim_taskc_holdcnt_offset = 56;

void sim_atq_set_taskc_atq_offset(unsigned long off)
{
	sim_taskc_atq_offset = off;
}

void sim_atq_set_taskc_holdcnt_offset(unsigned long off)
{
	sim_taskc_holdcnt_offset = off;
}

/* Read/write the back-pointer at the registered offset. */
static inline void sim_atq_taskc_set_atq(void *taskc, void *atq)
{
	if (!taskc)
		return;
	*(void **)((char *)taskc + sim_taskc_atq_offset) = atq;
}

static inline void *sim_atq_taskc_get_atq(void *taskc)
{
	if (!taskc)
		return NULL;
	return *(void **)((char *)taskc + sim_taskc_atq_offset);
}

/*
 * Mirror production scx_atq_task_hold(): bump the popped task's holdcnt.
 * Production's scx_atq_pop(atq, hold=true) increments holdcnt so the task
 * stays pinned off-queue until the paired scx_atq_task_drop() runs. The
 * drop is a static-inline in atq.h that executes natively in the compiled
 * cgroup_bw code, so the sim MUST perform the matching increment here or
 * the drops drive holdcnt negative and scx_atq_task_detach()'s
 * `while (holdcnt > 0)` wait observes an inconsistent count.
 */
static inline void sim_atq_taskc_hold(void *taskc)
{
	if (!taskc)
		return;
	*(int *)((char *)taskc + sim_taskc_holdcnt_offset) += 1;
}

/* Mirror production's scx_atq_task_drop(): decrement the task's holdcnt. */
static inline void sim_atq_taskc_drop(void *taskc)
{
	if (!taskc)
		return;
	*(int *)((char *)taskc + sim_taskc_holdcnt_offset) -= 1;
}

/*
 * SCX_ATQ_DEAD sentinel (atq.h enum scx_atq_consts::SCX_ATQ_DEAD = 0x1).
 * scx_atq_task_detach() latches this into taskc->atq so the task can never
 * be re-queued. sim_atq.c cannot include atq.h (BPF-only), so mirror the
 * value locally.
 */
#define SIM_ATQ_DEAD ((void *)(unsigned long)0x1)

/*
 * Grow the entries[] array if needed. Doubles capacity from a base of
 * 4 entries -- common case is small atqs (per-LLC backlog), so the
 * doubling growth is bounded in practice.
 */
static int sim_atq_grow_if_needed(struct sim_atq *a)
{
	unsigned long long new_cap;
	struct sim_atq_entry *new_entries;

	if (a->size < a->cap)
		return 0;
	new_cap = a->cap ? a->cap * 2 : 4;
	new_entries = realloc(a->entries, (size_t)new_cap * sizeof(*new_entries));
	if (!new_entries)
		return -ENOMEM;
	a->cap = new_cap;
	a->entries = new_entries;
	return 0;
}

/* ------------------------------------------------------------------- */
/* Public API (matches scx/scheds/include/lib/atq.h signatures)        */
/* ------------------------------------------------------------------- */

int scx_atq_init(void)
{
	/*
	 * Production initializes `scx_atq_allocator` here. Our impl uses
	 * glibc malloc instead of the BPF arena allocator; nothing to do.
	 */
	return 0;
}

unsigned long long scx_atq_create_internal(int fifo, unsigned long long capacity)
{
	struct sim_atq *a = calloc(1, sizeof(*a));
	if (!a)
		return 0;
	a->fifo = fifo ? 1 : 0;
	a->capacity = capacity; /* SCX_ATQ_INF_CAPACITY = (u64)-1 */
	a->size = 0;
	a->seq = 0;
	a->cap = 0;
	a->entries = NULL;
	return (unsigned long long)(uintptr_t)a;
}

int scx_atq_insert_vtime_unlocked(void *atq_raw, void *taskc, unsigned long long vtime)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	unsigned long long key;
	long lo, hi, mid;
	int rc;

	if (!a || !taskc)
		return -EINVAL;
	if (a->size == a->capacity)
		return -ENOSPC;
	if ((vtime == SCX_ATQ_FIFO_SENTINEL) != (a->fifo == 1))
		return -EINVAL;

	rc = sim_atq_grow_if_needed(a);
	if (rc)
		return rc;

	/*
	 * For FIFO mode we synthesize a monotonic key from `seq` so the
	 * sort order matches insertion order. Production "leaks the seq
	 * on error" comment doesn't matter here -- we increment only after
	 * the realloc succeeded.
	 */
	key = (vtime == SCX_ATQ_FIFO_SENTINEL) ? a->seq++ : vtime;

	/*
	 * Binary-search for the insertion position (first entry with
	 * vtime > key). The array is kept sorted ascending so subsequent
	 * pop/peek are O(1).
	 */
	lo = 0;
	hi = (long)a->size;
	while (lo < hi) {
		mid = lo + ((hi - lo) >> 1);
		if (a->entries[mid].vtime <= key)
			lo = mid + 1;
		else
			hi = mid;
	}
	/* Shift right to make room. */
	if ((unsigned long long)lo < a->size) {
		memmove(&a->entries[lo + 1], &a->entries[lo],
			(size_t)(a->size - (unsigned long long)lo)
				* sizeof(struct sim_atq_entry));
	}
	a->entries[lo].vtime = key;
	a->entries[lo].taskc = taskc;
	a->size += 1;

	sim_atq_taskc_set_atq(taskc, a);
	return 0;
}

int scx_atq_insert_vtime(void *atq, void *taskc, unsigned long long vtime)
{
	/* Single-threaded sim -- no lock needed. */
	return scx_atq_insert_vtime_unlocked(atq, taskc, vtime);
}

unsigned long long scx_atq_pop(void *atq_raw, int hold)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	void *taskc;

	if (!a || a->size == 0)
		return 0;
	taskc = a->entries[0].taskc;
	a->size -= 1;
	if (a->size > 0) {
		memmove(&a->entries[0], &a->entries[1],
			(size_t)a->size * sizeof(struct sim_atq_entry));
	}
	/* Match production: hold the popped task if requested (see
	 * sim_atq_taskc_hold). */
	if (hold)
		sim_atq_taskc_hold(taskc);
	sim_atq_taskc_set_atq(taskc, NULL);
	return (unsigned long long)(uintptr_t)taskc;
}

unsigned long long scx_atq_peek(void *atq_raw)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	if (!a || a->size == 0)
		return 0;
	return (unsigned long long)(uintptr_t)a->entries[0].taskc;
}

int scx_atq_nr_queued(void *atq_raw)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	if (!a)
		return 0;
	return (int)a->size;
}

int scx_atq_cancel(void *taskc)
{
	struct sim_atq *a;
	unsigned long long i;

	if (!taskc)
		return 0;
	a = (struct sim_atq *)sim_atq_taskc_get_atq(taskc);
	if (!a || a == (struct sim_atq *)SIM_ATQ_DEAD)
		return 0;
	for (i = 0; i < a->size; i++) {
		if (a->entries[i].taskc == taskc) {
			if (i + 1 < a->size) {
				memmove(&a->entries[i], &a->entries[i + 1],
					(size_t)(a->size - i - 1)
						* sizeof(struct sim_atq_entry));
			}
			a->size -= 1;
			sim_atq_taskc_set_atq(taskc, NULL);
			return 0;
		}
	}
	/* Race-loser path in production. Single-threaded sim never hits this. */
	return -ENOENT;
}

/*
 * ATQ task-lifecycle API (upstream commits 2f085946 "add DEAD state and
 * detach/fini operations" and cd9c4600 "add task hold and drop helpers").
 * In production these live in atq.h (hold/drop as static-inline) and
 * atq.bpf.c (detach/fini as __weak), all under #ifdef __BPF__ -- so they
 * are NOT compiled into scxsim's userspace build and must be provided here
 * as the kernel ATQ substrate. cgroup_bw.bpf.c (compiled into the scheduler
 * .so) resolves these via dlopen + -rdynamic. Semantics mirror production;
 * the single-threaded sim collapses the lock/hold-wait loops to no-ops.
 */
void scx_atq_task_hold(void *taskc)
{
	sim_atq_taskc_hold(taskc);
}

void scx_atq_task_drop(void *taskc)
{
	sim_atq_taskc_drop(taskc);
}

/*
 * Detach a dying task: unlink it from whatever atq it sits in, then latch
 * SCX_ATQ_DEAD so it can never be queued again. Production then spins until
 * holdcnt drops to 0; the single-threaded sim has no concurrent holders, so
 * holdcnt is already balanced by the paired hold/drop calls and no wait is
 * needed.
 */
int scx_atq_task_detach(void *taskc)
{
	void *atq;

	if (!taskc)
		return 0;
	atq = sim_atq_taskc_get_atq(taskc);
	if (atq && atq != SIM_ATQ_DEAD)
		scx_atq_cancel(taskc); /* removes entry, clears atq back-ptr */
	sim_atq_taskc_set_atq(taskc, SIM_ATQ_DEAD);
	return 0;
}

/*
 * Cancel a task's atq membership while keeping it reusable. Returns 1 if this
 * caller removed the task, 0 if it was not queued (or already dying).
 */
int scx_atq_task_fini(void *taskc)
{
	void *atq;

	if (!taskc)
		return 0;
	atq = sim_atq_taskc_get_atq(taskc);
	if (!atq || atq == SIM_ATQ_DEAD)
		return 0;
	scx_atq_cancel(taskc); /* removes entry, clears atq back-ptr */
	return 1;
}

int scx_atq_destroy(void *atq_raw)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	unsigned long long i;

	if (!a)
		return 0;
	/* Mirror production's drain-on-destroy: clear taskc->atq for every
	 * remaining entry so a stale `taskc->atq` doesn't outlive the atq. */
	for (i = 0; i < a->size; i++)
		sim_atq_taskc_set_atq(a->entries[i].taskc, NULL);
	free(a->entries);
	free(a);
	return 0;
}

/* ------------------------------------------------------------------- */
/* Remaining public entry points (FIFO insert + arbitrary remove).      */
/* No Phase 2 consumer today, but implementing them now keeps the API   */
/* surface complete and stub-free so future Phase 3 consumers don't     */
/* surprise us. Each is a thin wrapper over the sorted-array primitives */
/* above; production semantics are preserved.                           */
/* ------------------------------------------------------------------- */

int scx_atq_insert_unlocked(void *atq, void *taskc)
{
	/*
	 * FIFO insert: production passes SCX_ATQ_FIFO as the vtime
	 * sentinel; the insert path synthesizes a monotonic seq for it.
	 */
	return scx_atq_insert_vtime_unlocked(atq, taskc, SCX_ATQ_FIFO_SENTINEL);
}

int scx_atq_insert(void *atq, void *taskc)
{
	/* Single-threaded sim -- no lock needed. */
	return scx_atq_insert_unlocked(atq, taskc);
}

int scx_atq_remove_unlocked(void *atq_raw, void *taskc)
{
	struct sim_atq *a = (struct sim_atq *)atq_raw;
	unsigned long long i;

	if (!a || !taskc)
		return -EINVAL;
	for (i = 0; i < a->size; i++) {
		if (a->entries[i].taskc == taskc) {
			if (i + 1 < a->size) {
				memmove(&a->entries[i], &a->entries[i + 1],
					(size_t)(a->size - i - 1)
						* sizeof(struct sim_atq_entry));
			}
			a->size -= 1;
			sim_atq_taskc_set_atq(taskc, NULL);
			return 0;
		}
	}
	return -ENOENT;
}

int scx_atq_remove(void *atq, void *taskc)
{
	/* Single-threaded sim -- no lock needed. */
	return scx_atq_remove_unlocked(atq, taskc);
}
