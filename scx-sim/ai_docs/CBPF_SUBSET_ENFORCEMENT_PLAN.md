# C/BPF Safe-Subset Enforcement Plan

**Status:** measured design; implementation not yet wired into CI

**Date:** 2026-08-12

**Tracking:** `epic-clang-c-bpf-compatibility`, tg
`enforce-cbpf-safe-subset-mechanism`

## Verdict

A semantics-transforming compiler pass is **not required** to enforce a
conservative C/BPF common subset. The measured minimum is:

1. in-source ABI assertions compiled for both targets;
2. selected Clang warnings promoted to errors;
3. three existing `clang-tidy` checks;
4. a structural AST/preprocessor gate with deliberately conservative bans;
5. the existing UB probe; and
6. compilation plus loading of the real BPF object, followed by differential
   tests at helper and map boundaries.

This is stronger than “turn on UBSan.” UBSan does not diagnose
implementation-defined or unspecified behavior. It remained silent for a
real, verifier-loaded example where the same UB-free expression evaluated to
1792 as native x86-64 C and 896 as BPF because `long double` has different
size, alignment, and containing-structure layout on the two targets.

No existing tool covers the full policy. In particular, this machine did not
have `clang-query`; a prototype using Clang's JSON AST plus `jq` demonstrated
that the structural rules are observable without a compiler plugin, but that
prototype is not yet a production gate and is not counted as one.

## Related work

The related-work survey found **no project that reconciles BPF and native-C
semantics at the C source level**. This is not solved territory. The
enforcement ladder in this document is the plan of record, not a fallback
behind an existing package. Future readers should not repeat the same search
unless materially new work has appeared.

The distinction below is load-bearing: executing compiled BPF bytecode in
userspace is a different problem from compiling BPF C source as native C.

### Bucket A: compiles BPF C source as native C — our problem

