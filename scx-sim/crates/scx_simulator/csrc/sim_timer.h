/*
 * sim_timer.h - Generic BPF timer slot-table for the userspace simulator.
 *
 * Opt-in shared substrate: a wrapper that uses bpf_timer #includes this AFTER
 * sim_wrapper.h (which pulls <scx/common.bpf.h> for `struct bpf_timer` and the
 * bpf_timer_* helper macros this file overrides). Schedulers WITHOUT timers do
 * NOT include it, so their bpf_timer_* stay unoverridden and never arm the
 * engine -- including it is what makes a scheduler's timer live.
 *
 * bpf_timer_init/set_callback/start are BPF helpers (bpf_helper_defs.h
 * fn-ptr-via-helper-id), NOT extern __ksym kfuncs, so function-like macro
 * overrides are safe here (the #undef first clears the helper-id pointer
 * constants). Each (struct bpf_timer *) is assigned a slot by first-fit init
 * order; arming routes to the engine via sim_timer_start_slot(slot, nsecs). The
 * engine fires a slot by calling the scheduler's <name>_fire_timer(slot) entry
 * (the symbol the Rust FireTimerFn resolves), which forwards to
 * scxsim_fire_timer(). Replaces the former per-scheduler copies (lavd's
 * lavd_timer_table, mitosis's single-timer statics).
 */
#pragma once

/*
 * Must match the Rust-side MAX_BPF_TIMERS
 * (crates/scx_simulator/src/unsafe_impl/kfuncs.rs). The _Static_assert is a
 * one-directional C-side guard: a C TU cannot read the Rust const, so this pins
 * the C side to the agreed literal and catches C-side drift; the Rust side
 * carries the matching literal.
 */
#ifndef MAX_BPF_TIMERS
#define MAX_BPF_TIMERS 8
#endif
_Static_assert(MAX_BPF_TIMERS == 8,
	       "C MAX_BPF_TIMERS must match Rust unsafe_impl/kfuncs.rs MAX_BPF_TIMERS");

extern void sim_timer_start_slot(unsigned int slot, unsigned long long nsecs);

struct scxsim_timer_slot {
	struct bpf_timer *timer_ptr; /* NULL = unused slot */
	int (*timer_cb)(void *, int *, struct bpf_timer *);
	void *timer_map;
};

static struct scxsim_timer_slot scxsim_timer_table[MAX_BPF_TIMERS];

/*
 * Find or assign a slot for the given (struct bpf_timer *). Returns the slot
 * index in [0, MAX_BPF_TIMERS), or -1 on exhaustion. First-fit by init order:
 * slot 0 is whichever timer bpf_timer_init runs first.
 */
static inline int scxsim_timer_slot_for(struct bpf_timer *timer)
{
	int i;
	for (i = 0; i < MAX_BPF_TIMERS; i++)
		if (scxsim_timer_table[i].timer_ptr == timer)
			return i;
	for (i = 0; i < MAX_BPF_TIMERS; i++)
		if (!scxsim_timer_table[i].timer_ptr) {
			scxsim_timer_table[i].timer_ptr = timer;
			return i;
		}
	return -1;
}

/* Reset all slots; called at scheduler setup for cross-run determinism. */
static inline void scxsim_timer_reset(void)
{
	__builtin_memset(scxsim_timer_table, 0, sizeof(scxsim_timer_table));
}

#undef bpf_timer_init
#define bpf_timer_init(timer, map, flags) \
	({ \
		int _s = scxsim_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) scxsim_timer_table[_s].timer_map = (void *)(map); \
		0; \
	})

#undef bpf_timer_set_callback
#define bpf_timer_set_callback(timer, cb) \
	({ \
		int _s = scxsim_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) \
			scxsim_timer_table[_s].timer_cb = \
				(typeof(scxsim_timer_table[0].timer_cb))(cb); \
		0; \
	})

#undef bpf_timer_start
#define bpf_timer_start(timer, nsecs, flags) \
	({ \
		int _s = scxsim_timer_slot_for((struct bpf_timer *)(timer)); \
		if (_s >= 0) sim_timer_start_slot((unsigned int)_s, (nsecs)); \
		0; \
	})

/*
 * Dispatch a fired timer slot to its registered callback. The per-scheduler
 * <name>_fire_timer (resolved by the Rust engine) forwards here.
 */
static inline void scxsim_fire_timer(unsigned int slot)
{
	int key = 0;
	if (slot >= MAX_BPF_TIMERS)
		return;
	if (scxsim_timer_table[slot].timer_cb && scxsim_timer_table[slot].timer_ptr)
		scxsim_timer_table[slot].timer_cb(
			scxsim_timer_table[slot].timer_map,
			&key,
			scxsim_timer_table[slot].timer_ptr);
}
