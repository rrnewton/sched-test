/*
 * sim_deterministic_mem.c - Deterministic memory ops for PMU determinism.
 *
 * Compiled into each scheduler .so to override glibc's SIMD-optimized
 * versions. glibc's memset/memcpy have alignment-dependent conditional
 * branches that cause nondeterministic PMU RBC counts between runs.
 *
 * Also overrides calloc/free/malloc/realloc to use the sim_arena bump
 * allocator. glibc's malloc internals have complex branching that depends
 * on heap state (free lists, chunk coalescing, etc.), making PMU RBC
 * counts nondeterministic across runs. The bump allocator has a fixed,
 * minimal branch pattern. See sim-70abc8.
 *
 * These are simple implementations with a fixed branch pattern: the
 * same code path executes regardless of pointer alignment, ensuring
 * perfectly deterministic retired-branch-conditional counts.
 *
 * Since the .so is linked with -nostdlib and these provide strong
 * definitions, the linker uses these instead of resolving from the
 * main binary's glibc.
 */

typedef unsigned long size_t;
#define NULL ((void *)0)

/* sim_arena bump allocator state (defined in sim_arena.c, linked into .so) */
extern char sim_arena_buf[];
extern unsigned long sim_arena_offset;

/*
 * Arena size — MUST stay in sync with `SIM_ARENA_SIZE` in
 * `csrc/sim_arena.h`. See that header for the rationale of the 32 MiB
 * ceiling (Phase 1 BPF infra scale-up: per-task contexts, compiled-in
 * cgroup_bw cgroup contexts, atq instances, headroom for Phase 2/3).
 *
 * This file is compiled into each scheduler .so under `-nostdlib`, so
 * it cannot include `sim_arena.h` (which depends on the simulator's
 * full include path). Keep the literal in lockstep manually.
 */
#define SIM_ARENA_SIZE (32UL * 1024 * 1024)

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

/*
 * Deterministic malloc using the bump allocator.
 * Aligns all allocations to 16 bytes for consistency.
 * Returns NULL if the arena is exhausted (should not happen
 * in practice — arena is 64 MiB).
 */
void *malloc(size_t size)
{
	/* Align to 16 bytes. */
	size_t aligned = (size + 15) & ~(size_t)15;
	unsigned long off = sim_arena_offset;
	unsigned long new_off = off + aligned;
	if (new_off > SIM_ARENA_SIZE)
		return NULL;
	sim_arena_offset = new_off;
	return &sim_arena_buf[off];
}

/*
 * Deterministic calloc: bump-allocate + zero-fill.
 * The arena is BSS-zeroed at program start, but we memset anyway
 * for correctness when the arena is reused across simulation runs.
 */
void *calloc(size_t nmemb, size_t size)
{
	size_t total = nmemb * size;
	void *p = malloc(total);
	if (p)
		memset(p, 0, total);
	return p;
}

/*
 * Deterministic realloc: bump-allocate a new block + copy.
 * The old block is not freed (bump allocator).
 */
void *realloc(void *ptr, size_t size)
{
	void *new_ptr = malloc(size);
	if (new_ptr && ptr)
		memcpy(new_ptr, ptr, size);
	return new_ptr;
}

/*
 * No-op free: the bump allocator does not reclaim memory.
 * The arena is reset between simulation runs via sim_arena_offset = 0.
 */
void free(void *ptr)
{
	(void)ptr;
}

/*
 * Deterministic memmove for when src and dst overlap.
 * glibc's memmove has SIMD-optimized paths with alignment-dependent
 * branches. This simple byte-copy is always deterministic.
 */
void *memmove(void *dst, const void *src, size_t n)
{
	unsigned char *d = (unsigned char *)dst;
	const unsigned char *s = (const unsigned char *)src;
	size_t i;

	if (d < s) {
		for (i = 0; i < n; i++)
			d[i] = s[i];
	} else {
		for (i = n; i > 0; i--)
			d[i - 1] = s[i - 1];
	}
	return dst;
}

/*
 * Deterministic memcmp — simple byte comparison.
 */
int memcmp(const void *a, const void *b, size_t n)
{
	const unsigned char *pa = (const unsigned char *)a;
	const unsigned char *pb = (const unsigned char *)b;
	size_t i;

	for (i = 0; i < n; i++) {
		if (pa[i] != pb[i])
			return (int)pa[i] - (int)pb[i];
	}
	return 0;
}

/*
 * Deterministic strncmp — simple byte comparison.
 * glibc's strncmp has SIMD-optimized paths with alignment-dependent
 * branches. This version has a fixed branch pattern for the same inputs.
 */
int strncmp(const char *s1, const char *s2, size_t n)
{
	size_t i;

	for (i = 0; i < n; i++) {
		unsigned char c1 = (unsigned char)s1[i];
		unsigned char c2 = (unsigned char)s2[i];
		if (c1 != c2)
			return (int)c1 - (int)c2;
		if (c1 == 0)
			return 0;
	}
	return 0;
}

/*
 * Deterministic strcmp — simple byte comparison.
 */
int strcmp(const char *s1, const char *s2)
{
	while (*s1 && *s1 == *s2) {
		s1++;
		s2++;
	}
	return (int)(unsigned char)*s1 - (int)(unsigned char)*s2;
}

/*
 * Deterministic strlen — simple byte scan.
 */
size_t strlen(const char *s)
{
	size_t len = 0;
	while (s[len])
		len++;
	return len;
}
