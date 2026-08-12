---
name: scxsim-shimify-scheduler
description: Wire an upstream scx scheduler into scxsim via a wrapper.c shim — include order, kernel-capability (bpf_ksym_exists) rules, and how to verify a compat branch actually compiled the way you think it did
---

# Shimify a scheduler into scxsim

`scx-sim/schedulers/<name>/wrapper.c` is the shim that lets an upstream
`*.bpf.c` scheduler compile and run as a native `.so` under scxsim. The
Makefile discovers schedulers automatically:

```make
SCHEDS := $(patsubst %/wrapper.c,%,$(wildcard */wrapper.c))
```

so a new directory containing a `wrapper.c` is all it takes to be built.

Everything below is subordinate to the **No-Stub Rule** in
`scx-sim/CLAUDE.md`: scxsim must run 100% of the scheduler's real logic.
A shim exists to supply the *kernel* side, never to stand in for the
scheduler's own decisions.

---

## 1. The include-order mechanic (read this first)

This single fact resolves most "why is this compat branch behaving like
that" questions, and it is not obvious from reading any one file.

```
wrapper.c
  #include "sim_wrapper.h"
      └── #include <scx/common.bpf.h>
              └── common.bpf.h:1143  #include "compat.bpf.h"   <-- parsed HERE
  #undef  bpf_ksym_exists          <-- wrapper's own overrides run AFTER
  #define bpf_ksym_exists(sym) ...
  #include "<scheduler>.bpf.c"     <-- re-includes common.bpf.h; header
                                       guard already set, so it is SKIPPED
```

`compat.bpf.h` is fully parsed inside `sim_wrapper.h`, **before** any
override in `wrapper.c`. Therefore:

| compat construct | when `bpf_ksym_exists` binds | affected by a wrapper override? |
|---|---|---|
| `static inline` **function** (`__COMPAT_scx_bpf_cpu_curr`, `__COMPAT_scx_bpf_dsq_peek`, `scx_bpf_dsq_insert`, `scx_bpf_task_set_slice`, `scx_bpf_task_set_dsq_vtime`, `__COMPAT_scx_bpf_reenqueue_local_from_anywhere`, `__COMPAT_has_generic_reenq`, `scx_bpf_dsq_reenq`) | at parse time, inside `sim_wrapper.h` | **NO — immune** |
| **macro** (`__COMPAT_scx_bpf_cpu_node`, `__COMPAT_scx_bpf_*_node`, `__COMPAT_HAS_scx_bpf_select_cpu_and`, `scx_bpf_dsq_move*`, `__COMPAT_bpf_cpumask_populate`, …) | at the call site in the `.bpf.c` | **YES** |

Consequence: a wrapper-level `#define bpf_ksym_exists(sym) (0)` can look
like it disables everything while actually changing nothing, because the
constructs that scheduler happens to use are all inline functions. This
exact confusion produced a false P0 in Aug 2026. **Do not reason about it
from the `#define` — verify with `objdump` (§3).**

---

## 2. Kernel-capability policy: never fake `bpf_ksym_exists`

`bpf_ksym_exists(sym)` is libbpf's `!!sym` on a `__weak` symbol.
`__weak` survives into this native build (`lib/scxtest/scx_test.h:6` maps
it to `__attribute__((weak))`), so it is a **genuine runtime NULL check**
against what the process actually provides:

- symbols the simulator binary exports — `#[no_mangle]` in `kfuncs.rs`,
  resolved into the `.so` at `dlopen` time via `-rdynamic`; plus
- symbols the scheduler's own `wrapper.c` defines.

**That set IS the capability table.** It is ground truth by construction
and cannot drift the way a hand-maintained list would. Inspect it:

```sh
nm -D --defined-only target/debug/scxsim | awk '{print $3}' | sort -u
```

### The rule

> To make a modern path run, **provide the symbol** — do not fake the test.

Export the kfunc from `kfuncs.rs`, or define it in that scheduler's
`wrapper.c`. The genuine test then answers `TRUE` by itself and *keeps*
answering correctly as upstream moves.

`cosmos/wrapper.c` is the worked example: it needs
`__COMPAT_scx_bpf_cpu_node()` to take the modern branch, so it **defines
`scx_bpf_cpu_node()`** against its own `cpu_node_map`. The real test finds
the symbol and returns true — no lie required.

