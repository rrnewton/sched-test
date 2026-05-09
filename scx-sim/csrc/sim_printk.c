/*
 * sim_printk.c - userspace sink for LAVD's bpf_printk() under scxsim.
 *
 * Lives in its own translation unit so it can include the standard
 * userspace headers (<stdio.h>, <stdarg.h>, <stdlib.h>) without
 * colliding with the kernel-types views (vmlinux.h, libbpf headers)
 * pulled in by the LAVD wrapper TU.
 *
 * Stream B (printk pipeline) overnight, agent/lavd-printk-pipeline.
 *
 * Visibility model:
 *   LAVD_PRINTK unset / 0  -> suppressed.
 *   LAVD_PRINTK=1          -> debugln() lines emitted (level=1).
 *   LAVD_PRINTK=2          -> debugln() + traceln() lines emitted.
 *
 * Output format: every line is prefixed with `[LAVD-PRINTK]` to make
 * the stream trivially grep-able from normal scxsim stderr noise. The
 * `[BUG1-DBG]` substring further narrows the cgroup-bandwidth call
 * sites added by Stream B (see scx_lavd/src/bpf/main.bpf.c).
 *
 * No allocations on the hot path: vfprintf into stderr only. The
 * fflush is intentional; under panic/SIGABRT we want the trailing
 * lines to survive in the captured stderr.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

int sim_lavd_printk_level(void)
{
	static int cached = -1;
	if (cached < 0) {
		const char *e = getenv("LAVD_PRINTK");
		int level = (e && *e) ? atoi(e) : 0;
		cached = level < 0 ? 0 : level;
	}
	return cached;
}

__attribute__((format(printf, 1, 2)))
void sim_lavd_printk(const char *fmt, ...)
{
	va_list ap;
	if (sim_lavd_printk_level() <= 0)
		return;
	fputs("[LAVD-PRINTK] ", stderr);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	fflush(stderr);
}