- [`lib/scxtest`](https://github.com/sched-ext/scx/tree/main/lib/scxtest) is
  the only direct precedent found. It compiles real scheduler BPF C as native
  C and supplies replacements for CO-RE builtins and BPF substrate APIs.
  It does not reconcile arithmetic, object-representation, padding, or other
  C/BPF semantic differences. scxsim already builds on it; this plan adds the
  missing semantic layer rather than replacing it.

That is the complete bucket. No Clang flag, sanitizer, plugin, BPF unit-test
framework, or published MISRA-like subset was found that addresses this
source-to-native-C equivalence problem.

### Normative target — neither execution bucket

- The [IETF BPF ISA
  draft](https://www.ietf.org/archive/id/draft-ietf-bpf-isa-04.html) and
  [Linux BPF instruction-set
  documentation](https://docs.kernel.org/bpf/standardization/instruction-set.html)
  define the target behavior: wrapping arithmetic, masked shifts,
  division/modulo by zero, and the `INT_MIN / -1` cases. They specify BPF
  instructions, not how C source must be constrained to produce equivalent
  native code.

### Adoptable artifact, but still bytecode-level

- [`bpf_conformance`](https://github.com/Alan-Jowett/bpf_conformance) runs BPF
  bytecode and compares `r0` with an expected value. Its roughly 313 cases
  include over-width and negative shift counts, division by zero, and signed
  edge cases. The harness solves Bucket B below, but its test corpus is an
  adoptable external oracle: transcribe the arithmetic cases into C-level
  probes compiled through both scxsim and BPF pipelines.

### Bucket B: executes or translates BPF bytecode — different problem

- [uBPF](https://github.com/iovisor/ubpf),
  [rbpf](https://github.com/qmonnet/rbpf),
  [eBPF for Windows](https://github.com/microsoft/ebpf-for-windows),
  [llvmbpf/bpftime](https://github.com/eunomia-bpf/llvmbpf), and
  [PREVAIL](https://github.com/vbpf/ebpf-verifier) consume compiled BPF
  instructions. They can implement masked shifts and guarded division one
  opcode at a time because they control that lowering. Published llvmbpf
  [conformance
  results](https://eunomia-bpf.github.io/llvmbpf/bpf_conformance_results.txt)
  report the bytecode corpus passing, but this does not constrain Clang's
  native-C optimization of the original source.
- [CertrBPF](http://www.irisa.fr/prive/talpin/papers/cav22.pdf) formally proves
  an interpreter against the BPF ISA. It is valuable evidence that the ISA
  target is formalizable, not a C-source equivalence solution.
- Trail of Bits' [userspace verifier
  harness](https://blog.trailofbits.com/2023/01/19/ebpf-verifier-harness/)
  exercises verifier acceptance, not runtime value equivalence.

The bytecode projects are useful reference implementations and test oracles.
None can be adopted as the source-level enforcement layer.

## The safe subset to enforce

The common subset is not merely “C without undefined behavior.” It is C that
also avoids dependence on implementation-defined and unspecified choices,
pins its shared ABI, passes the real verifier, and uses explicitly modeled
BPF runtime contracts.

### S1. Target and ABI contract

- BPF is `bpfel`; the host is little-endian LP64; `CHAR_BIT == 8`.
- Shared map, context, and helper-boundary structures assert exact
  `sizeof`, `_Alignof`, and `offsetof` values under both targets.
- Persisted and shared integer fields use explicit-width types.
- Plain `char`, `long`, enum representation, bitfield layout, and target
  preprocessor macros do not determine shared values.

### S2. Integer semantic domain

- Signed addition, subtraction, multiplication, and left shift cannot
  overflow.
- Shift counts are less than the promoted operand width. Signed left-shift
  operands and results stay in C's defined range.
- Divisors are nonzero; signed division excludes `INT_MIN / -1` and the
  corresponding wider cases.
- Narrowing and signedness-changing conversions are range-checked.
- Negative signed right shift is prohibited; use an explicit unsigned
  operation or a reviewed sign-extension helper.
- Unsigned modulo arithmetic is allowed.
- Expressions do not rely on unspecified operand or argument evaluation
  order.

### S3. Types and structural constructs

- No `float`, `double`, `long double`, `_Complex`, or `_BitInt`, including
  layout-only and `sizeof` uses.
- No variable-length arrays or variadic functions.
- No union punning.
- No bitfields in shared ABI structures.
- No target-semantic `#ifdef __BPF__` / `SCX_BPF_UNITTEST` branches in shared
  decision logic. Required substrate adaptations stay in reviewed boundary
  files.

### S4. Objects and representation

- Every read observes an initialized object value.
- Padding bytes are never observed accidentally. A shared key/value is either
  field-serialized or every byte is canonicalized before use.
- `memcmp`, hashing, byte iteration, `memcpy`, and `memmove` over a record are
  forbidden by default and allowed only for an annotated, layout-pinned,
  byte-canonical type.
- Raw bitfield, union, pointer, and host-endian object representations are not
  protocols.

### S5. Pointer and alignment contract

- Pointers are opaque capabilities. Only null/equality tests and
  bounds-proven arithmetic within the represented object are portable.
- Pointer-to-integer values, address hashing/ordering, and numeric pointer
  handles are prohibited unless a boundary shim maps them to stable IDs.
- Typed accesses are naturally aligned. Packed or byte-oriented access must
  use a reviewed byte-copy helper supported by both targets.

### S6. BPF loadability and runtime contract

- The actual BPF object passes the real verifier for its program type,
  helpers, kfuncs, pointer regions, bounded loops, call depth, stack, and
  complexity limits.
- Maps, helpers, per-CPU state, atomics/concurrency, CO-RE, time, random
  values, CPU identity, and context fields have explicit simulator contracts
  and differential tests. They are not guaranteed by C language conformance.

## Enforcement ladder

The ladder is ordered by cost. A later layer does not excuse a missing earlier
one.

### L1. In-source assertions

Compile ABI assertions under both native and BPF targets:

```c
_Static_assert(CHAR_BIT == 8, "C/BPF requires 8-bit bytes");
_Static_assert(__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__, "bpfel only");
_Static_assert(sizeof(struct shared_key) == EXPECTED_KEY_SIZE, "ABI size");
_Static_assert(_Alignof(struct shared_key) == EXPECTED_KEY_ALIGN, "ABI align");
_Static_assert(offsetof(struct shared_key, field) == EXPECTED_FIELD_OFF,
               "ABI field offset");
```

This is complete for facts named in assertions and useless for unnamed
structures or operations. Generate/assert every annotated shared type; do not
maintain a hand-picked list.

### L2. Clang warnings promoted individually to errors

The measured useful set is:

```text
-Werror=conditional-uninitialized
-Werror=conversion
-Werror=sign-conversion
-Werror=vla
-Werror=double-promotion
-Werror=padded
-Werror=bitfield-constant-conversion
-Werror=address-of-packed-member
```

Promotion must name the warning. A diagnostic printed while the command exits
zero is not enforcement. `-Wpadded` is a deliberately conservative default;
annotated and fully canonical shared types may need a narrow allowlist.

`-Wshift-sign-overflow`, `-Wunsequenced`, `-Wpointer-to-int-cast`, and
`-Wvarargs` did **not** catch the corresponding deliberate probes described
below. `-Wdouble-promotion` catches a conversion, not the presence of a
floating type. `-Wbitfield-constant-conversion` catches a bad assignment, not
a bitfield declaration.

### L3. Existing clang-tidy checks

Only three tested checks contributed coverage for C:

```text
bugprone-narrowing-conversions
hicpp-signed-bitwise
bugprone-suspicious-memory-comparison
```

Run with `-warnings-as-errors='*'`; otherwise the same false-green failure mode
remains. `hicpp-signed-bitwise` is intentionally broader than the signed
right-shift rule and will require unsigned rewrite or a reviewed suppression.

The tested C++-named union/varargs checks, `google-runtime-float`, and the
other candidate checks did not diagnose their C probes. They are not coverage.

### L4. Structural AST and preprocessor gate

The production gate should mark shared ABI records and reviewed boundary
files explicitly, then reject structural violations. Suggested annotations:

```c
#define CBPF_SHARED_ABI __attribute__((annotate("cbpf_shared_abi")))
#define CBPF_BOUNDARY   __attribute__((annotate("cbpf_boundary")))
```

Matchers or equivalent AST predicates must reject:

- floating, complex, `_BitInt`, VLA, and variadic types;
- bitfield fields in `CBPF_SHARED_ABI` records;
- union member punning;
- pointer-to-integer casts outside `CBPF_BOUNDARY`;
- signed `>>`;
- calls with multiple unsequenced, potentially side-effecting argument
  expressions;
- record-to-byte pointer casts;
- `memcmp`, raw hash, `memcpy`, or `memmove` of records unless the record is an
  annotated canonical-byte type.

A preprocessor scan separately rejects target-semantic conditionals in shared
logic. Inactive `#ifdef` branches are absent from the AST, so an AST-only gate
cannot cover this rule.

`clang-query` was not installed during measurement. Clang 22 JSON AST plus
`jq` demonstrated every syntax shape above except the semantic fact “this
padding was canonicalized on all paths.” Treat the JSON approach as a
prototype until it is packaged as a fail-closed script with canary tests.

### L5. Runtime UB, BPF compile/load, and differential gates

- Keep `-Wconditional-uninitialized` and the trapping UBSan checks for
  signed overflow, shift exponent, and integer division by zero.
- Compile the actual BPF target. A 600-byte-address-taken stack probe was
  rejected by Clang's BPF backend with “BPF stack limit is exceeded.” This is
  the backend enforcing the BPF architecture's 512-byte stack limit during
  compilation; the kernel verifier was not invoked for this probe. A real
  verifier load remains a separate required gate for constraints that survive
  compilation.
- Load the actual objects or run `veristat`; native compilation cannot prove
  verifier acceptance.
- Differentially test helper/map contracts and import the arithmetic portion
  of `bpf_conformance` as an external semantic oracle.

These are required even if every source-level gate is green.

### L6. Custom Clang plugin or compiler pass — last resort

Do not build a semantics-transforming LLVM pass for the conservative subset.
If the structural script becomes too brittle, a custom clang-tidy check or
Clang AST plugin may be justified as a more maintainable implementation of L4,
but that is a checker, not a transformation.

Interprocedural proof that arbitrary code never observes noncanonical padding
is the only measured rule that simple matchers cannot establish precisely.
The preferred solution is to make the policy syntactic: raw representation is
forbidden except through one annotated canonicalization API. A dataflow-aware
plugin becomes necessary only if the project refuses that conservative API
boundary and wants arbitrary raw-byte code accepted when analysis can prove it
safe.

## Measurement method

Measurements used Clang 22.1.3 and clang-tidy 23.2.0. Every row below had a
small C translation unit containing the stated violation. A catch counts only
when the command exits nonzero or an AST query demonstrably selects the
violating node. Merely printing a warning is not a catch.

The deliberately wrong ABI translation unit exited 1 with four static
assertion errors under both `x86_64-unknown-linux-gnu` and `bpfel`. The three
trapping UBSan probes each built successfully and exited 132 at runtime. In
shell exit-status convention, 132 is 128 + signal 4 (`SIGILL`): trapping UBSan
lowered the violation to an illegal-instruction trap, so these probes failed
closed rather than merely printing a diagnostic and returning success. The
address-taken 600-byte BPF stack probe failed BPF compilation with the explicit
“BPF stack limit is exceeded” diagnostic. That is compile-time enforcement by
Clang's BPF backend of the same 512-byte architectural stack limit the kernel
verifier enforces; it is not evidence that this probe reached the verifier.

Candidate warning command:

```bash
clang -std=c11 -fsyntax-only -Werror \
  -Wall -Wextra -Wconversion -Wsign-conversion -Wshift-sign-overflow \
  -Wshift-overflow -Wshift-negative-value -Wshift-count-overflow \
  -Wvla -Wdouble-promotion -Wpadded -Wbitfield-constant-conversion \
  -Wunsequenced -Wpointer-to-int-cast -Wcast-align \
  -Waddress-of-packed-member -Wvarargs -Wpedantic probe.c
```

Candidate tidy command:

```bash
clang-tidy \
  -checks='-*,bugprone-narrowing-conversions,bugprone-suspicious-memory-comparison,hicpp-signed-bitwise,cppcoreguidelines-pro-type-union-access,cppcoreguidelines-pro-type-vararg,hicpp-vararg,google-runtime-float' \
  -warnings-as-errors='*' probe.c -- -std=c11
```

AST prototype command shape:

```bash
clang -std=c11 -Xclang -ast-dump=json -fsyntax-only probe.c |
  jq -e '<predicate selecting a forbidden node>'
```

The AST prototype is shown as `AST` below, not as `clang-query`, because the
latter was unavailable and therefore unmeasured.

### Deliberate violation probes

| Probe | Essential violating source |
|---|---|
| ABI contract | `_Static_assert(CHAR_BIT == 7, "wrong");` plus deliberately wrong `sizeof`, `_Alignof`, and `offsetof` |
| narrowing/sign | `int32_t f(uint64_t x) { return x; }` and `uint32_t g(int32_t x) { return x; }` |
| uninitialized | outer loop reads `inner` before the inner loop first assigns it |
| signed overflow | volatile `INT_MAX + 1` |
| shift exponent | volatile `1U << 32` |
| division by zero | volatile `7 / 0` |
| signed right shift | `int f(int x) { return x >> 1; }` |
| unspecified order | `consume(first(), second())` with both callees potentially effectful |
| VLA | `int values[n];` |
| floating | shared `long double` field and `double f(double)`; separate float-to-double promotion |
| extended types | `_BitInt(65)` and `double _Complex` |
| union punning | initialize a union's `float`, read its `uint32_t` member |
| bitfield ABI | shared record with `unsigned mode : 3`; separate constant `mode = 15` |
| target conditional | `#ifdef __BPF__` returns 1, native branch returns 2 |
| pointer representation | explicit `(uintptr_t)pointer` |
| packed alignment | return the address of a packed `int` member |
| padded layout | `struct { char tag; long value; }` |
| record comparison | `memcmp(left, right, sizeof(*left))` for a padded key |
| raw record hash | cast record pointer to `unsigned char *` and hash `sizeof(record)` bytes |
| raw record copy | `memcpy(destination, source, sizeof(*source))` |
| verifier stack | address-taken `volatile unsigned char bytes[600]` passed to a BPF helper |

### Candidate mechanism matrix

Legend: **Y** = demonstrated catch; **N** = demonstrated no diagnostic;
**P** = partial/proxy only; **—** = mechanism is not applicable. `AST` means
the JSON-AST prototype, not an installed production gate.

No custom plugin was available or built. Its demonstrated result is therefore
**not measured for every row**, and it contributes zero claimed coverage. It
is omitted from the matrix rather than filling a column with hypothetical
“yes” results.

| Subset rule / probe | `_Static_assert` | Clang `-Werror` | clang-tidy | AST / preprocessor | Runtime / BPF gate |
|---|---:|---:|---:|---:|---:|
| ABI width/endian/size/align/offset | **Y** | P (`-Wpadded`) | N | P | — |
| narrowing and sign-changing conversion | — | **Y** | **Y** for narrowing; P for sign | **Y** structurally | — |
| uninitialized read | — | **Y** (`conditional-uninitialized`) | N | P | P (`pattern`) |
| signed overflow | — | N for variable input | N | P | **Y**, trap exit 132 |
| over-width shift | — | N for variable input | N | P | **Y**, trap exit 132 |
| division by variable zero | — | N | N | P | **Y**, trap exit 132 |
| negative signed right shift | — | **N** | **Y** (`hicpp-signed-bitwise`) | **Y** | — |
| unspecified function-argument order | — | **N** | **N** | **Y**, conservatively selected two nested calls | — |
| VLA | — | **Y** | **N** | **Y** | BPF compile may also reject |
| any floating type | P for named ABI layout | **N**; P for promotion only | **N** | **Y** | BPF arithmetic compile rejects |
| `_BitInt` / complex | — | P under C11 pedantic only | **N** | **Y** | BPF support differs |
| union punning | — | **N** | **N** for C | **Y**, union declaration and member-access nodes | — |
| bitfield in shared ABI | P for resulting layout | **N**; P for bad constant only | **N** | **Y** | — |
| target-semantic preprocessor branch | — | **N** | **N** | AST N; preprocessor scan **Y** | differential test |
| pointer-to-integer representation | — | **N** for explicit same-width cast | **N** | **Y** (`PointerToIntegral`) | differential boundary test |
| packed/unaligned member address | — | **Y** | N in tested set | **Y** | verifier/load gate |
| padded record exists | P for exact layout | **Y** (`-Wpadded`) | N | **Y** | — |
| `memcmp` of padded record | — | P (record padding, not call) | **Y** | **Y** | differential map test |
| raw-byte record hash | — | P (record padding only) | **N** | **Y** for record-to-byte cast | differential map test |
| raw record `memcpy` | — | P (record padding only) | **N** | **Y** for call/type pattern | differential map test |
| BPF stack/resource/verifier constraints | — | N in native build | N | P (size only) | **Y**, BPF compile/load |
| helper/map/CO-RE/concurrency contracts | — | N | N | N | **Y** only with dedicated differential tests |

### Selected enforcement and residuals

| Subset rule | Enforcing mechanism | Demonstrated catch? | Residual |
|---|---|---:|---|
| Shared ABI facts | generated `_Static_assert`s under both targets | **Yes** | Missing annotation/assertion remains invisible; generate the inventory. |
| Narrowing/sign conversion | Clang conversion warnings + tidy narrowing check | **Yes** | Explicit casts need AST policy and reviewed range proof. |
| C UB arithmetic | trapping UBSan plus source guards | **Yes** | Dynamic coverage is incomplete; source guards remain mandatory. |
| Uninitialized reads | `-Wconditional-uninitialized` canary gate | **Yes** | Known false positives require review; warning must be fail-closed after an allowlist. |
| Signed right shift | `hicpp-signed-bitwise` or signed-`>>` AST ban | **Yes** | Broad tidy check also bans other signed bitwise operations. |
| Unspecified evaluation order | conservative AST ban on multiple effectful call arguments | **Yes**, syntax probe | Purity is not inferred. Use an allowlist or split expressions into statements. |
| Forbidden types, VLA, varargs | Clang warning where available plus AST type bans | **Yes**, AST prototype | Production AST script still must be packaged and canaried. |
| Union punning and ABI bitfields | AST structural bans | **Yes**, AST prototype | Distinguishing harmless unions requires annotations; conservative ban avoids analysis. |
| Target conditionals | preprocessor-source scan restricted to shared logic | **Yes** | Reviewed substrate boundary files need an explicit allowlist. |
| Pointer representation | pointer-to-integral AST ban | **Yes**, AST prototype | Stable-ID boundary shims need differential tests. |
| Alignment | packed-member warnings + AST + verifier gate | **Yes** | Arbitrary byte helpers require review. |
| Padding and raw representation | `-Wpadded`; tidy record-`memcmp`; AST ban on raw record bytes | **Partial** | Arbitrary interprocedural byte flow cannot be proven by simple matchers. Require one canonicalization API; otherwise build a dataflow-aware checker. |
| Verifier/resource rules | BPF compile and real load/veristat | **Yes** | Requires privileged CI or an equivalent verifier environment. |
| Runtime substrate contracts | dedicated BPF/native differential tests | **Per-contract** | No generic language tool can establish environment equivalence. |

## Fail-closed rollout plan

1. Commit all violation probes as canaries. Each gate must be tested by
   temporarily removing its flag/check and observing its canary turn green.
2. Add ABI annotations and generate assertions for every shared map/context
   record. Compile them for both native and `bpfel` targets.
3. Add warning flags one at a time with explicit, reviewed suppressions. Never
   use a blanket warning disable around vendored scheduler source.
4. Provision and pin clang-tidy in CI; do not inherit a machine's floating
   check set. Use `-warnings-as-errors='*'`.
5. Package the AST/preprocessor prototype as a versioned, fail-closed tool.
   Its own test suite contains every probe above plus one clean control per
   rule. Missing Clang/JQ/tool execution is a hard failure, not a skip.
6. Compile and load the real BPF objects in CI. Keep host UBSan as a separate
   diagnostic dimension.
7. Import the relevant `bpf_conformance` arithmetic oracle cases and add
   differential tests for each simulator helper/map contract.
8. Publish a generated coverage report from the canary matrix. A configured
   mechanism without a red canary is reported as **not enforced**.

## Plugin decision

**No Clang plugin or LLVM compiler pass is required for the conservative
subset described here.** The hard cases can be made enforceable by choosing
strict syntactic rules:

- signed right shift is banned by an existing tidy check or AST predicate;
- unspecified argument order is removed by splitting potentially effectful
  expressions;
- raw record representation is confined to one annotated canonicalization
  API; and
- target conditionals are restricted to reviewed boundary files.

A custom dataflow-aware Clang check is warranted only if maintainers want to
accept arbitrary raw-byte and interprocedural code whenever analysis can prove
padding canonicalization. That would reduce false positives; it is not needed
for correctness. A semantics-transforming pass is a different project needed
only if scxsim must accept unmodified source that deliberately relies on
BPF-defined/C-undefined operations instead of rejecting that source.
