---
title: sim_atq's taskc field offsets are process globals, and their defaults went stale with the scx pin bump (56/64, now 48/56)
status: open
priority: 1
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.584497578+00:00
updated_at: 2026-09-25T03:43:12.584497578+00:00
---

# Description

csrc/sim_atq.c reads and writes two fields inside the scheduler's struct scx_task_common: the int holdcnt and the scx_atq_t *atq back-pointer. Their offsets are process globals set through sim_atq_set_taskc_holdcnt_offset() and sim_atq_set_taskc_atq_offset(). lavd's wrapper.c calls both with __builtin_offsetof from lavd_register_cbw_maps. Every other path gets the file's hard-coded defaults: holdcnt 56, atq 64.

Those defaults went stale with the scx pin bump to 413031d4. Upstream 9ca8390bd ('scx: lib - Add scx_free() and drop the embedded allocation ids') removed 'union sdt_id tid' from struct rbnode, which is the first member of scx_task_common, so the production offsets are now holdcnt 48 and atq 56 (struct size 72). Read from the DWARF of the libscx_lavd.so built by the release-candidate consumer:
  gdb -batch -ex 'ptype /o struct scx_task_common' <out>/schedulers/libscx_lavd.so
lavd sets the offsets itself, so the six bundled schedulers are unaffected. Their traces are unchanged across the bump.

Any other ATQ user that does not call the two setters gets the stale defaults. scx_p2dq, for example, uses lib/atq and would have to be built by an embedder. For such a scheduler sim_atq stores the 8-byte back-pointer at offset 64, over 'state' and its padding, and pop(hold=true) and task_drop add to or subtract from the 32 bits at offset 56, which is the low half of the atq pointer. The result is silent memory corruption inside the scheduler's task context, with no error. The comment block above the defaults (a 56-byte rbnode, 'OFFSET 56/64') and the header of tests/atq_operations.rs ('the production offsets 56/64') describe the pre-bump layout.

Fix (No Silent Failures): remove the defaults. Start both offsets at an 'unset' sentinel and make every sim_atq entry point that touches a taskc fail loudly (scx_bpf_error or abort, naming the missing registration) when they are unset. Better still, register the offsets from code compiled into each scheduler's own translation unit (for example from the scxsim-build wrapper preamble), so every .so built through build_schedulers registers its own layout and a layout change cannot desynchronise them. The globals are also shared across loaded schedulers: one process that loads two ATQ schedulers compiled against different layouts would run one of them with the other's offsets.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
