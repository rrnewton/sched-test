#pragma once

struct scx_test_map {
	void *keys;
	/*
	 * `values` has two interpretations, discriminated by access path (there is
	 * no map-type field on the struct):
	 * - NORMAL/HASH/PERCPU maps (scx_test_map_lookup_elem / map_update_elem /
	 *   scx_test_map_delete_elem): a contiguous value_size-strided buffer
	 *   accessed via SCX_MAP_VALUE, reallocarray'd on growth.
	 * - TASK/CGRP local storage (scx_test_task_storage_get / scx_storage_delete):
	 *   a void** array of individually-allocated, lifetime-stable slots; growth
	 *   reallocs only the pointer array, never moving a returned slot.
	 * Safe because BPF map-type segregation guarantees a storage map is reached
	 * only via the storage entry points and a NORMAL/HASH/PERCPU map only via
	 * the contiguous ones, never both.
	 */
	void *values;
	unsigned int max_entries;
	unsigned int key_size;
	unsigned int value_size;
	int nr;
};

/* Byte-level element accessors — stride by key_size / value_size. */
#define SCX_MAP_KEY(map, i)   ((void *)((char *)(map)->keys + (size_t)(i) * (map)->key_size))
#define SCX_MAP_VALUE(map, i) ((void *)((char *)(map)->values + (size_t)(i) * (map)->value_size))

struct scx_percpu_test_map {
	struct scx_test_map *per_cpu_maps;
	int nr_cpus;
};

void scx_test_map_register(struct scx_test_map *map, void *map_ptr);
void scx_test_map_clear_all(void);
void *scx_test_map_lookup_elem(void *map, const void *key);
void *scx_test_map_lookup_percpu_elem(void *map, const void *key, int cpu);
int scx_test_map_update_elem(void *map, const void *key, const void *value,
			     unsigned long flags);
struct scx_percpu_test_map *scx_alloc_percpu_test_map(int nr_cpus);
void scx_init_percpu_test_map(struct scx_percpu_test_map *map, unsigned int max_entries,
			      unsigned int key_size, unsigned int value_size);
void scx_register_percpu_test_map(struct scx_percpu_test_map *map, void *map_ptr);
void *scx_test_task_storage_get(void *map, const void *key, void *value,
				unsigned long flags);
/*
 * Delete a task/cgrp local-storage slot by object identity (the 8-byte pointer
 * value at `key`). Backs bpf_task_storage_delete / bpf_cgrp_storage_delete.
 * Pairs with scx_test_task_storage_get's per-slot void** storage (frees the
 * slot it allocated).
 */
int scx_storage_delete(void *map, const void *key);

/*
 * Real per-cgroup local storage.
 *
 * `scx_test_cgrp_storage_get(map, cgrp_ptr_loc, value, flags)` returns
 * the per-cgroup slot for `cgrp_ptr_loc` (the address of a
 * `struct cgroup *` value) in `map`, allocating a new zero-initialized
 * (or `value`-initialized) slot when `flags &
 * BPF_LOCAL_STORAGE_GET_F_CREATE` is set and the slot did not already
 * exist. Returns NULL if no slot exists and CREATE was not requested.
 *
 * `scx_test_cgrp_storage_delete(map, cgrp_ptr_loc)` drops the slot;
 * returns 0 on success, -1 if the slot was not present.
 *
 * Both functions delegate to the same pointer-identity-keyed per-slot
 * storage that backs `scx_test_task_storage_get` (key = the 8-byte object
 * pointer value; each value slot is individually allocated and stable
 * across other objects' inserts). The BPF map must have been registered
 * via `scx_test_map_register` so that `value_size` is known; the wrapper.c
 * initializer chain uses `INIT_SCX_TEST_MAP_FROM_TASK_STORAGE` against the
 * cgroup-storage BPF map declaration.
 */
void *scx_test_cgrp_storage_get(void *map, const void *cgrp_ptr_loc,
				void *value, unsigned long flags);
int scx_test_cgrp_storage_delete(void *map, const void *cgrp_ptr_loc);
/* Delete an element from a NORMAL/HASH map (contiguous values). Backs
 * bpf_map_delete_elem for schedulers that route it here (e.g. lavd's
 * cbw_cgrp_map). Distinct from scx_storage_delete (TASK/CGRP local storage). */
int scx_test_map_delete_elem(void *map, const void *key);

/*
 * The kernel doesn't have this, it always does it on it's current CPU, but we
 * need this for unit testing to update a specific cpu's map.
 */
