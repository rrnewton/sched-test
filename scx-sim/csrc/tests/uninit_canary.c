/*
 * uninit_canary.c — a deliberately-broken translation unit used to prove the
 * uninitialised-read DETECTOR is switched on in the real scheduler CFLAGS.
 *
 * This file is NEVER linked into scxsim. scripts/check_ub_fidelity.sh compiles
 * it with the same warning flags the scheduler build uses and asserts that
 * clang reports the read below. If someone drops
 * -Wconditional-uninitialized from schedulers/Makefile, this canary goes
 * silent and the check fails.
 *
 * The shape is copied from the real instance that motivated the policy:
 * scx_layered's match_substr() in scx/scheds/rust/scx_layered/src/bpf/
 * util.bpf.c, where the outer loop compares against `y` before the inner loop
 * that assigns it. Under coverage instrumentation the garbage value made
 * MATCH_CGROUP_CONTAINS silently return "no match".
 *
 * NOTE ON FLAG CHOICE: neither -Wall nor -Wuninitialized catches this shape;
 * only -Wconditional-uninitialized does. Verified against clang 22.
 */

int scxsim_uninit_canary(const unsigned char *buf, int len);

int scxsim_uninit_canary(const unsigned char *buf, int len)
{
	int x, y, hits = 0;

	for (x = 0; x < len; x++) {
		/* `y` is read here on the first iteration, before the inner
		 * loop below has ever assigned it. */
		if (len - x < y)
			break;

		for (y = 0; y < len; y++) {
			if (buf[y] == 0)
				return hits;
			hits++;
		}
	}
	return hits;
}
