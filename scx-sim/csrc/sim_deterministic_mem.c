/*
 * sim_deterministic_mem.c - Branchless memset/memcpy for PMU determinism.
 *
 * Compiled into each scheduler .so to override glibc's SIMD-optimized
 * versions. glibc's memset/memcpy have alignment-dependent conditional
 * branches that cause nondeterministic PMU RBC counts between runs.
 *
 * These are simple implementations with a fixed branch pattern: the
 * same code path executes regardless of pointer alignment, ensuring
 * perfectly deterministic retired-branch-conditional counts.
 *
 * Since the .so is linked with -nostdlib and these provide strong
 * memset/memcpy definitions, the linker uses these instead of
 * resolving from the main binary's glibc.
 */

typedef unsigned long size_t;

void *memset(void *s, int c, size_t n)
{
	unsigned char *p = (unsigned char *)s;
	unsigned char val = (unsigned char)c;
	size_t i;

	/* 8-byte fill for the bulk. */
	unsigned long pattern = val;
	pattern |= pattern << 8;
	pattern |= pattern << 16;
	pattern |= pattern << 32;

	unsigned long *wp = (unsigned long *)p;
	size_t nwords = n / sizeof(unsigned long);
	for (i = 0; i < nwords; i++)
		wp[i] = pattern;

	/* Remaining bytes. */
	size_t tail = nwords * sizeof(unsigned long);
	for (i = tail; i < n; i++)
		p[i] = val;

	return s;
}

void *memcpy(void *dst, const void *src, size_t n)
{
	unsigned char *d = (unsigned char *)dst;
	const unsigned char *s = (const unsigned char *)src;
	size_t i;

	/* 8-byte copy for the bulk. */
	unsigned long *dw = (unsigned long *)d;
	const unsigned long *sw = (const unsigned long *)s;
	size_t nwords = n / sizeof(unsigned long);
	for (i = 0; i < nwords; i++)
		dw[i] = sw[i];

	/* Remaining bytes. */
	size_t tail = nwords * sizeof(unsigned long);
	for (i = tail; i < n; i++)
		d[i] = s[i];

	return dst;
}