int scx_test_map_update_percpu_elem(void *map, const void *key, const void *value,
				    int cpu, unsigned long flags);

#define MAX_ENTRIES(bpfmap) sizeof(*bpfmap.max_entries) / sizeof((*bpfmap.max_entries)[0])

#define INIT_SCX_TEST_MAP(map, bpfmap) \
	do { \
		(map)->values = NULL; \
		(map)->keys = NULL; \
		(map)->max_entries = MAX_ENTRIES(bpfmap); \
		(map)->key_size = sizeof(typeof(*bpfmap.key)); \
		(map)->value_size = sizeof(typeof(*bpfmap.value)); \
		(map)->nr = 0; \
	} while (0)

/*
 * TASK_STORAGE / CGRP_STORAGE maps are keyed by the OBJECT IDENTITY (the
 * task_struct* / cgroup* pointer VALUE), exactly like the kernel's
 * BPF_MAP_TYPE_{TASK,CGRP}_STORAGE. The .bpf.c-declared key type (int/u64) is
 * a BPF-verifier artifact, not the real key — so key_size is the pointer
 * width, not sizeof(declared key). bpf_task_storage_get / scx_test_cgrp_storage_get
 * pass the ADDRESS of the object pointer so the generic storage compares the
 * full pointer value (see bpf_task_storage_get below). A declared int key
 * (4 bytes) would otherwise truncate / mis-key the identity.
 */
#define INIT_SCX_TEST_MAP_FROM_TASK_STORAGE(map, bpfmap) \
	do { \
		(map)->values = NULL; \
		(map)->keys = NULL; \
		(map)->max_entries = 100; \
		(map)->key_size = sizeof(void *); \
		(map)->value_size = sizeof(typeof(*bpfmap.value)); \
		(map)->nr = 0; \
	} while (0)

#define INIT_SCX_PERCPU_TEST_MAP(map, bpfmap) \
	scx_init_percpu_test_map(map, MAX_ENTRIES(bpfmap), \
		sizeof(typeof(*bpfmap.key)), sizeof(typeof(*bpfmap.value)))

#define bpf_map_lookup_elem(map, key) scx_test_map_lookup_elem(map, key)
#define bpf_map_lookup_percpu_elem(map, key, cpu) scx_test_map_lookup_percpu_elem(map, key, cpu)
#define bpf_map_update_elem(map, key, value, flags) \
	scx_test_map_update_elem(map, key, value, flags)
/*
 * Task-local storage is keyed by the task IDENTITY — the task_struct pointer
 * VALUE — like the kernel's BPF_MAP_TYPE_TASK_STORAGE. Bind the task pointer
 * to a temp and pass its ADDRESS so the generic storage compares the 8-byte
 * pointer value (scx_test_task_storage_get memcmps sizeof(void*) at the key).
 * Passing `task` directly would make memcmp DEREFERENCE it and compare the
 * task_struct's leading bytes, which are identically zero across all sim tasks
 * (sim_task_alloc calloc's the struct; the leading thread_info is never set)
 * → every task aliases to one slot. The statement-expression temp also accepts
 * rvalue arguments (e.g. the (struct task_struct *)p casts in cosmos/tickless).
 * Mirrors the Rust kfunc bpf_task_storage_get, which already passes &task.
 */
#define bpf_task_storage_get(map, task, value, flags) \
	({ void *scx_obj_key_ = (void *)(task); \
	   scx_test_task_storage_get(map, &scx_obj_key_, value, flags); })
/*
 * Per-cgroup local storage, same object-identity model as bpf_task_storage_get.
 * libbpf's bpf_cgrp_storage_get/_delete are helper-ID pointer constants that
 * would SIGSEGV if called in the simulator, so a macro is mandatory. Bind the
 * cgroup pointer to a temp and pass its ADDRESS so the storage compares the
 * 8-byte pointer value (scx_test_cgrp_storage_get / scx_storage_delete). A
 * scheduler needing per-cgroup behavior (e.g. lavd's cap-aware accounting)
 * installs its own override that #undefs and shadows this after the header.
 */
#define bpf_cgrp_storage_get(map, cgrp, value, flags) \
	({ void *scx_cgrp_key_ = (void *)(cgrp); \
	   scx_test_cgrp_storage_get(map, &scx_cgrp_key_, value, flags); })
#define bpf_cgrp_storage_delete(map, cgrp) \
	({ void *scx_cgrp_key_ = (void *)(cgrp); \
	   scx_test_cgrp_storage_delete(map, &scx_cgrp_key_); })
