# BPF/C Undefined-Behaviour Fidelity Policy

**Status:** adopted. Enforced by `scripts/check_ub_fidelity.sh`, run from
`validate.sh`.
**Date:** 2026-08-12
**Task:** tg `bpf-semantics-ub-fidelity-policy`

scxsim compiles BPF scheduler source as ordinary userspace C. BPF and C
disagree about several classes of undefined behaviour, so scxsim's semantics
can silently diverge from the kernel's. This document states, per UB class,
what the kernel actually does, what scxsim does, and where the two cannot be
reconciled.

Every kernel claim below cites `torvalds/linux` **v6.12** source. Claims marked
**MEASURED** were verified by loading BPF programs on this dev machine
(`6.13.2-0_fbk15_hardened`) with `bpftool prog load`, or by compiling and
running the C in question.

---

## The finding that reframes the whole question

The intuitive model — *"the verifier rejects undefined behaviour, so scxsim
must reject it too"* — is wrong in both directions, and the corrections point
opposite ways.

**1. The verifier does NOT reject uninitialised stack reads for the programs we
care about.** `verifier.c:22334` sets

```c
env->allow_uninit_stack = bpf_allow_uninit_stack(env->prog->aux->token);
```

and `include/linux/bpf.h:2353` defines that as `bpf_token_capable(token,
CAP_PERFMON)`. At `verifier.c:4981` and `:5029` the `STACK_INVALID` rejection is
explicitly skipped when that flag is set, and the destination register is then
`mark_reg_unknown()` — an unconstrained scalar. **Every scx scheduler loads
privileged**, so this path is always taken. The kernel reads whatever garbage is
in the slot, exactly like native C does.

**MEASURED.** A program whose first instruction is `r1 = *(u64 *)(r10 - 0x8)`
with no preceding store — from `volatile long x; long y = x;` — loads
successfully as root.

**2. The verifier DOES reject uninitialised register reads, unconditionally.**
`verifier.c:3338`, in `check_reg_arg()`, has no privilege gate:

```c
if (reg->type == NOT_INIT) {
        verbose(env, "R%d !read_ok\n", regno);
        return -EACCES;
}
```

**MEASURED.** A naked program containing `r0 = r7; exit;` is rejected with
`-EACCES` and the verifier log line `R7 !read_ok`.

**3. Which one a C-level uninitialised variable becomes is a register
allocation decision — and it differs per target.** This is the crux.

The motivating bug is `match_substr()` in
`scx/scheds/rust/scx_layered/src/bpf/util.bpf.c:181`, which compares against
`y` before the inner `bpf_for` at `:184` assigns it. Compiling that file for
`-target bpf` at `scx@59c30ba` and disassembling shows the compare as
`if w1 s< w9` (insn 340), with clang having allocated `y` to `r9` — which at
that point holds the **live `str_buf` map pointer**, spilled at insn 318.

So at BPF level this is not an uninitialised read at all. `r9` is initialised,
the verifier is satisfied, and `y` takes the low 32 bits of a map-value pointer.
The register allocator made the value defined-but-garbage *before* the verifier
ever looked at it.

**The verifier's uninitialised-read checks run on compiled bytecode, not on C.**
A given uninitialised C variable may become a rejected register, an allowed
stack slot, or a silently-reused live register, and the answer differs between
the BPF target and the x86-64 target scxsim compiles for. No single C compiler
flag can close that gap. The C source is the only place the whole class is
visible, which is why the policy below is built around source-level detection.

---

## Policy, per UB class

### 1. Uninitialised reads

| | |
|---|---|
| **Kernel — stack** | ALLOWED for privileged loaders; register becomes an unconstrained scalar. `verifier.c:4981,5029`, `bpf.h:2353`. MEASURED. |
| **Kernel — register** | REJECTED, `R%d !read_ok`, `-EACCES`. `verifier.c:3338`. MEASURED. |
| **Native C** | UB. In practice: reads stack residue, or reuses whatever was in the register. |
| **scxsim policy** | **DETECT AT SOURCE. Never substitute a value.** |

`-Wconditional-uninitialized` is on in `schedulers/Makefile` `CFLAGS_BASE`.

**This specific flag is load-bearing.** Neither `-Wall` nor `-Wuninitialized`
catches the `match_substr` shape — measured against clang 22, both report zero
warnings on that file. Only `-Wconditional-uninitialized` flags it (at
`util.bpf.c:181:21`, naming `y`).

It is a **warning, never `-Werror`**. It has a real false-positive rate against
the `bpf_for()` / `can_loop()` macro shape — on `util.bpf.c` it reports four
sites of which one (`y`) is the genuine bug, and the current in-tree scheduler
build reports exactly one site, `scx_lavd/src/bpf/power.bpf.c:138` (`p_pressure`),
which is a false positive from `&&` short-circuit correlation. The source it
inspects is vendored upstream scx that we do not control, so `-Werror` would
break the build on every submodule bump.