### Why forcing the constant is wrong

`bpf_ksym_exists` is consulted for ~31 different symbols. A blanket
`#define` answers for **all of them at once** to fix the one that
motivated it:

- Forcing `0` sends every macro-form capability down its legacy branch,
  including ones the simulator fully supports.
- Forcing `1` asserts that symbols the simulator does **not** export do
  exist. Each such call then jumps through a NULL weak `__ksym` and
  SIGSEGVs the moment its guard condition goes true.

Both are fake values standing in for a real capability test — the
No-Stub Rule's core prohibition. And an unexplained forced constant is
indistinguishable from a stub to the next reader.

### If you genuinely need a per-symbol override

Make it **per symbol**, never blanket, and **put the reason in the code**
next to it. State what breaks without it. A reader must be able to tell
your override from a stub without running `git log`.

---

## 3. Verify what actually compiled

Reading the `#define` is not evidence. Relocations are.

```sh
objdump -R <sched>.so | grep GLOB_DAT     # genuine runtime test survived
objdump -R <sched>.so | grep JUMP_SLOT    # symbol is actually called
objdump -d <sched>.so                     # read the branch itself
```

Interpretation:

- **`GLOB_DAT` on the symbol** → the compiler emitted a real NULL test;
  the genuine capability check is in the binary.
- **`JUMP_SLOT` but no `GLOB_DAT`** → the test was folded to a constant.
  Someone overrode it (or the symbol is locally defined, so the compiler
  proved it non-NULL — check for a `W`/`T` definition in the same `.so`).

A correctly-compiled genuine test looks like this:

```
<__COMPAT_scx_bpf_cpu_curr>:
  cmpq $0x0,0xad58(%rip)        # <scx_bpf_cpu_curr>   <-- runtime NULL test
  jne  4b0 <scx_bpf_cpu_curr@plt>                      <-- modern path
  call 320 <scx_bpf_cpu_rq@plt>                        <-- legacy fallback
```

Both branches present + a `cmpq` against the GOT slot = genuine.

> **Gotcha that has bitten twice:** `nm -D --undefined-only` hides symbols
> that are *defined locally* in the `.so`. Grepping only undefined symbols
> makes a fallback look absent when it is right there. Use `nm -D` plain,
> plus `objdump -R`, plus the disassembly. Do not conclude from one of the
> three.

### Regression-checking a shim change

Capture relocations before and after; an inert change diffs to nothing:

```sh
for so in <out>/schedulers/*.so; do
  objdump -R "$so" | awk '/GLOB_DAT|JUMP_SLOT/{print $2, $3}' | sort > /tmp/$(basename $so).before
done
# ... make your change, rebuild ...
diff /tmp/libscx_<name>.so.before /tmp/libscx_<name>.so.after
```

---

## 4. Other shim conventions

- **Provide the kernel side, never the scheduler side.** Maps, timers,
  cpumasks, task fields, kfuncs are fair game. Dispatch policy, throttle
  decisions, latency scoring are not — see "Don't Model the Scheduler" in
  `scx-sim/CLAUDE.md`.
- **`BPF_STRUCT_OPS`** is redefined in `sim_wrapper.h` to emit plain C
  functions; `SCX_OPS_DEFINE` becomes a no-op. The engine calls the ops
  functions by name.
- **Config globals** (`slice_ns`, feature flags) are set from a
  `<sched>_setup()` the engine calls before `<sched>_init()`. Prefer
  *disabling* what the engine cannot model (`no_freq_scaling = true`)
  over *enabling* a feature it cannot really support — enabling one means
  the scheduler's genuine "feature off" path never runs, and whatever the
  substrate feeds it is a fabricated input. `lavd/wrapper.c` is the model:
  it declares its limits (`is_smt_active = false`, `nr_llcs = 1`) instead
  of hiding them.
- **Map overrides** go through `scx_test_map` (`lib/scxtest/`). Per-symbol
  `#undef`/`#define` of `bpf_map_lookup_elem` and friends is normal and
  expected — that is the kernel side.
- **Keep `sim_wrapper.h` authoritative.** Anything every scheduler needs
  belongs there, not copy-pasted per wrapper. Three wrappers answering the
  same question three different ways is what made the Aug 2026 capability
  confusion possible in the first place.
