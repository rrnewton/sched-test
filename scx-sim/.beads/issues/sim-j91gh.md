---
title: 'layer-config: an unknown spec key is Fatal, but upstream loads such a config — make it Refusable'
status: open
priority: 3
issue_type: task
created_at: 2026-09-11T17:26:38.600396262+00:00
updated_at: 2026-09-11T17:26:38.600396262+00:00
---

# Description

safe/layered_config.rs refuses a config carrying a key no layer kind has, and the refusal is Fatal — 'not a refusable scx_layered config field; waivable fields are: idle_resume_us, membw_gb, util_peak_half_life_ms'.

scx_layered sets no #[serde(deny_unknown_fields)] anywhere in the crate, so it LOADS such a config and silently drops the key. So scxsim currently refuses configs production runs.

The doc that introduced the refusal table states the governing principle itself (ai_docs/LAYERED_ARBITRARY_CONFIG_LOADING_20260911.md): 'Mirroring upstream's own ignoring is fidelity; refusing would be the divergence.' It applies that to allow_node_aligned, idle_smt and util_range-on-Open. An unknown key is the same shape.

THE DETECTION IS VALUABLE AND MUST STAY LOUD — it is how tg exercise-layered-state-space found that four live production configs write 'layer_growth' where the field is 'growth_algo', and one writes 'cpu_range' where it is 'cpus_range'. Confirmed with upstream's own --print-and-exit: those layers run the DEFAULT growth algo ('Sticky') and, in one case, no cpus_range at all. Nothing in production reports this.

ASK: move the unknown-key disposition from Fatal to Refusable, so --layer-config-drop <key> waives it and echoes the waiver before the run (the existing Refusable machinery already does the echo). A researcher then gets both halves: told loudly that the key did nothing, AND able to reproduce exactly what production runs.

Do NOT make it a silent accept — that is the failure the refusal table exists to prevent, and it is how the typos survived in production in the first place.

VERIFY: a config with an unknown key is still refused by default with the key named; the same config loads under --layer-config-drop <key> with the waiver echoed on stderr; add a test for both arms.
