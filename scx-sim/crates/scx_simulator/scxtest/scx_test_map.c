#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include <linux/bpf.h>

#include "scx_test_map.h"
#include "sim_rbc_guard.h"

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
	struct scx_test_map *test_map = scx_test_map_lookup(map, 0);
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

void *scx_test_task_storage_get(void *map, const void *key, void *value,
				unsigned long flags)
{
	RBC_GUARD_START;
	void *newvalue = NULL;
	struct scx_test_map *test_map;

	void *ret = scx_test_map_lookup_elem(map, key);
	if (ret)
		RBC_GUARD_RETURN(ret);

	if (!(flags & BPF_LOCAL_STORAGE_GET_F_CREATE))
		RBC_GUARD_RETURN(NULL);

	test_map = scx_test_map_lookup(map, 0);

	/*
	 * If no value is specified we have to allocate an empty value and
	 * insert it.
	 */
	if (!value) {
		newvalue = calloc(1, test_map->value_size);
		if (!newvalue) {
			perror("Failed to allocate empty value for scx_test_task_storage_get");
			exit(EXIT_FAILURE);
		}
		value = newvalue;
	}

	map_update_elem(test_map, key, value, BPF_ANY);
	if (newvalue)
		free(newvalue);

	RBC_GUARD_RETURN(scx_test_map_lookup_elem(map, key));
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
 * Delete a (key) from a scx_test_map.
 *
 * Returns 0 on success, -1 if not found. Used to back
 * `bpf_task_storage_delete` and `bpf_cgrp_storage_delete` in the
 * simulator -- the kernel API contract is the same: drop the slot
 * for `key`, return -ENOENT if it wasn't there.
 *
 * Phase 1 BPF infra scale-up (tg `scxsim-bpf-infra-scale-up-phase1`,
 * design doc section Phase 1 item 5): retires the prior NULL-returning
 * delete stub so Phase 2's compiled-in `cgroup_bw.bpf.c` can call
 * `bpf_cgrp_storage_delete(&cbw_cgrp_map, cgrp)` and observe the
 * production semantics (slot dropped, subsequent
 * `bpf_cgrp_storage_get(... 0)` returns NULL).
 *
 * Implementation: linear scan, swap-remove. The map is small
 * (max_entries bounded by the BPF map declaration) and the simulator
 * is single-threaded so no locking is needed. Subsequent inserts may
 * reuse the freed slot index.
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
 * contract. The map is treated as a hash table keyed by the
 * `struct cgroup *` pointer (the same way `scx_test_task_storage_get`
 * uses the `struct task_struct *` pointer). The value_size is read
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
	return scx_test_map_delete_elem(map, cgrp_ptr_loc);
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
