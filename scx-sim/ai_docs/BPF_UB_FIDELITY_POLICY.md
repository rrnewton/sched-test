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

---

## Audit: guaranteed by the ISA, versus observed on one target

**Added 2026-08-12 by tg `audit-ub-table-guaranteed-vs-observed`.**

The table below was built by loading real BPF programs on ONE machine
(`6.13.2-0_fbk15_hardened`, x86-64 JIT) and reading the Linux v6.12 verifier.
Measurement establishes *behaviour*; it cannot establish a *promise*. This audit
re-checked every row against the BPF ISA specification, the standardization
drafts, and the full `bpf@vger.kernel.org` archive, and classifies each as:

- **(i) GUARANTEED** — normative in the ISA spec, or an explicit maintainer
  statement. Safe to rely on; a conforming runtime must reproduce it.
- **(ii) NOT PROMISED** — implementation-defined, or a Linux verifier/JIT
  behaviour that another conforming runtime need not share. **The remedy is
  avoidance, not reliance.** These are not "BPF defines it where C does not";
  they are "nobody portably defines it."
- **(iii) VERIFIER-ONLY** — a load-time acceptance decision by the Linux
  verifier. Real and dependable *for programs loaded on Linux*, but outside the
  ISA, and invisible to a native-C build either way.

| Row | Class | Basis |
|---|---|---|
| Signed overflow wraps | **(i) GUARANTEED** | ISA: *"Underflow and overflow are allowed... the value will wrap."* Previously uncited here. |
| div by zero → 0 | **(i) GUARANTEED** | ISA, normative. Linux implements it via the verifier patchlet; the rule itself is the spec's. |
| mod by zero → dst unchanged | **(i) GUARANTEED**, with a caveat | ISA: ALU64 leaves dst unchanged, but for **32-bit ALU the upper 32 bits are zeroed** — i.e. `dst = (u32)dst`, not `dst`. See the open question below. |
| `LLONG_MIN`/`INT_MIN` sdiv -1 → itself; smod -1 → 0 | **(i) GUARANTEED** | ISA, normative. Matches the patchlet list exactly. |
| **Variable shift masked to width** | **(ii) NOT PROMISED** | **Downgraded by this audit.** Interpreter masks; JITs are implementation-defined per target and deliberately so. See §3. |
| Constant shift ≥ width rejected | **(iii) VERIFIER-ONLY** | `verifier.c:14511-14519`. Not an ISA rule. |
| Constant divisor zero rejected | **(iii) VERIFIER-ONLY** | `verifier.c:14505`. Not an ISA rule. |
| Uninit **stack** read allowed (privileged) | **(iii) VERIFIER-ONLY** | Gated on `CAP_PERFMON` via `bpf_token_capable`. A Linux privilege-model behaviour; the ISA does not mention uninitialised reads at all. |
| Uninit **register** read rejected | **(iii) VERIFIER-ONLY** | `verifier.c:3338`. Not an ISA rule. |
| OOB / pointer arithmetic rejected | **(iii) VERIFIER-ONLY** | The specific diagnostics are Linux implementation detail; the ISA is silent. |

**Scope check performed:** the ISA specification mentions "uninitialised",
"out of bounds" and "verifier" **zero** times. Every (iii) row is therefore
Linux-verifier behaviour by construction, not a portable BPF guarantee. That
does not make those rows wrong — scx schedulers do load on Linux — but it does
mean they describe *this loader*, and a doc that presents them beside the ISA
rules without distinction invites the same category error this audit corrects.

**Method, for reproducibility.** lore's web search is behind an Anubis
proof-of-work challenge; the public-inbox git mirror is not:
`with-proxy git clone --bare https://lore.kernel.org/bpf/0`, extract with
`git cat-file --batch-all-objects --batch`, then `LC_ALL=C grep -a` (plain
`grep -i` in a UTF-8 locale dies on the 1.9 GB corpus and returns empty output
for patterns that DO match — sanity-check any negative). Corpus searched:
192,993 messages, **2019-02-13 onward**; epoch 0 begins then, so pre-2019
traffic is not covered.

