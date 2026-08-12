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
void tickless_register_maps(void)
{
	scx_test_map_clear_all();

	SCX_REGISTER_STORAGE(task_ctx_stor);
	SCX_REGISTER_ARRAY(cpu_ctx_stor, false);
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

	tickless_register_maps();
	enable_primary_cpu(&arg);
}
