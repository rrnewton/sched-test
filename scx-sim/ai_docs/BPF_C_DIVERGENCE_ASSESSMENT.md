# Standing Assessment: can compiling BPF source as userspace C model BPF semantics?

**Standing document — re-assessed per cycle, not closed.**
**Task:** tg `standing-bpf-c-semantic-divergence-assessment`
**Companion:** `BPF_UB_FIDELITY_POLICY.md` — the per-UB-class policy this
assessment tests. NOTE: as of cycle 1 that file is **not yet on
`integration`**; it lives on branch `agent/ubfidelity` (commit `9337fdf`,
unpushed, awaiting review). Every reference to it below is a forward
reference until that branch merges.

| Cycle | Date | Assessor | Verdict |
|---|---|---|---|
| 1 | 2026-08-12 | agent `divergence` | Premise holds, but the risk is a different shape than assumed. Recommend **no** default flag changes. Compiler pass **not** justified yet. |

---

## Cycle 1 — the risk is not where it was thought to be

The concern as filed: BPF *defines* several behaviours that C leaves
*undefined*, so clang may optimise on the assumption they cannot happen, and
the compiled-as-C build diverges from real BPF by optimisation, silently.

That is true as far as it goes. What it misses is that **the production BPF
program is compiled by the same clang, from the same source, with the same UB
license.** Measured:

```
scx/rust/scx_cargo/src/clang_info.rs:164-169
  -g -O2 -Wall -Wno-compare-distinct-pointer-types -D__TARGET_ARCH_<arch> -mcpu=v3
```

No `-fwrapv`. No `-fno-strict-aliasing`. The deployed BPF object is a
UB-exploiting build.

And clang **does** exploit it on the BPF target. Compiling a probe for
`-target bpf -mcpu=v3` and disassembling, the expression `(a / b) == 0` with a
runtime-zero `b` becomes:

```
31:  w3 = 0x1
32:  if r2 > r1 goto +0x1     ; divisor > dividend
33:  w3 = 0x0
```

— the same algebraic rewrite measured on x86-64 in the UB work. The verifier's
`chk_and_div` patchlet still guarantees the *instruction* yields 0, but clang
has already deleted the code that would observe it. The same object also
strength-reduces `a % b` into `a - (a/b)*b`, so `chk_and_mod` never runs at all.

**So for this class, scxsim and production are wrong in the same direction and
therefore agree with each other**, while both diverge from the ISA guarantee.
The scxsim-vs-production gap is smaller than the filing assumed.

### The restated risk

> The danger is not that C has UB where BPF does not — both builds inherit the
> same UB license from the same compiler. The danger is that **the x86-64
> backend and the BPF backend make different choices given that same license.**

`match_substr` is the canonical case and it is already measured
(`BPF_UB_FIDELITY_POLICY.md`): the BPF backend allocated the uninitialised `y`
to `r9`, which held a live map pointer; the x86-64 backend left it as stack
residue that changed with coverage flags. Same source, same UB, different
garbage, different behaviour.

This restatement is *more* actionable than the original: it says the thing to
watch is per-target codegen divergence, which is detectable by differential
testing, rather than a language-semantics gap, which is not.

---

## Status by divergence class

| Class | BPF | C | Measured this cycle | Status | Plausible cost if wrong |
|---|---|---|---|---|---|
| Uninitialised read | stack allowed (privileged), register rejected | UB | prior cycle, load-tested | **OPEN — detector only** | High. Already bit us: coverage-only `match_substr` failure, cost a full investigation. |
| div/mod by variable zero | defined by patchlet | UB | yes — clang exploits it on **both** targets | **OPEN, but symmetric** | Low for sim-vs-prod fidelity (both diverge alike); high if anyone reasons from the ISA guarantee. |
| Signed overflow | wraps | UB | yes — clang folds `x+1 > x` to true at `-O2` | **OPEN by choice** (see below) | Low. Production has the same exposure. |
| Variable shift ≥ width | masked to width | UB | yes — **no divergence found** at `-O0/-O2/-O3`, including a guard-deletion probe | **LATENT, not observed** | Unknown, currently unobserved. |
| Constant zero divisor / over-width shift | verifier **rejects** | UB | prior cycle | **OPEN — no load-time gate** | Medium. scxsim runs programs production would refuse to load. Tracked `sim-ixinx`. |
| Signed div/mod (`sdiv`/`smod`) | v4 only | UB | yes — clang **refuses** for `-mcpu=v3` | **NOT REACHABLE** today | None until scx moves to v4. |

---

## Recommendation: do not adopt the "cheap mitigations" by default

