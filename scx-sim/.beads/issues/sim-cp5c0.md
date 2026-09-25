---
title: KernelConfig::hz changes HZ for scx_tickless alone; the engine tick, jiffies and scx_layered stay at 250
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.612059807+00:00
updated_at: 2026-09-25T03:43:12.612059807+00:00
---

# Description

scxsim_build::KernelConfig::hz changes HZ for scx_tickless alone, and only when tickless's tick_freq is 0. The engine tick, bpf_jiffies64() and scx_layered stay at 250 Hz. An embedder who follows the doc and passes the kernel under test's HZ gets a model in which parts of one run disagree about HZ.

- hz emits -DSIM_CONFIG_HZ. The only consumer is schedulers/tickless/wrapper.c (unsigned int CONFIG_HZ = SIM_CONFIG_HZ), read by tickless's tick_interval_ns() as the fallback when tick_freq is 0 ('tick_freq ? : CONFIG_HZ'). The bundled tickless manifest sets tick_freq = 250.
- The engine ticks every engine::TICK_INTERVAL_NS = 4 ms. unsafe_impl/kfuncs.rs derives CONFIG_HZ = 1e9 / TICK_INTERVAL_NS = 250 from it, and bpf_jiffies64() and the jiffies conversions divide by that.
- schedulers/layered/wrapper.c hardcodes CONFIG_HZ = 250. Its comment says it must match TICK_INTERVAL_NS, because otherwise layered's antistall delay accounting silently drifts. KernelConfig::hz does not reach it.

KernelConfig's doc does call hz 'the tickless fallback, reachable only when tick_freq is 0'. It then concludes that 'an embedder supplying the kernel-under-test's values is correct even where the consumer is presently shadowed'. For hz that conclusion does not hold. With hz = 1000 and tick_freq = 0, tickless arms its timer every 1 ms, while the engine's tick and jiffies still run at 250 Hz.

This matters for the release because KernelConfig is a public struct in a crate about to be published. Removing or changing the meaning of a public field later is a breaking change.

Fix, one of, before publishing:
- Make hz whole-sim: drive TICK_INTERVAL_NS, the host CONFIG_HZ and layered's CONFIG_HZ from the same value.
- Or remove hz from the public KernelConfig and keep 250 fixed until that exists.

In both cases, correct the doc sentence.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