**OPEN QUESTION for a follow-up, not fixed here.** The ISA says 32-bit modulo
by zero zeroes the upper 32 bits of the destination, i.e. `dst = (u32)dst`.
The patchlet summary in §2 records this class as `x mod 0 -> x`, which is the
ALU64 rule. Whether `crates/scx_simulator/csrc/sim_sigfpe.c` and the twelve
assertions in `csrc/tests/sim_bpf_ub_semantics_test.c` distinguish the 32-bit
case has NOT been verified by this audit — it is a code question, not a
documentation one, and deserves its own test rather than a claim here.


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
| **Kernel — variable divisor** | DEFINED at runtime, and **GUARANTEED BY THE ISA SPECIFICATION** — not just a Linux implementation detail. The verifier implements it by rewriting every such site with a guard patchlet (`verifier.c:20534-20595`): `x div 0 -> 0`, `x mod 0 -> x`, `x sdiv 0 -> 0`, `x smod 0 -> x`, `LLONG_MIN sdiv -1 -> LLONG_MIN`, `INT_MIN sdiv -1 -> INT_MIN`, `x smod -1 -> 0`. The ISA states the same rules normatively, so any conforming runtime must reproduce them. |
| **Native C** | UB; x86-64 raises `#DE` → `SIGFPE`. |
| **scxsim policy** | **MATCH the runtime semantics where the value is read; DETECT the rest.** |

`crates/scx_simulator/csrc/sim_sigfpe.c` implements the patchlet semantics exactly. x86-64 writes the
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
| **Kernel — variable shift, INTERPRETER** | Masked to the operand width. `kernel/bpf/core.c:1760-1766`: `DST = DST OP (SRC & 63)` for 64-bit, `((u32) SRC & 31)` for 32-bit. |
| **Kernel — variable shift, JIT (what production runs)** | **NOT GUARANTEED. Implementation-defined per target architecture.** |
| **Native C** | UB for shift counts ≥ width. x86 happens to mask by 63/31, so this usually coincides — but that is hardware accident, not a guarantee, and the optimiser may still exploit the UB. |
| **scxsim policy** | **AVOID, do not rely.** DETECT via `shift-exponent` in UB probe mode. Never emulate a masked result. |

**CORRECTION (2026-08-12, tg `audit-ub-table-guaranteed-vs-observed`).** An
earlier revision of this row said the kernel DEFINES variable shifts as masked,
and read the `core.c` comment as saying JIT backends must not add the AND
*because the hardware already masks*. **That is a misreading of the comment, and
the conclusion is wrong for the configuration we actually care about.** The
comment, verbatim from `kernel/bpf/core.c:1750-1758` at v6.12:

> *"...in case of native 64 bit archs such as x86-64 or arm64, the compiler is
> optimizing the AND away for the interpreter. In case of JITs, each of the JIT
> backends compiles the BPF shift operations to machine instructions which
> produce **implementation-defined results** in such a case; the resulting
> contents of the register may be arbitrary, but program behaviour as a whole
> remains defined. In other words, in case of JIT backends, the AND must /not/
> be added to the emitted LSH/RSH/ARSH translation."*

The AND is omitted because arbitrary results are *acceptable*, not because the
hardware is *promised* to mask. The "compiler is optimizing the AND away" clause
describes the interpreter's own C, not a hardware guarantee.

Edward Cree, quoted in the upstream commit that added the interpreter's AND
(`28131e9d933339a92f78e7ab6429f4aaaa07061c`, "bpf: Fix up register-based shifts
in interpreter to silence KUBSAN", on-list via the AUTOSEL backports, e.g.
Message-Id `<20210706112203.2062605-87-sashal@kernel.org>`, 2021-07-06):

> *"Shifts by more than insn bitness are legal in the BPF ISA; they are
> implementation-defined behaviour [of the underlying architecture], rather than
> UB, and have been made legal for performance reasons... Guard checks in the
> fast path (i.e. affecting JITted code) will thus not be accepted."*

So the interpreter masks (to silence KUBSAN) and the JITs do not. **scx
schedulers run JIT'd in production**, so the masked value is the one
configuration we do *not* execute.

The ISA specification does state flatly that "Shift operations use a mask of
0x3F (63) for 64-bit operations and 0x1F (31) for 32-bit operations", and that
text passed standardization review unchallenged. A search of the full
`bpf@vger.kernel.org` archive found **no thread reconciling that sentence with
Cree's position or with the `core.c` comment**. Until one exists, treat the
guarantee as contested: there is no single value to conform to, because
"the resulting contents of the register may be arbitrary" is a licence for
divergence rather than a semantics.

**Consequence for the safe subset: out-of-range shifts move from "BPF defines
what C leaves undefined" to "nobody portably defines this." The remedy is
avoidance, not emulation.** Note the verifier already rejects the constant case
outright, so only the variable case is reachable at all.

### 4. Signed integer overflow

| | |
|---|---|
| **Kernel** | DEFINED, and **GUARANTEED BY THE ISA SPECIFICATION**, not merely observed: *"Underflow and overflow are allowed during arithmetic operations, meaning the 64-bit or 32-bit value will wrap."* (BPF ISA, Arithmetic instructions.) This is one of the rows the 2026-08-12 audit **upgraded** — it was previously uncited and could have been mistaken for an x86-JIT observation. |
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
