---
title: 'Upstream cgroup_bw dump: printf arity bug prints stack garbage for period/burst'
status: open
priority: 3
issue_type: bug
created_at: 2026-08-12T23:38:45.315802083+00:00
updated_at: 2026-08-12T23:38:45.315802083+00:00
---

# Description

scx/lib/cgroup_bw.bpf.c:2907, and present at upstream main (lib/cgroup_bw.bpf.c:2884), so not something our fork introduced:

    bpf_printk("%s   \\_ quota: %llu/%llu/%llu, period: %llu, burst: %llu", indent_str,
                    cgx->quota, cgx->period, cgx->burst);

SIX conversions (one %s, five %llu) against FOUR arguments (indent_str plus three values). The printed 'quota: A/B/C' triple actually consumes quota, period and burst; the trailing 'period:' and 'burst:' conversions then read past the supplied arguments.

Observed on GitHub CI:
    quota: 10000000/100000000/0, period: 140720394283280, burst: 140230518010496
0x7FFC.. and 0x7F8A.. — stack addresses, because that is what is in the next argument slots.

IMPACT: diagnostic output only. It is one bpf_printk in cbw_dump_cgroup_tree; the is_throttled line two lines below is separately well-formed (seven conversions, seven arguments) and its values are trustworthy. Nothing reads these printed numbers programmatically.

WHY IT MATTERS ANYWAY: it cost real investigation time. The garbage was initially read as memory corruption of cgx and used as evidence in mb sim-1ei8j, which sent that issue down a wrong path until the format string was counted.

FIX: one line — either supply the two missing arguments or drop the two surplus conversions. The intended output was probably 'quota: quota/period/burst' as a triple, in which case the correct format is:

    bpf_printk("%s   \\_ quota: %llu/%llu/%llu", indent_str, cgx->quota, cgx->period, cgx->burst);

This is upstream's to fix; it can ride the same path as the match_substr fix (mb sim-hyr11), which is already prepared on a branch off sched-ext/scx main.

# Acceptance Criteria

- The dump line prints only values that were actually passed.
- Upstream PR opened, or the fix carried locally with a note if upstream declines.