**`-ftrivial-auto-var-init=zero` is REJECTED**, but not for the reason usually
given. The objection is not "zeroing is more permissive than a rejecting
verifier" — for the stack class the verifier does not reject. The objection is
that zero substitutes **one plausible-looking value** for the kernel's
unconstrained garbage, so a divergence looks like a correct answer. Zero
**hides** this bug class. `-ftrivial-auto-var-init=pattern` is available in UB
probe mode (below) precisely because it does the opposite: it makes a surviving
uninitialised read produce an obviously-bogus `0xAA..` value.

### 2. Division and modulo by zero

| | |
|---|---|
| **Kernel — constant divisor** | REJECTED at verification: `"div by zero"`, `verifier.c:14505`. |
| **Kernel — variable divisor** | DEFINED at runtime. The verifier rewrites every such site with a guard patchlet (`verifier.c:20534-20595`): `x div 0 -> 0`, `x mod 0 -> x`, `x sdiv 0 -> 0`, `x smod 0 -> x`, `LLONG_MIN sdiv -1 -> LLONG_MIN`, `INT_MIN sdiv -1 -> INT_MIN`, `x smod -1 -> 0`. |
| **Native C** | UB; x86-64 raises `#DE` → `SIGFPE`. |
| **scxsim policy** | **MATCH the runtime semantics where the value is read; DETECT the rest.** |

`csrc/sim_sigfpe.c` implements the patchlet semantics exactly. x86-64 writes the
quotient to `(R|E)AX` and the remainder to `(R|E)DX`, so the handler sets both to
their BPF-defined values and satisfies the div rule and the mod rule
simultaneously, whichever one the compiler goes on to read.

The handler decodes the faulting instruction fully, including SIB, displacement
and RIP-relative memory operands, because it must distinguish `divisor == 0`
from `divisor == -1`: those have **different** BPF-defined results. Memory
operands are not hypothetical — clang divides straight out of memory for globals
and struct fields.

`csrc/tests/sim_bpf_ub_semantics_test.c` asserts all twelve cases at `-O0` and
`-O2`.

**This handler previously got it wrong.** It set `RAX=0, RDX=0`
unconditionally, which is right for division and wrong for modulo (BPF yields
the dividend) and wrong for signed overflow (BPF yields the dividend). Six of
the twelve cases failed. `x % 0` inside a simulated scheduler silently produced
`0` where the kernel produces `x`.

**KNOWN RESIDUAL DIVERGENCE — a SIGFPE handler cannot fully fix this class.**
The compiler optimises *around* the division on the assumption that it never
traps, before any signal exists. Measured at `-O2`:

```c
uint32_t q = ua32 / uz32;   /* uz32 is volatile and zero */
printf("%u %d", q, q == 0);  /* prints "0 0" — q is 0, but q == 0 is FALSE */
```

clang rewrote `q == 0` into `divisor > dividend`, algebraically valid for
unsigned division **only** if the divisor is nonzero. The comparison never reads
`EAX`, so fixing `EAX` in the handler cannot fix it.

The handler is therefore a **partial mitigation**: it fixes the value at sites
that actually read the result register. It is not a semantics guarantee. This
also explains the `sed` patch in `schedulers/cosmos/config.mk` that inserts an
`interval ? ... : 0` guard at one cosmos division site — the handler alone was
not sufficient there either. That `sed` is a symptom, not a fix; the general
remedies are UB probe mode (detect) or a source-level guard (per site).

### 3. Shifts

| | |
|---|---|
| **Kernel — constant shift ≥ width** | REJECTED: `"invalid shift %d"`, `verifier.c:14511-14519`. |
| **Kernel — variable shift** | DEFINED: masked to the operand width. `kernel/bpf/core.c:1760-1766`: `DST = DST OP (SRC & 63)` for 64-bit, `((u32) SRC & 31)` for 32-bit. The comment there is explicit that JIT backends must **not** add the AND, because the hardware already masks. |
| **Native C** | UB for shift counts ≥ width. x86 happens to mask by 63/31, so this usually coincides — but that is hardware accident, not a guarantee, and the optimiser may still exploit the UB. |
| **scxsim policy** | **DETECT** via `shift-exponent` in UB probe mode. Not otherwise handled. |

### 4. Signed integer overflow

| | |
|---|---|
| **Kernel** | DEFINED: BPF arithmetic is two's-complement wrapping. |
| **Native C** | UB. |
| **scxsim policy** | **DETECT** via `signed-integer-overflow` in UB probe mode. |

### 5. Out-of-bounds access and pointer arithmetic

