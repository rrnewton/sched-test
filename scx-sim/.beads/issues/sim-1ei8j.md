---
title: bug1_canonical_subprocess_reproduces_throttle fails ONLY on CI — garbage pointer values in cgroup_bw period/burst
status: open
priority: 1
issue_type: bug
created_at: 2026-08-12T23:11:24.227350044+00:00
updated_at: 2026-08-12T23:11:24.227350044+00:00
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
