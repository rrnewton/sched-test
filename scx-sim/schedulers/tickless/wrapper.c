/*
 * tickless_wrapper.c - Wrapper to compile scx_tickless as userspace C
 *
 * This file includes the simulator wrapper infrastructure and then
 * the actual scheduler source. The header guards in common.bpf.h
 * prevent re-inclusion, so our overridden macros take effect.
 *
 * NOTE: This file is compiled with -Dconst= to strip const qualifiers.
 * BPF schedulers declare globals as "const volatile" (patched by the
 * BPF loader). Stripping const makes them writable from Rust.
 */
#include "sim_wrapper.h"
#include "sim_task.h"
#include "sim_kconfig_defaults.h"

/*
 * BPF timer substrate. scx_tickless is driven entirely by a periodic
 * bpf_timer: ops.init arms one per primary CPU via init_timer(), and
 * sched_timerfn() re-arms itself and does the dispatch work. Without these
 * overrides the calls resolve to libbpf's bpf_helper_defs.h declarations,
 * which are function pointers holding the raw helper id -- so bpf_timer_init
 * would jump to address 169 rather than fail to link (mb sim-rq117).
 *
 * sim_timer.h is the shared slot table the engine already drives for lavd and
 * mitosis; tickless only has to include it and expose a fire entry point.
 */
#include "sim_timer.h"

/*
 * CONFIG_HZ: __kconfig extern referenced by tickless. In the kernel,
 * this resolves to the HZ config value. Default 250 for simulation; an embedder
 * overrides via -DSIM_CONFIG_HZ (see sim_kconfig_defaults.h). Reachable only when
 * tick_freq is 0 (`tick_freq ? : CONFIG_HZ`); the tickless manifest sets
 * tick_freq=250, so CONFIG_HZ is the fallback.
 */
unsigned int CONFIG_HZ = SIM_CONFIG_HZ;

/* Include tickless interface header, then the scheduler source.
 * common.bpf.h is already included (header guard set), so our
 * BPF_STRUCT_OPS and SCX_OPS_DEFINE overrides are in effect. */
#include "intf.h"
#include "main.bpf.c"

/*
 * Register the tickless BPF maps with the test map infrastructure.
 *
 * The test infrastructure needs maps registered before they can be
 * used by bpf_map_lookup_elem / bpf_task_storage_get. This function
 * should be called before tickless_init().
 */
/*
 * Fire the stored BPF timer callback for `slot`.
 *
 * Resolved by the Rust engine as "<prefix>_fire_timer" and called when an
 * EventKind::TimerFired { slot } pops from the event queue.
 */
void tickless_fire_timer(unsigned int slot)
{
	scxsim_fire_timer(slot);
}

/*
 * Userspace-side post-attach step, called by the engine right after ops.init.
 *
 * Upstream scx_tickless splits timer bring-up in two: ops.init creates the
 * timers via init_timer(), and its Rust userspace then invokes the
 * `start_timer` SEC("syscall") program to arm them. scxsim's wrapper plays
 * that userspace role (it already does so for enable_primary_cpu in
 * tickless_setup), so call the scheduler's own start_timer here rather than
 * arming anything ourselves. Without it the timer is initialised and never
 * started, so sched_timerfn -- the callback the whole scheduler is built
 * around -- never fires (mb sim-rq117).
 *
 * start_timer() rejects a cpu that is not the current one, matching the
 * kernel's per-CPU timer semantics; CPU 0 is the primary set up by
 * tickless_setup and is the CPU ops.init runs on.
 */
void tickless_post_init(void)
{
	struct cpu_arg arg = { .cpu_id = 0 };

	start_timer(&arg);
}

void tickless_register_maps(void)
{
	scx_test_map_clear_all();

	SCX_REGISTER_STORAGE(task_ctx_stor);
	/*
	 * pre_seed = true. cpu_ctx_stor is a BPF_MAP_TYPE_ARRAY with
	 * max_entries = MAX_CPUS, and kernel array maps are PREALLOCATED: a
	 * lookup with an in-range index always returns a valid zeroed pointer,
	 * never NULL. Leaving it unseeded made try_lookup_cpu_ctx() return NULL,
	 * so init_timer() bailed with -ENOENT and ops.init failed outright. That
	 * stayed invisible only because is_primary_cpu() was false for the whole
	 * run (mb sim-hfvmf), so init_timer() was never reached.
	 */
	SCX_REGISTER_ARRAY(cpu_ctx_stor, true);
}

/*
 * Combined setup function called from Rust before tickless_init().
 * Registers maps and enables CPU 0; the config globals (nr_cpu_ids/smt_enabled/
 * slice_ns/tick_freq) are written before run by the generic manifest
 * apply_rodata path (scheduler_manifest.rs tickless.runtime.rodata), not here.
 * struct cpu_arg is defined in intf.h (included above).
 */
void tickless_setup(unsigned int num_cpus)
{
	unsigned int i;
	struct cpu_arg arg = { .cpu_id = 0 };

	for (i = 0; i < num_cpus && i < 1024; i++)
		preferred_cpus[i] = i;

	scxsim_timer_reset();
	tickless_register_maps();
	enable_primary_cpu(&arg);
}