`-fwrapv` and `-fno-strict-aliasing` make scxsim match the BPF **ISA
specification**. scxsim's job under Twin Design Principle 1 is to match
**production**, and production is the compiled artifact, which uses neither
flag. Turning them on would move scxsim *away* from the binary that actually
runs.

This is not a theoretical objection — both flags measurably change scheduler
codegen. Rebuilding all five schedulers and comparing sha256 against baseline:

| Flag | cosmos | lavd | mitosis | simple | tickless |
|---|---|---|---|---|---|
| `-fwrapv` | CHANGED | CHANGED | unchanged | unchanged | CHANGED |
| `-fno-strict-aliasing` | unchanged | CHANGED | CHANGED | unchanged | unchanged |

Four of five schedulers change under one or both. These flags reach real
scheduler code.

**Adopt instead:** keep them as *diagnostic* modes alongside the existing
`SCX_SIM_UB_PROBE=1`, used to answer "does this scheduler's behaviour depend on
a UB-based optimisation?" A scheduler whose simulated behaviour changes under
`-fwrapv` is one where the deployed binary is relying on signed-overflow UB —
that is a finding to report upstream, not a bug to paper over locally.

---

## The compiler-pass option: not justified yet, and not scxsim-local

The proposal is a pass that normalises the remaining cases to BPF semantics —
masking shifts, defining div/mod by zero.

**The blocking objection is direction, not cost.** A normalising pass applied
only to the scxsim build would make scxsim obey the ISA while production keeps
obeying clang's UB-licensed rewrites. That is a *new* divergence, introduced by
the fix, in the direction that matters most (sim disagrees with the deployed
scheduler). To be correct the pass would have to apply to the production BPF
build too — which is upstream `sched-ext/scx`'s build, not ours. It is
therefore an upstream proposal, not a local mitigation.

Cost if pursued anyway, honestly estimated:

- An LLVM IR pass pinned to a specific LLVM major version. This machine is on
  clang 22; the repo does not pin a toolchain, and the LLVM pass API churns
  across majors. Expect breakage on every toolchain bump.
- Must be applied identically to both build paths to avoid the divergence
  above, so it needs upstream buy-in.
- Per the No-Stub Rule, every normalisation must reproduce what the kernel
  *actually does*, verified by loading and running real programs — not by what
  the ISA document claims. That verification is currently blocked (below).

**Verdict for cycle 1: do not build it.** Revisit if differential testing shows
per-target codegen divergence that flags cannot detect and that costs real
investigation time.

---

## What is not verified, and what would fix it

**BPF programs cannot be executed on this host.** `BPF_PROG_TEST_RUN` via
`bpftool prog run` returns `duration: 0ns` with result globals unchanged, for
both `SEC("syscall")` and `SEC("xdp")` programs, on
`6.13.2-0_fbk15_hardened`. The hardened kernel appears to block it. Program
*loading* works — that is how the verifier accept/reject results in
`BPF_UB_FIDELITY_POLICY.md` were obtained.

Consequently the div/mod finding above rests on **disassembly of the BPF object
plus kernel-source reading**, not on observed runtime values. That is weaker
evidence than the load-based results, and it is flagged as such rather than
rounded up.

**Top item for cycle 2:** run the probes under `vng` / virtme-ng, where
`BPF_PROG_TEST_RUN` should be available, and replace the inferred runtime
semantics with measured ones.

---

## Re-assessment triggers

Re-run this assessment when any of these fire:

1. A new UB-class divergence is found.
2. A scheduler behaves differently under coverage vs non-coverage builds — that
   is how `match_substr` surfaced, and it is the signature of per-target or
   per-config codegen divergence.
3. Before claiming any scheduler is faithfully supported.
4. **New:** scx changes its BPF cflags in `clang_info.rs`, or moves off
   `-mcpu=v3`. The whole "both builds share the same UB license" argument
   depends on those flags, and `v4` makes signed div/mod reachable.
5. **New:** the toolchain major version changes. Every "clang does / does not
   exploit this" result here is a measurement against clang 22, not a
   guarantee.

## Cycle 2 backlog

- Execute the probes under `vng` and replace inferred runtime semantics with
  measured ones.
- Differential test: run the same workload under scxsim and against the real
  BPF object, and diff the BPF call stream (the existing bpftrace
  structops/helpers harness is the vehicle). This targets the restated risk —
  per-target codegen divergence — directly, which no compiler flag does.
- Run the suite under `SCX_SIM_UB_PROBE=1` and enumerate which UB sites are
  actually reachable (`sim-kykh4`).
- Land the real-verifier CI gate (`sim-ixinx`); it closes the constant-divisor
  and over-width-shift rows, which are load-time rejections scxsim has no
  equivalent of.
