// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! Trace comparison framework: compare original vs reproduced experiments.
//!
//! Goes beyond `score.rs` (which computes aggregate geomean ratios) by adding:
//! - Per-metric distribution shape comparison (p50/p99 spread)
//! - Per-thread breakdown and thread count matching
//! - Divergence flagging (>2× difference)
//! - Overall match percentage
//!
//! Usage: `repm analyze --compare <original> <reproduced>`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::score;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Per-metric comparison detail.
#[derive(Debug, Clone)]
pub struct MetricComparison {
    pub metric: String,
    pub original: MetricStats,
    pub reproduced: MetricStats,
    pub ratio: f64,
    pub divergent: bool, // true if ratio > 2.0
}

/// Stats for a single metric across threads.
#[derive(Debug, Clone)]
pub struct MetricStats {
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub thread_count: usize,
    pub sample_count: u64,
}

/// Full comparison result for a (scheduler, mode) pair.
#[derive(Debug)]
pub struct ComparisonResult {
    pub scheduler: String,
    pub mode: String,
    pub metrics: Vec<MetricComparison>,
    pub thread_count_original: usize,
    pub thread_count_reproduced: usize,
    pub match_pct: f64,
    pub divergent_count: usize,
}

// ---------------------------------------------------------------------------
// Metric extraction (richer than score.rs — keeps per-thread detail)
// ---------------------------------------------------------------------------

/// Key: (scheduler, mode) → metric_key → Vec<(thread_id, value, sample_count)>
type DetailedMetricMap = BTreeMap<(String, String), BTreeMap<String, Vec<(String, f64, u64)>>>;

