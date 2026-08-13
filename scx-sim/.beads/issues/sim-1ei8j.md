---
title: bug1_canonical_subprocess_reproduces_throttle fails ONLY on CI — garbage pointer values in cgroup_bw period/burst
status: open
priority: 1
issue_type: bug
created_at: 2026-08-12T23:11:24.227350044+00:00
updated_at: 2026-08-12T23:38:15.009156036+00:00
---

# Description

Same commit, deterministic simulation, opposite results by environment.

  local (316-core dev box): Fingerprint { exit_code: 0, is_throttled: 0, nr_throttled_periods: "5/6", nr_throttled_tasks: 0 }  -> PASSES
  CI (ubuntu-latest runner): Fingerprint { exit_code: 0, is_throttled: 1, nr_throttled_periods: "5/6", nr_throttled_tasks: 4 }  -> FAILS

The assertion is bug1_canonical_repro.rs:301, 'post-V4C: cgroup should UNTHROTTLE when no real work is pending. is_throttled==1 indicates the V1-V3 stall pattern has regressed (engine over-charge has returned).'

SMOKING GUN in the CI stderr — the LAVD-PRINTK cgroup dump contains stack-address-shaped garbage where quota/period/burst values belong:

  [LAVD-PRINTK]  \_ quota: 10000000/100000000/0, period: 140720394283280, burst: 140230518010496

140720394283280 = 0x7FFC_xxxx_xxxx and 140230518010496 = 0x7F8A_xxxx_xxxx. Those are stack addresses, not durations. quota reads sanely (10000000/100000000/0) while period and burst next to it do not, so this looks like an uninitialised or mis-typed read of adjacent state rather than a wholesale corruption.

WHY IT IS ENVIRONMENT-DEPENDENT: the test invokes scxsim with --no-disable-aslr, so if the value being read is derived from an address it will differ between hosts and between runs. That is the same class as the upstream scx_layered match_substr() uninitialised-'y' bug (mb sim-hyr11): source-level UB whose observable behaviour is decided by codegen/addresses, invisible on one machine and fatal on another.

HOW IT SURFACED: the test was #[ignore]d with a stale reason ('upstream cgroup_bw rewrite changed end-of-run throttle state'). It was un-ignored in PR #70 after integration 37f7f8a fixed the throttle-blind watchdog and it passed 20/20 locally. It had never run on CI, because CI could not reach the test step at all — the coverage gate died in ld with SIGBUS before any test executed. Fixing that disk exhaustion (PR #81) is what first let CI run this test, and it failed immediately.

So two prior defects were stacking to hide this one: the stale #[ignore], then the disk exhaustion.

NEXT STEPS:
1. Find the read behind period/burst. Prime suspects: an uninitialised local or a struct-field/type mismatch in the LAVD-PRINTK cgroup dump path, or in the cgroup_bw state it reads.
2. Determine whether is_throttled==1 is CAUSED by that garbage (garbage period -> bogus refill arithmetic -> stays throttled) or merely printed alongside it. If caused, this is a real cgroup_bw fidelity bug, not just a reporting bug.
3. Re-run under --disable-aslr on CI to confirm the address dependence.
4. The test is #[ignore]d again meanwhile, with this issue id in the reason, so CI is not held red by it.

# Acceptance Criteria

- The test passes on CI as well as locally, with ASLR enabled.
- period/burst in the LAVD-PRINTK dump hold plausible durations on both hosts.
- The #[ignore] added to unblock CI is removed.

# Notes

CORRECTION TO MY OWN HYPOTHESIS — the 'garbage pointer values' are NOT memory corruption. They are an upstream printf arity bug, and it is diagnostic-only.

scx/lib/cgroup_bw.bpf.c:2907 (and upstream main:2884, so it is not something our fork introduced):

    bpf_printk("%s   \\_ quota: %llu/%llu/%llu, period: %llu, burst: %llu", indent_str,
                    cgx->quota, cgx->period, cgx->burst);

The format string has SIX conversions — one %s and FIVE %llu — but only FOUR arguments are supplied:
indent_str plus three values. So the printed 'quota: 10000000/100000000/0' triple actually consumed
quota, period and burst, and the trailing 'period:' and 'burst:' conversions then read past the
supplied arguments and print whatever is in the next slots. On CI that was 0x7FFC.. and 0x7F8A..,
which look like stack addresses because that is exactly what they are.

So: no corrupt cgx, no wild pointer. A cosmetic upstream bug in ONE dump line. I filed this issue
partly on the strength of that garbage and I was wrong to read it as corruption.

WHAT IT DOES NOT EXPLAIN, and this is the part that still matters: is_throttled is printed by a
SEPARATE, correctly-formed bpf_printk two lines down (7 conversions, 7 arguments). That value is a
genuine read of cgx->is_throttled. So the local-vs-CI divergence in is_throttled is real and is NOT a
reporting artefact.

WHAT I RULED OUT LOCALLY, by experiment rather than reasoning:
  host CPU count   316 / 4 / 2 (taskset)      -> is_throttled 0 in all three
  ASLR             on (default) and off (setarch -R), 5 runs each -> 0 in all ten
  repetition       20 consecutive runs earlier -> 0 every time
Locally it is rock solid 0. On CI it is 1. So it is neither flakiness, nor host parallelism, nor
address layout.

REMAINING CANDIDATE: toolchain codegen. CI runs rustc 1.97.1 with Ubuntu's clang; this box runs rustc
1.96.0 with clang 18.1.8, and the scheduler .so is clang-built while the engine is rustc-built. That is
the same shape as the upstream match_substr uninitialised-'y' bug (mb sim-hyr11): behaviour decided by
codegen, stable on one machine and different on another. It is a strong hypothesis, NOT a confirmed
cause — I cannot reproduce CI's toolchain on this box, so I am not claiming it.

NEXT STEP FOR WHOEVER TAKES THIS: build the scheduler .so with a different clang (or the engine with
rustc 1.97) and see whether is_throttled flips. That is the cheapest discriminator and it needs no CI
round trip.
