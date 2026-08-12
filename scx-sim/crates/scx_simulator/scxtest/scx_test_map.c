#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include <linux/bpf.h>

#include "scx_test_map.h"
#include "sim_rbc_guard.h"

/*
 * Sentinel cpu for plain bpf_map_lookup_elem on a PERCPU map: "the current
 * callback CPU" (kernel semantics). Resolved lazily in scx_test_map_lookup's
 * PERCPU branch via sim_current_cpu_or_none() — NORMAL maps ignore cpu, so the
 * accessor is never called for them, keeping setup/test-scaffolding contexts
 * (no callback context) safe.
 */
#define SCX_CPU_CURRENT (-1)

/*
 * Light, panic-free current-CPU accessor (Rust, resolved from the loading
 * binary). Returns UINT_MAX when there is no callback context; the (int) cast
 * yields -1, which the percpu bounds-check below maps to NULL.
 */
extern unsigned int sim_current_cpu_or_none(void);

enum {
	SCX_MAP_TYPE_NORMAL,
	SCX_MAP_TYPE_PERCPU,
};

struct scx_map_type {
	void *map_ptr;
	int map_type;
};

struct scx_map_entry {
	void *map_ptr;
	struct scx_test_map *map;
};

struct scx_percpu_map_entry {
	void *map_ptr;
	struct scx_percpu_test_map *map;
};

static struct scx_map_entry *scx_map_entries = NULL;
static int scx_map_entries_count = 0;

static struct scx_percpu_map_entry *scx_percpu_map_entries = NULL;
static int scx_percpu_map_entries_count = 0;

static struct scx_map_type *scx_map_types = NULL;
static int scx_map_types_count = 0;

/*
 * Reset all map registries.
 *
 * This must be called before re-registering maps (e.g. when a scheduler
 * .so is reloaded) to prevent stale entries from pointing to unmapped
 * memory after dlclose/dlopen cycles.
 *
 * Does NOT free map keys/values — those belong to the scx_test_map
 * structs inside the .so and are managed by the caller.
 */
void scx_test_map_clear_all(void)
{
	free(scx_map_entries);
	scx_map_entries = NULL;
	scx_map_entries_count = 0;

	free(scx_percpu_map_entries);
	scx_percpu_map_entries = NULL;
	scx_percpu_map_entries_count = 0;

	free(scx_map_types);
	scx_map_types = NULL;
	scx_map_types_count = 0;
}

static void scx_regsiter_map_type(void *map_ptr, int map_type)
{
	int index = scx_map_types_count;

	scx_map_types_count++;
	scx_map_types = reallocarray(scx_map_types, scx_map_types_count,
				    sizeof(struct scx_map_type));
	if (!scx_map_types) {
		perror("Failed to allocate memory for scx_map_types");
		exit(EXIT_FAILURE);
	}

	scx_map_types[index].map_ptr = map_ptr;
	scx_map_types[index].map_type = map_type;
}

static struct scx_test_map *scx_percpu_entry(const void *map_ptr, int cpu)
{
	for (int i = 0; i < scx_percpu_map_entries_count; i++) {
		if (scx_percpu_map_entries[i].map_ptr == map_ptr) {
			/* cpu may be -1 (no current callback CPU — e.g. a percpu
			 * plain lookup outside any callback): fail safe to NULL,
			 * the kernel-faithful "no current cpu" answer, never an
			 * out-of-bounds index. */
			if (cpu < 0 || cpu >= scx_percpu_map_entries[i].map->nr_cpus)
				return NULL;
			return &scx_percpu_map_entries[i].map->per_cpu_maps[cpu];
		}
	}
	return NULL;
}

static struct scx_test_map *scx_normal_entry(const void *map_ptr)
{
	for (int i = 0; i < scx_map_entries_count; i++) {
		if (scx_map_entries[i].map_ptr == map_ptr) {
			return scx_map_entries[i].map;
		}
	}
	return NULL;
}

static struct scx_test_map *scx_test_map_lookup(const void *map_ptr, int cpu)
{
	for (int i = 0; i < scx_map_types_count; i++) {
		if (scx_map_types[i].map_ptr == map_ptr) {
			if (scx_map_types[i].map_type == SCX_MAP_TYPE_PERCPU) {
				/* Plain lookup (SCX_CPU_CURRENT) resolves to the
				 * current callback CPU; explicit-cpu callers pass a
				 * real cpu. Resolve only in this PERCPU branch so
				 * NORMAL-map lookups never call the accessor. */
				if (cpu == SCX_CPU_CURRENT)
					cpu = (int)sim_current_cpu_or_none();
				return scx_percpu_entry(map_ptr, cpu);
			} else if (scx_map_types[i].map_type == SCX_MAP_TYPE_NORMAL) {
				return scx_normal_entry(map_ptr);
			}
		}
	}
	return NULL;
}