| | |
|---|---|
| **Kernel** | REJECTED at verification, with per-region diagnostics: `"invalid access to map value, value_size=%d off=%d size=%d"` (`verifier.c:5226`), `"invalid access to map key"` (`:5222`), `"invalid access to packet"` (`:5232`), `"R%d unbounded memory access, make sure to bounds check any such access"` (`:5285`), `"math between %s pointer and register with unbounded min value is not allowed"` (`:12916`). |
| **Native C** | UB; typically no diagnostic, sometimes a segfault. |
| **scxsim policy** | **OUT OF SCOPE for the C build — use the real verifier.** See below. |

This is the one class where emulating the kernel in C flags is hopeless. The
verifier's bounds reasoning is a whole-program abstract interpretation over
tracked register ranges; ASan approximates a strictly different property
(spatial safety of the *host* allocation). The right gate is not a C flag but
running the actual verifier over the actual BPF object — see "What this policy
does not cover".

---

## Build configurations

`schedulers/Makefile` has three orthogonal switches. **All four combinations
build clean and report identical uninitialised-read warnings**, which was not
previously true — the motivating `match_substr` failure only manifested under
coverage flags, meaning the build configs disagreed about which UB was visible.

| Switch | Effect |
|---|---|
| *(default)* | `-Wconditional-uninitialized` on. SIGFPE handler active. Production-fidelity. |
| `SCX_SIM_COVERAGE=1` | Adds `-fprofile-instr-generate -fcoverage-mapping`. Changes stack layout, so it changes *which* garbage an uninitialised read sees — this is why UB bugs are coverage-config-sensitive. |
| `SCX_SIM_UB_PROBE=1` | Adds `-ftrivial-auto-var-init=pattern` and trapping UBSan for `integer-divide-by-zero`, `shift-exponent`, `signed-integer-overflow`. |

UB probe mode is **opt-in and never the default**, per Twin Design Principle 2:
the default must match production, and the production kernel does not abort on
any of these — it defines them. Probe mode is an exaggerated-diagnostics mode
that converts silent divergence into a loud, deterministic abort at the exact
site. Trapping UBSan (`-fsanitize-trap=`) is used rather than the diagnostic
runtime because the scheduler `.so` files link `-nostdlib`.

Three translation units are deliberately built with `CFLAGS_BASE` only, excluded
from both coverage and UB instrumentation: `sim_sigfpe.c` (decodes instructions
at RIP; instrumentation changes the layout it decodes), `sim_rbc_trampoline.c`
and `sim_deterministic_mem.c` (hot paths whose determinism is the point). This
exclusion predates the policy; it is correct and is retained.

Running probe mode:

```bash
make -C schedulers SCX_SIM_UB_PROBE=1 BUILD_DIR=<dir> BPF_INCLUDE=<dir>
```

---

## Enforcement

`scripts/check_ub_fidelity.sh`, called from `validate.sh`:

1. **Division semantics.** Builds and runs
   `csrc/tests/sim_bpf_ub_semantics_test.c` at `-O0` and `-O2`; all twelve
   verifier-defined results must match.
2. **Detector wired on.** Compiles `csrc/tests/uninit_canary.c` — a
   deliberately-broken file carrying the `match_substr` shape — and asserts the
   warning fires; then asks `make print-CFLAGS_BASE` for the *effective* flags
   and asserts `-Wconditional-uninitialized` is really in the compile line
   rather than merely mentioned in a comment.
3. **Warning inventory.** Reports uninitialised-read warnings from the cached
   scheduler build. Advisory, given the known false-positive rate.

Both gates were verified to fail when they should: restoring the old SIGFPE
handler fails check 1 with 6/12; removing the flag from `CFLAGS_BASE` fails
check 2.

---

## What this policy does not cover

**scxsim never runs the BPF verifier.** Everything above is about matching
*runtime* semantics. The verifier's *static* rejections — uninitialised
registers, out-of-bounds access, unbounded pointer arithmetic, constant
division by zero, constant over-width shifts — are load-time gates that scxsim
simply does not have. A program scxsim runs happily may be unloadable in
production.

Closing that gap means running the real verifier, not approximating it: compile
each supported scheduler for `-target bpf` and load it (or run `veristat`) as a
separate CI gate. `veristat` is not currently used anywhere in the scx
submodule. Filed as **`sim-ixinx`** rather than done here, because it needs a
per-scheduler BPF build that the scxsim build system does not currently
produce.

Two further residual gaps are recorded rather than fixed:

- **`sim-kykh4`** — the compiler-optimises-around-division problem, which no
  handler can fix, together with the `cosmos/config.mk` `sed` patch that guards
  one division site at source. Nobody has yet measured how many division sites
  actually reach a zero divisor; running the suite under `SCX_SIM_UB_PROBE=1`
  is the first step.
- **`sim-7rl9v`** — the upstream `match_substr` bug itself. Whether upstream
  layered's cgroup substring matching actually misbehaves in production is not
  established here: on the BPF target the value is a map pointer's low bits, so
  the outcome depends on the allocation address.
