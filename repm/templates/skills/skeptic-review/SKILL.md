---
name: skeptic-review
description: Rigorous skeptic review of experimental results — completeness, fishy numbers, hypothesis testing, methodology
---

# Skeptic Review

Every experimental result MUST pass skeptic review before being
reported, committed, or used to drive decisions.

## When to invoke

- After `repm run` completes all modes
- Before writing RESULTS.md
- Before citing any number in a commit message, report, or task update

## Completeness checks

A result set is incomplete until proven complete:

- [ ] All modes defined in config have data in the latest version
- [ ] All schedulers have results for every mode — missing cells are **BLOCKING**
- [ ] Only the latest experiment version matters — older partial data is noise
- [ ] Rep count meets or exceeds `defaults.reps` for every cell
- [ ] Warmup period was excluded before computing statistics

**Missing modes or schedulers are BLOCKING gaps, not "nice to have".**

## Fishy number detection

Flag and investigate any of these:

- **Identical values** across different configs/modes/schedulers (e.g.,
  every P50 = 500 µs) → measurement bug until proven otherwise
- **Implausible magnitudes** — below hardware floor (< 1 µs timer
  resolution), above theoretical ceiling, or zero stddev on stochastic
  workloads
- **Too-round numbers** — real measurements are messy; clean multiples
  of 10 are suspicious
- **IRQ exposure > 50%** — exceeds random baseline; requires deep
  investigation (may be real pathology or measurement artifact)
- **Physical-model contradictions** — throughput up while latency up
  with no load change? Red flag.

## Hypothesis testing

For every experiment, explicitly document ALL THREE categories:

1. **✅ Evidence CONFIRMING** the hypothesis — cite metric, value,
   mode, scheduler, rep count
2. **❌ Evidence REFUTING** the hypothesis — even one data point must
   be explained, never swept under the rug
3. **🎲 Evidence that seems RANDOM or WRONG** — classify as noise vs.
   signal and explain why

A review listing only confirming evidence is not a review — it is
cheerleading.

## Methodology error awareness

The measurement pipeline itself can lie:

- **Pre-register expectations:** before looking at data, write down
  what you expect. Surprises require investigation.
- **Verify the pipeline:** timestamps monotonic? CSV columns correct?
  Non-zero sample count? Provenance hashes match?
- **When anything is fishy:** dig deeper — add tracing, run minimal
  repro, confirm mechanism, THEN accept or reject.
- **Provenance chain:** `provenance.json` matches intent, config hash
  matches current config, binary from expected git rev, kernel and host
  match.

## Report format

Every skeptic review MUST use this template:

```markdown
## Skeptic Review: [experiment] [version]

### ✅ Confirmed expectations
- [finding with evidence]

### ⚠️ Unexpected results
- [what was expected vs. observed, root cause if known]

### ❌ Missing / incomplete data
- [what is missing, why blocking, remediation]

### 🔍 Fishy numbers
- [what looks wrong, investigation done, conclusion]

### Verdict: PASS / FAIL / NEEDS INVESTIGATION
[one sentence justification]
```

**Empty sections MUST say "None found" — do not omit them.**
Omission is ambiguous; "None found" is a positive assertion.

## Quick reference

```bash
# After completing an experiment run:
# 1. Verify completeness
repm status                    # Shows mode×scheduler matrix with gaps

# 2. Run analysis with skeptic flag
repm analyze --skeptic         # Flags identical values, outliers, gaps

# 3. Write the skeptic review in RESULTS.md
# 4. Set verdict: PASS / FAIL / NEEDS INVESTIGATION
```