void *scx_test_map_lookup_percpu_elem(void *map, const void *key, int cpu)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, cpu);
	if (!test_map)
		RBC_GUARD_RETURN(NULL);

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key, test_map->key_size) == 0)
			RBC_GUARD_RETURN(SCX_MAP_VALUE(test_map, i));
	}

	RBC_GUARD_RETURN(NULL);
}

void *scx_test_map_lookup_elem(void *map, const void *key)
{
	RBC_GUARD_START;
	/* Plain lookup: SCX_CPU_CURRENT resolves to the current callback CPU for
	 * PERCPU maps (kernel semantics); NORMAL maps ignore the cpu arg. */
	struct scx_test_map *test_map = scx_test_map_lookup(map, SCX_CPU_CURRENT);
	if (!test_map)
		RBC_GUARD_RETURN(NULL);

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key, test_map->key_size) == 0)
			RBC_GUARD_RETURN(SCX_MAP_VALUE(test_map, i));
	}

	RBC_GUARD_RETURN(NULL);
}

static int map_update_elem(struct scx_test_map *test_map, const void *key,
			    const void *value, unsigned long flags)
{
	int index;

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key, test_map->key_size) == 0) {
			if (flags & BPF_NOEXIST) {
				return -1;
			}
			memcpy(SCX_MAP_VALUE(test_map, i), value, test_map->value_size);
			return 0;
		}
	}

	if (flags & BPF_EXIST) {
		return -1;
	}

	if (test_map->nr < 0) {
		return -1;
	}
	if ((unsigned int)test_map->nr >= test_map->max_entries) {
		return -1;
	}

	index = test_map->nr;
	test_map->nr++;

	test_map->keys = reallocarray(test_map->keys, test_map->nr, test_map->key_size);
	if (!test_map->keys) {
		perror("Failed to allocate memory for keys");
		exit(EXIT_FAILURE);
	}
	test_map->values = reallocarray(test_map->values, test_map->nr, test_map->value_size);
	if (!test_map->values) {
		perror("Failed to allocate memory for values");
		exit(EXIT_FAILURE);
	}
	memcpy(SCX_MAP_KEY(test_map, index), key, test_map->key_size);
	memcpy(SCX_MAP_VALUE(test_map, index), value, test_map->value_size);
	return 0;
}

/*
 * BPF_MAP_TYPE_{TASK,CGRP}_STORAGE backend.
 *
 * Local storage is keyed by the OBJECT IDENTITY -- the struct task_struct* /
 * struct cgroup* pointer VALUE -- not by the object's contents and not by the
 * .bpf.c-declared key type (an int/u64 verifier artifact). `key` points to the
 * 8-byte object pointer value (the bpf_task_storage_get macro binds the task to
 * a temp and passes its address; scx_test_cgrp_storage_get passes &cgrp), so we
 * compare sizeof(void*) -- NEVER memcmp the dereferenced object bytes (every sim
 * task shares an all-zero leading thread_info, which would alias all objects to
 * one slot).
 *
 * Values are STABLE: the kernel returns a per-object pointer valid for the
 * object's lifetime, and schedulers may hold it across other objects' creates.
 * So values is an array of individually-allocated slots (void** of per-slot
 * pointers); growing the map reallocs only the pointer array, never moving an
 * already-returned slot. (The NORMAL/HASH/PERCPU path keeps its contiguous
 * reallocarray'd values -- those return value-not-handle and nothing holds a
 * NORMAL-map value across inserts.)
 */