/// Load detailed per-thread metrics from a combined_results.csv.
fn load_detailed_metrics(csv_path: &Path, thread_type: &str) -> Result<DetailedMetricMap> {
    let content = std::fs::read_to_string(csv_path)
        .with_context(|| format!("read {}", csv_path.display()))?;
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(content.as_bytes());

    let headers = reader.headers().context("CSV has no headers")?.clone();
    let col = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };

    let col_mode = col("mode");
    let col_scheduler = col("scheduler");
    let col_thread_type = col("thread_type");
    let col_thread_id = col("thread_id");
    let col_metric = col("metric_name");
    let col_percentile = col("percentile");
    let col_value = col("value");
    let col_unit = col("unit");
    let col_rep = col("rep");
    let col_sample = col("sample_count");
    let col_cpu = col("avg_cpu_util_pct");

    // Collect per-(sched, mode, rep) → metric → per-thread values
    type RepData = BTreeMap<String, Vec<(String, f64, u64)>>;
    let mut by_rep: BTreeMap<(String, String, u32), RepData> = BTreeMap::new();

    for record in reader.records() {
        let record = record?;
        let get = |c: Option<usize>| -> &str { c.and_then(|i| record.get(i)).unwrap_or("") };

        if let Some(tt) = col_thread_type {
            if record.get(tt).unwrap_or("") != thread_type {
                continue;
            }
        }

        let mode = get(col_mode).to_string();
        let sched = get(col_scheduler).to_string();
        let rep: u32 = get(col_rep).parse().unwrap_or(1);
        let thread_id = get(col_thread_id).to_string();
        let metric = get(col_metric);
        let pctile = get(col_percentile);
        let unit = get(col_unit);
        let sample: u64 = get(col_sample).parse().unwrap_or(0);

        let key = match (metric, pctile) {
            ("e2e_latency", "p50") => Some("e2e_p50"),
            ("e2e_latency", "p99") => Some("e2e_p99"),
            ("e2e_latency", "avg") => Some("e2e_avg"),
            ("sched_latency", "p50") => Some("sched_p50"),
            ("sched_latency", "p99") => Some("sched_p99"),
            ("sched_latency", "avg") => Some("sched_avg"),
            ("run_duration", "avg") => Some("run_duration"),
            ("irq_exposure", _) if unit == "pct" => Some("irq_exposure"),
            _ => None,
        };

        if let Some(key) = key {
            if let Ok(val) = get(col_value).parse::<f64>() {
                let val = if unit == "ns" { val / 1000.0 } else { val };
                by_rep
                    .entry((sched.clone(), mode.clone(), rep))
                    .or_default()
                    .entry(key.to_string())
                    .or_default()
                    .push((thread_id.clone(), val, sample));
            }
        }

        // CPU utilisation — store once per thread
        if metric == "e2e_latency" && pctile == "p50" {
            if let Some(cpu_col) = col_cpu {
                if let Some(cpu_str) = record.get(cpu_col) {
                    if let Ok(cpu_val) = cpu_str.parse::<f64>() {
                        by_rep
                            .entry((sched.clone(), mode.clone(), rep))
                            .or_default()
                            .entry("cpu_util".to_string())
                            .or_default()
                            .push((thread_id, cpu_val, sample));
                    }
                }
            }
        }
    }

    // Select median rep (by mean e2e_p99)
    let mut groups: BTreeMap<(String, String), Vec<(u32, RepData)>> = BTreeMap::new();
    for ((sched, mode, rep), data) in by_rep {
        groups.entry((sched, mode)).or_default().push((rep, data));
    }

    let mut result = DetailedMetricMap::new();
    for (key, mut reps) in groups {
        reps.sort_by(|a, b| {
            let mean_a = mean_of_entries(a.1.get("e2e_p99"));
            let mean_b = mean_of_entries(b.1.get("e2e_p99"));
            mean_a
                .partial_cmp(&mean_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let median_idx = reps.len() / 2;
        let (_rep, data) = reps.into_iter().nth(median_idx).unwrap();
        result.insert(key, data);
    }

    Ok(result)
}

fn mean_of_entries(entries: Option<&Vec<(String, f64, u64)>>) -> f64 {
    match entries {
        Some(v) if !v.is_empty() => v.iter().map(|(_, val, _)| val).sum::<f64>() / v.len() as f64,
        _ => f64::MAX,
    }
}

fn compute_stats(entries: &[(String, f64, u64)]) -> MetricStats {
    let values: Vec<f64> = entries.iter().map(|(_, v, _)| *v).collect();
    let total_samples: u64 = entries.iter().map(|(_, _, s)| s).sum();
    let n = values.len();
    if n == 0 {
        return MetricStats {
            mean: 0.0,
            min: 0.0,
            max: 0.0,
            thread_count: 0,
            sample_count: 0,
        };
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    // Unique thread count
    let mut thread_ids: Vec<&str> = entries.iter().map(|(id, _, _)| id.as_str()).collect();
    thread_ids.sort_unstable();
    thread_ids.dedup();
    MetricStats {
        mean,
        min,
        max,
        thread_count: thread_ids.len(),
        sample_count: total_samples,
    }
}

// ---------------------------------------------------------------------------
// Comparison logic
// ---------------------------------------------------------------------------

/// Compare two experiments and return per-(scheduler,mode) comparison results.
pub fn compare_experiments(
    original: &DetailedMetricMap,
    reproduced: &DetailedMetricMap,
) -> Vec<ComparisonResult> {
    let all_keys: std::collections::BTreeSet<_> =
        original.keys().chain(reproduced.keys()).cloned().collect();

    let all_metrics = [
        "e2e_p50",
        "e2e_p99",
        "e2e_avg",
        "sched_p50",
        "sched_p99",
        "sched_avg",
        "run_duration",
        "cpu_util",
        "irq_exposure",
    ];

    let mut results = Vec::new();

    for (sched, mode) in all_keys {
        let orig = original.get(&(sched.clone(), mode.clone()));
        let repr = reproduced.get(&(sched.clone(), mode.clone()));

        let (orig_data, repr_data) = match (orig, repr) {
            (Some(o), Some(r)) => (o, r),
            _ => continue, // Skip if only in one side
        };

        let mut metrics = Vec::new();
        let mut divergent_count = 0;

        // Count unique threads across all metrics
        let orig_threads = count_unique_threads(orig_data);
        let repr_threads = count_unique_threads(repr_data);

        for &metric in &all_metrics {
            let o_entries = orig_data.get(metric);
            let r_entries = repr_data.get(metric);

            if let (Some(oe), Some(re)) = (o_entries, r_entries) {
                let o_stats = compute_stats(oe);
                let r_stats = compute_stats(re);

                let ratio = if o_stats.mean > 0.0 && r_stats.mean > 0.0 {
                    let r = r_stats.mean / o_stats.mean;
                    r.max(1.0 / r)
                } else {
                    f64::NAN
                };

                let divergent = ratio > 2.0;
                if divergent {
                    divergent_count += 1;
                }

                metrics.push(MetricComparison {
                    metric: metric.to_string(),
                    original: o_stats,
                    reproduced: r_stats,
                    ratio,
                    divergent,
                });
            }
        }

        // Match percentage: (metrics within 2× / total comparable metrics) × 100
        let comparable = metrics.iter().filter(|m| !m.ratio.is_nan()).count();
        let within_2x = metrics
            .iter()
            .filter(|m| !m.ratio.is_nan() && !m.divergent)
            .count();
        let match_pct = if comparable > 0 {
            (within_2x as f64 / comparable as f64) * 100.0
        } else {
            0.0
        };

        results.push(ComparisonResult {
            scheduler: sched,
            mode,
            metrics,
            thread_count_original: orig_threads,
            thread_count_reproduced: repr_threads,
            match_pct,
            divergent_count,
        });
    }

    results
}

fn count_unique_threads(data: &BTreeMap<String, Vec<(String, f64, u64)>>) -> usize {
    let mut all_ids: Vec<&str> = data
        .values()
        .flat_map(|entries| entries.iter().map(|(id, _, _)| id.as_str()))
        .collect();
    all_ids.sort_unstable();
    all_ids.dedup();
    all_ids.len()
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

/// Format comparison results as markdown.
pub fn format_comparison(
    original_name: &str,
    reproduced_name: &str,
    results: &[ComparisonResult],
) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "# Trace Comparison: {} vs {}\n\n",
        original_name, reproduced_name
    ));

    for cr in results {
        out.push_str(&format!("## {} / {}\n\n", cr.mode, cr.scheduler));

        // Thread count
        out.push_str(&format!(
            "Threads: {} (original) → {} (reproduced)\n\n",
            cr.thread_count_original, cr.thread_count_reproduced
        ));

        // Metric table
        out.push_str("| Metric | Original (mean) | Reproduced (mean) | Ratio | Status |\n");
        out.push_str("|:-------|----------------:|------------------:|------:|:------:|\n");

        for mc in &cr.metrics {
            let status = if mc.divergent {
                "⚠️ >2×"
            } else if mc.ratio.is_nan() {
                "—"
            } else if mc.ratio < 1.1 {
                "✅ match"
            } else {
                "⚡ close"
            };

            let unit = if mc.metric.contains("cpu_util") || mc.metric.contains("irq") {
                "%"
            } else {
                "µs"
            };

            out.push_str(&format!(
                "| {} | {:.1}{} | {:.1}{} | {:.3}× | {} |\n",
                mc.metric, mc.original.mean, unit, mc.reproduced.mean, unit, mc.ratio, status
            ));
        }
        out.push('\n');

        // Distribution spread (p50 vs p99 ratio — "tail heaviness")
        let orig_p50 = cr
            .metrics
            .iter()
            .find(|m| m.metric == "e2e_p50")
            .map(|m| m.original.mean);
        let orig_p99 = cr
            .metrics
            .iter()
            .find(|m| m.metric == "e2e_p99")
            .map(|m| m.original.mean);
        let repr_p50 = cr
            .metrics
            .iter()
            .find(|m| m.metric == "e2e_p50")
            .map(|m| m.reproduced.mean);
        let repr_p99 = cr
            .metrics
            .iter()
            .find(|m| m.metric == "e2e_p99")
            .map(|m| m.reproduced.mean);

        if let (Some(op50), Some(op99), Some(rp50), Some(rp99)) =
            (orig_p50, orig_p99, repr_p50, repr_p99)
        {
            if op50 > 0.0 && rp50 > 0.0 {
                let orig_tail = op99 / op50;
                let repr_tail = rp99 / rp50;
                out.push_str(&format!(
                    "Tail ratio (P99/P50): {:.2}× (original) → {:.2}× (reproduced)\n\n",
                    orig_tail, repr_tail
                ));
            }
        }

        // Per-thread min/max spread for key metrics
        for mc in &cr.metrics {
            if mc.original.thread_count > 1 && (mc.metric == "e2e_p99" || mc.metric == "sched_p99")
            {
                out.push_str(&format!(
                    "{} spread: [{:.0}–{:.0}]µs (orig, {} threads) → [{:.0}–{:.0}]µs (repr, {} threads)\n",
                    mc.metric,
                    mc.original.min, mc.original.max, mc.original.thread_count,
                    mc.reproduced.min, mc.reproduced.max, mc.reproduced.thread_count,
                ));
            }
        }

        // Summary
        out.push_str(&format!(
            "\n**Match: {:.0}%** ({} divergent metrics)\n\n",
            cr.match_pct, cr.divergent_count
        ));
        out.push_str("---\n\n");
    }

    // Overall summary
    let total_metrics: usize = results.iter().map(|r| r.metrics.len()).sum();
    let total_divergent: usize = results.iter().map(|r| r.divergent_count).sum();
    let overall_match = if total_metrics > 0 {
        ((total_metrics - total_divergent) as f64 / total_metrics as f64) * 100.0
    } else {
        0.0
    };
    out.push_str(&format!(
        "**Overall: {:.0}% match** ({}/{} metrics within 2×)\n",
        overall_match,
        total_metrics - total_divergent,
        total_metrics
    ));

    out
}

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

/// Execute trace comparison: load two experiments and compare.
pub fn execute_compare(
    ws_root: &Path,
    original_name: &str,
    reproduced_name: &str,
    thread_type: &str,
    write: bool,
) -> Result<()> {
    let orig_dir = resolve_dir(ws_root, original_name)?;
    let repr_dir = resolve_dir(ws_root, reproduced_name)?;

    let orig_csv = find_csv(&orig_dir)?;
    let repr_csv = find_csv(&repr_dir)?;

    eprintln!("repm compare: {} vs {}", original_name, reproduced_name);

    let orig_metrics = load_detailed_metrics(&orig_csv, thread_type)?;
    let repr_metrics = load_detailed_metrics(&repr_csv, thread_type)?;

    let results = compare_experiments(&orig_metrics, &repr_metrics);
    let output = format_comparison(original_name, reproduced_name, &results);

    if write {
        let out_path = ws_root.join("experiments").join("TRACE_COMPARISON.md");
        std::fs::write(&out_path, &output)
            .with_context(|| format!("writing {}", out_path.display()))?;
        eprintln!("  wrote {}", out_path.display());
    } else {
        println!("{}", output);
    }

    // Also run score.rs for the aggregate geomean
    eprintln!();
    score::execute_score(ws_root, original_name, reproduced_name, thread_type)?;

    Ok(())
}

fn resolve_dir(ws_root: &Path, name: &str) -> Result<PathBuf> {
    let dir = ws_root.join("experiments").join(name);
    if dir.is_dir() {
        return Ok(dir);
    }
    let abs = PathBuf::from(name);
    if abs.is_dir() {
        return Ok(abs);
    }
    bail!("Experiment not found: {}", name);
}

fn find_csv(exp_dir: &Path) -> Result<PathBuf> {
    let combined = exp_dir.join("data").join("combined_results.csv");
    if combined.is_file() {
        return Ok(combined);
    }
    let metrics = exp_dir.join("data").join("metrics.csv");
    if metrics.is_file() {
        return Ok(metrics);
    }
    bail!(
        "No combined_results.csv or metrics.csv in {}/data/",
        exp_dir.display()
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entries(values: &[(f64, u64)]) -> Vec<(String, f64, u64)> {
        values
            .iter()
            .enumerate()
            .map(|(i, &(v, s))| (format!("thread_{}", i), v, s))
            .collect()
    }

    #[test]
    fn test_compute_stats_basic() {
        let entries = make_entries(&[(100.0, 50), (200.0, 50), (300.0, 50)]);
        let stats = compute_stats(&entries);
        assert_eq!(stats.thread_count, 3);
        assert!((stats.mean - 200.0).abs() < 1e-10);
        assert!((stats.min - 100.0).abs() < 1e-10);
        assert!((stats.max - 300.0).abs() < 1e-10);
        assert_eq!(stats.sample_count, 150);
    }

    #[test]
    fn test_compute_stats_empty() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.thread_count, 0);
        assert_eq!(stats.mean, 0.0);
    }

    #[test]
    fn test_compare_identical() {
        let mut orig = DetailedMetricMap::new();
        let mut repr = DetailedMetricMap::new();

        let entries = make_entries(&[(1000.0, 100), (1010.0, 100)]);
        let mut metrics = BTreeMap::new();
        metrics.insert("e2e_p50".to_string(), entries.clone());
        metrics.insert("e2e_p99".to_string(), entries.clone());
        metrics.insert("cpu_util".to_string(), make_entries(&[(50.0, 1)]));

        orig.insert(("lavd".into(), "sim".into()), metrics.clone());
        repr.insert(("lavd".into(), "sim".into()), metrics);

        let results = compare_experiments(&orig, &repr);
        assert_eq!(results.len(), 1);

        let cr = &results[0];
        assert_eq!(cr.divergent_count, 0);
        assert!((cr.match_pct - 100.0).abs() < 1e-10);
        for mc in &cr.metrics {
            assert!(
                (mc.ratio - 1.0).abs() < 1e-10,
                "{} ratio should be 1.0, got {}",
                mc.metric,
                mc.ratio
            );
        }
    }

    #[test]
    fn test_compare_divergent() {
        let mut orig = DetailedMetricMap::new();
        let mut repr = DetailedMetricMap::new();

        let mut o_metrics = BTreeMap::new();
        o_metrics.insert("e2e_p50".to_string(), make_entries(&[(1000.0, 100)]));
        o_metrics.insert("e2e_p99".to_string(), make_entries(&[(1000.0, 100)]));
        o_metrics.insert("cpu_util".to_string(), make_entries(&[(30.0, 1)]));

        let mut r_metrics = BTreeMap::new();
        r_metrics.insert("e2e_p50".to_string(), make_entries(&[(1000.0, 100)])); // same
        r_metrics.insert("e2e_p99".to_string(), make_entries(&[(3000.0, 100)])); // 3× diff
        r_metrics.insert("cpu_util".to_string(), make_entries(&[(90.0, 1)])); // 3× diff

        orig.insert(("lavd".into(), "sim".into()), o_metrics);
        repr.insert(("lavd".into(), "sim".into()), r_metrics);

        let results = compare_experiments(&orig, &repr);
        let cr = &results[0];

        assert_eq!(cr.divergent_count, 2); // e2e_p99 and cpu_util >2×
        let e2e_p99 = cr.metrics.iter().find(|m| m.metric == "e2e_p99").unwrap();
        assert!(e2e_p99.divergent);
        assert!((e2e_p99.ratio - 3.0).abs() < 1e-10);

        let cpu = cr.metrics.iter().find(|m| m.metric == "cpu_util").unwrap();
        assert!(cpu.divergent);
    }

    #[test]
    fn test_compare_thread_counts() {
        let mut orig = DetailedMetricMap::new();
        let mut repr = DetailedMetricMap::new();

        // Original: 4 threads
        let o_entries: Vec<(String, f64, u64)> =
            (0..4).map(|i| (format!("fg_{}", i), 1000.0, 100)).collect();
        // Reproduced: 2 threads
        let r_entries: Vec<(String, f64, u64)> =
            (0..2).map(|i| (format!("fg_{}", i), 1000.0, 100)).collect();

        let mut o_m = BTreeMap::new();
        o_m.insert("e2e_p50".to_string(), o_entries);
        let mut r_m = BTreeMap::new();
        r_m.insert("e2e_p50".to_string(), r_entries);

        orig.insert(("lavd".into(), "sim".into()), o_m);
        repr.insert(("lavd".into(), "sim".into()), r_m);

        let results = compare_experiments(&orig, &repr);
        let cr = &results[0];
        assert_eq!(cr.thread_count_original, 4);
        assert_eq!(cr.thread_count_reproduced, 2);
    }

    #[test]
    fn test_format_output_has_key_sections() {
        let results = vec![ComparisonResult {
            scheduler: "lavd".into(),
            mode: "rtapp_sim".into(),
            metrics: vec![MetricComparison {
                metric: "e2e_p50".into(),
                original: MetricStats {
                    mean: 1000.0,
                    min: 990.0,
                    max: 1010.0,
                    thread_count: 4,
                    sample_count: 400,
                },
                reproduced: MetricStats {
                    mean: 1050.0,
                    min: 1040.0,
                    max: 1060.0,
                    thread_count: 4,
                    sample_count: 400,
                },
                ratio: 1.05,
                divergent: false,
            }],
            thread_count_original: 4,
            thread_count_reproduced: 4,
            match_pct: 100.0,
            divergent_count: 0,
        }];

        let output = format_comparison("orig", "repr", &results);
        assert!(output.contains("Trace Comparison"));
        assert!(output.contains("rtapp_sim / lavd"));
        assert!(output.contains("Match: 100%"));
        assert!(output.contains("Overall:"));
        assert!(output.contains("✅ match") || output.contains("⚡ close"));
    }

    #[test]
    fn test_no_overlap_produces_empty() {
        let mut orig = DetailedMetricMap::new();
        let mut repr = DetailedMetricMap::new();

        let mut o_m = BTreeMap::new();
        o_m.insert("e2e_p50".to_string(), make_entries(&[(100.0, 10)]));
        orig.insert(("lavd".into(), "sim".into()), o_m);

        let mut r_m = BTreeMap::new();
        r_m.insert("e2e_p50".to_string(), make_entries(&[(100.0, 10)]));
        repr.insert(("tickless".into(), "sim".into()), r_m);

        let results = compare_experiments(&orig, &repr);
        // No overlapping (scheduler, mode) keys → empty
        assert!(results.is_empty());
    }
}