void *scx_test_task_storage_get(void *map, const void *key, void *value,
				unsigned long flags)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, 0);
	if (!test_map)
		RBC_GUARD_RETURN(NULL);

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key, sizeof(void *)) == 0)
			RBC_GUARD_RETURN(((void **)test_map->values)[i]);
	}

	if (!(flags & BPF_LOCAL_STORAGE_GET_F_CREATE))
		RBC_GUARD_RETURN(NULL);
	/*
	 * NO max_entries CAP HERE, deliberately. `max_entries` for a
	 * TASK/CGRP_STORAGE map is set by INIT_SCX_TEST_MAP_FROM_TASK_STORAGE to
	 * a placeholder 100 -- local-storage maps are create-on-demand and have
	 * no meaningful declared capacity (the .bpf.h max_entries array is a
	 * verifier artifact for these types). Enforcing that placeholder makes
	 * every scenario with >100 concurrent tasks fail: the scheduler's
	 * init_task gets a NULL task_ctx and returns -ENOMEM, which the engine
	 * reports as `init_task failed for pid=101 rc=-12`. That regressed 8
	 * tests (memory_safety, dispatch_paths, kick_cpu_behavior,
	 * cpu_migration, panic_recovery) that pass without the cap. Silently
	 * returning NULL would also violate the No Silent Failures rule. The
	 * slot arrays below grow with reallocarray, so there is no fixed bound
	 * to enforce.
	 */

	int index = test_map->nr;
	test_map->nr++;

	test_map->keys = reallocarray(test_map->keys, test_map->nr,
				      test_map->key_size);
	test_map->values = reallocarray(test_map->values, test_map->nr,
					sizeof(void *));
	if (!test_map->keys || !test_map->values) {
		perror("Failed to allocate task/cgrp storage slot arrays");
		exit(EXIT_FAILURE);
	}

	void *slot = calloc(1, test_map->value_size);
	if (!slot) {
		perror("Failed to allocate task/cgrp storage value");
		exit(EXIT_FAILURE);
	}
	if (value)
		memcpy(slot, value, test_map->value_size);

	memcpy(SCX_MAP_KEY(test_map, index), key, test_map->key_size);
	((void **)test_map->values)[index] = slot;
	RBC_GUARD_RETURN(slot);
}

/*
 * Drop a task/cgrp local-storage slot by object identity (the 8-byte pointer
 * value at `key`). Frees the per-object value slot, then swap-removes the key +
 * slot pointer. Mirrors bpf_task_storage_delete / bpf_cgrp_storage_delete:
 * returns 0 on success, -1 (-ENOENT) if the object had no slot. Pairs with the
 * per-slot void** storage in scx_test_task_storage_get (frees the slot that
 * storage_get allocated).
 */
int scx_storage_delete(void *map, const void *key)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, 0);
	if (!test_map)
		RBC_GUARD_RETURN(-1);

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key, sizeof(void *)) == 0) {
			int last = test_map->nr - 1;
			free(((void **)test_map->values)[i]);
			if (i != last) {
				memcpy(SCX_MAP_KEY(test_map, i),
				       SCX_MAP_KEY(test_map, last),
				       test_map->key_size);
				((void **)test_map->values)[i] =
					((void **)test_map->values)[last];
			}
			test_map->nr = last;
			RBC_GUARD_RETURN(0);
		}
	}
	RBC_GUARD_RETURN(-1);
}

int scx_test_map_update_elem(void *map, const void *key, const void *value,
			     unsigned long flags)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, 0);
	if (!test_map)
		RBC_GUARD_RETURN(-1);
	RBC_GUARD_RETURN(map_update_elem(test_map, key, value, flags));
}

/*
 * Delete an element by key from a NORMAL/HASH scx_test_map (contiguous values).
 * Returns 0 on success, -1 (-ENOENT) if absent. Backs bpf_map_delete_elem for
 * schedulers that route it here (e.g. lavd's cbw_cgrp_map -- lavd/wrapper.c
 * #defines bpf_map_delete_elem to this). Linear scan + swap-remove; the map is
 * small (bounded by max_entries) and the simulator is single-threaded.
 * Distinct from scx_storage_delete, which frees the per-slot void** values of
 * TASK/CGRP local storage.
 */
int scx_test_map_delete_elem(void *map, const void *key)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, 0);
	if (!test_map)
		RBC_GUARD_RETURN(-1);

	for (int i = 0; i < test_map->nr; i++) {
		if (memcmp(SCX_MAP_KEY(test_map, i), key,
			   test_map->key_size) == 0) {
			int last = test_map->nr - 1;
			if (i != last) {
				memcpy(SCX_MAP_KEY(test_map, i),
				       SCX_MAP_KEY(test_map, last),
				       test_map->key_size);
				memcpy(SCX_MAP_VALUE(test_map, i),
				       SCX_MAP_VALUE(test_map, last),
				       test_map->value_size);
			}
			test_map->nr = last;
			RBC_GUARD_RETURN(0);
		}
	}
	RBC_GUARD_RETURN(-1);
}

/*
 * Per-cgroup local storage shim.
 *
 * Mirrors the kernel `bpf_cgrp_storage_get(map, cgrp, value, flags)`
 * contract. The map is pointer-identity-keyed by the `struct cgroup *`
 * pointer VALUE (the same way `scx_test_task_storage_get` keys on the
 * `struct task_struct *` pointer). The value_size is read
 * from the registered scx_test_map -- the BPF map's
 * `BPF_MAP_TYPE_CGRP_STORAGE` declaration provides it via
 * `INIT_SCX_TEST_MAP_FROM_TASK_STORAGE` (or its CGRP_STORAGE-named
 * sibling).
 *
 * Implementation note: the semantics of TASK_STORAGE and CGRP_STORAGE
 * are byte-identical at our level of abstraction (key = pointer,
 * value-size = map declaration, optional create-on-demand). Delegate
 * to the existing task-storage entry point to avoid duplicating the
 * insert / create logic.
 *
 * Phase 1 BPF infra scale-up item 5: retires the kfuncs.rs:2451
 * NULL-returning stub of `bpf_cgrp_storage_get`. The replacement
 * Rust impl in `unsafe_impl::kfuncs` delegates here for any registered
 * map, falling back to NULL only for unregistered maps (which would
 * be a usage error -- a real BPF program cannot use an unregistered
 * map either).
 */
void *scx_test_cgrp_storage_get(void *map, const void *cgrp_ptr_loc,
				void *value, unsigned long flags)
{
	return scx_test_task_storage_get(map, cgrp_ptr_loc, value, flags);
}

/*
 * Drop a per-cgroup local-storage slot. Same semantics as
 * `bpf_cgrp_storage_delete(map, cgrp)`.
 */
int scx_test_cgrp_storage_delete(void *map, const void *cgrp_ptr_loc)
{
	return scx_storage_delete(map, cgrp_ptr_loc);
}

int scx_test_map_update_percpu_elem(void *map, const void *key, const void *value,
				    int cpu, unsigned long flags)
{
	RBC_GUARD_START;
	struct scx_test_map *test_map = scx_test_map_lookup(map, cpu);
	if (!test_map)
		RBC_GUARD_RETURN(-1);
	RBC_GUARD_RETURN(map_update_elem(test_map, key, value, flags));
}

void scx_test_map_register(struct scx_test_map *map, void *map_ptr)
{
	int index = scx_map_entries_count;

	scx_map_entries_count++;
	scx_map_entries = reallocarray(scx_map_entries, scx_map_entries_count,
				       sizeof(struct scx_map_entry));
	if (!scx_map_entries) {
		perror("Failed to allocate memory for scx_map_entries");
		exit(EXIT_FAILURE);
	}

	scx_map_entries[index].map_ptr = map_ptr;
	scx_map_entries[index].map = map;
	scx_regsiter_map_type(map_ptr, SCX_MAP_TYPE_NORMAL);
}

struct scx_percpu_test_map *scx_alloc_percpu_test_map(int nr_cpus)
{
	struct scx_percpu_test_map *map = malloc(sizeof(struct scx_percpu_test_map));
	if (!map) {
		perror("Failed to allocate memory for scx_percpu_test_map");
		exit(EXIT_FAILURE);
	}
	map->per_cpu_maps = calloc(nr_cpus, sizeof(struct scx_test_map));
	if (!map->per_cpu_maps) {
		perror("Failed to allocate memory for per_cpu_maps");
		free(map);
		exit(EXIT_FAILURE);
	}
	map->nr_cpus = nr_cpus;
	return map;
}

void scx_init_percpu_test_map(struct scx_percpu_test_map *map, unsigned int max_entries,
			      unsigned int key_size, unsigned int value_size)
{
	for (int i = 0; i < map->nr_cpus; i++) {
		map->per_cpu_maps[i].keys = NULL;
		map->per_cpu_maps[i].values = NULL;
		map->per_cpu_maps[i].max_entries = max_entries;
		map->per_cpu_maps[i].key_size = key_size;
		map->per_cpu_maps[i].value_size = value_size;
		map->per_cpu_maps[i].nr = 0;
	}
}

void scx_register_percpu_test_map(struct scx_percpu_test_map *map, void *map_ptr)
{
	int index = scx_percpu_map_entries_count;

	scx_percpu_map_entries_count++;
	scx_percpu_map_entries = reallocarray(scx_percpu_map_entries, scx_percpu_map_entries_count,
					      sizeof(struct scx_percpu_map_entry));
	if (!scx_percpu_map_entries) {
		perror("Failed to allocate memory for scx_percpu_map_entries");
		exit(EXIT_FAILURE);
	}

	scx_percpu_map_entries[index].map_ptr = map_ptr;
	scx_percpu_map_entries[index].map = map;
	scx_regsiter_map_type(map_ptr, SCX_MAP_TYPE_PERCPU);
}
