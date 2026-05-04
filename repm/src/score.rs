// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! Accuracy scoring: compare original vs reproduced experiment metrics.
//!
//! **Formula**: For each metric M, compute
//!   `ratio_M = max(reproduced / original, original / reproduced)`
//! so that ratio ≥ 1.0 regardless of direction. The overall score is:
//!   `score = geomean(all ratios) = (∏ ratio_i)^(1/n)`
//! Perfect reproduction → 1.0×. Higher = worse.
//!
//! Missing metrics are reported but do not contribute to the score.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Metric identifiers
// ---------------------------------------------------------------------------

/// The set of metrics used for scoring.
const SCORE_METRICS: &[&str] = &[
    "e2e_p50",
    "e2e_p99",
    "sched_p50",
    "sched_p99",
    "cpu_util",
    "irq_exposure",
];

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Per-metric comparison result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRatio {
    pub metric: String,
    pub original: f64,
    pub reproduced: f64,
    /// Symmetric ratio: max(a/b, b/a), always ≥ 1.0.
    pub ratio: f64,
}

/// Full scoring result for an experiment pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreResult {
    /// Per-metric ratios (only metrics present in both datasets).
    pub ratios: Vec<MetricRatio>,
    /// Metrics present in original but missing from reproduced.
    pub missing_reproduced: Vec<String>,
    /// Metrics present in reproduced but missing from original.
    pub missing_original: Vec<String>,
    /// Geometric mean of all ratios. `None` if no ratios available.
    pub geomean: Option<f64>,
    /// Wall-clock time for the pipeline (if tracked).
    pub elapsed_secs: Option<f64>,
    /// Token count (if AI was used; 0 for pure algorithmic).
    pub tokens: Option<u64>,
}

/// Compute the symmetric ratio: max(a/b, b/a).
/// Returns `None` if either value is zero or negative.
fn symmetric_ratio(a: f64, b: f64) -> Option<f64> {
    if a <= 0.0 || b <= 0.0 {
        return None;
    }
    let r = a / b;
    Some(r.max(1.0 / r))
}

/// Compute the geometric mean of a slice of positive values.
/// Returns `None` if the slice is empty.
fn geomean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let log_sum: f64 = values.iter().map(|v| v.ln()).sum();
    Some((log_sum / values.len() as f64).exp())
}

// ---------------------------------------------------------------------------
// Metric extraction from CSV
// ---------------------------------------------------------------------------

/// Extracted per-(scheduler, mode) metrics from a combined_results.csv.
/// Key: (scheduler, mode) → metric_name → value.
type MetricMap = BTreeMap<(String, String), BTreeMap<String, f64>>;

/// Load metrics from a combined_results.csv, averaging across threads
/// and selecting the median rep (by e2e_p99).
///
/// Returns a map of (scheduler, mode) → { metric_name → value }.
fn load_metrics(csv_path: &Path, thread_type: &str) -> Result<MetricMap> {
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
    let col_metric = col("metric_name");
    let col_percentile = col("percentile");
    let col_value = col("value");
    let col_unit = col("unit");
    let col_rep = col("rep");
    let col_cpu = col("avg_cpu_util_pct");

    // Collect: (scheduler, mode, rep) → metric_key → [values across threads]
    let mut raw: BTreeMap<(String, String, u32), BTreeMap<String, Vec<f64>>> = BTreeMap::new();

    for record in reader.records() {
        let record = record?;
        let get = |c: Option<usize>| -> &str { c.and_then(|i| record.get(i)).unwrap_or("") };

        // Filter by thread type
        if let Some(tt) = col_thread_type {
            if record.get(tt).unwrap_or("") != thread_type {
                continue;
            }
        }

        let mode = get(col_mode).to_string();
        let sched = get(col_scheduler).to_string();
        let rep: u32 = get(col_rep).parse().unwrap_or(1);
        let metric = get(col_metric);
        let pctile = get(col_percentile);
        let unit = get(col_unit);

        // Map CSV metric names to our scoring keys
        let key = match (metric, pctile) {
            ("e2e_latency", "p50") => Some("e2e_p50"),
            ("e2e_latency", "p99") => Some("e2e_p99"),
            ("sched_latency", "p50") => Some("sched_p50"),
            ("sched_latency", "p99") => Some("sched_p99"),
            ("irq_exposure", _) if unit == "pct" => Some("irq_exposure"),
            _ => None,
        };

        if let Some(key) = key {
            if let Ok(val) = get(col_value).parse::<f64>() {
                // Convert ns → µs for latency metrics
                let val = if unit == "ns" { val / 1000.0 } else { val };
                raw.entry((sched.clone(), mode.clone(), rep))
                    .or_default()
                    .entry(key.to_string())
                    .or_default()
                    .push(val);
            }
        }

        // CPU utilisation (once per rep, not per-metric)
        if metric == "e2e_latency" && pctile == "p50" {
            if let Some(cpu_col) = col_cpu {
                if let Some(cpu_str) = record.get(cpu_col) {
                    if let Ok(cpu_val) = cpu_str.parse::<f64>() {
                        raw.entry((sched.clone(), mode.clone(), rep))
                            .or_default()
                            .entry("cpu_util".to_string())
                            .or_default()
                            .push(cpu_val);
                    }
                }
            }
        }
    }

    // For each (scheduler, mode), find the median rep by mean e2e_p99
    type RepMetrics = Vec<(u32, BTreeMap<String, Vec<f64>>)>;
    let mut groups: BTreeMap<(String, String), RepMetrics> = BTreeMap::new();
    for ((sched, mode, rep), metrics) in raw {
        groups
            .entry((sched, mode))
            .or_default()
            .push((rep, metrics));
    }

    let mut result = MetricMap::new();
    for (key, mut reps) in groups {
        // Sort by rep's mean e2e_p99
        reps.sort_by(|a, b| {
            let mean_a = mean_of(a.1.get("e2e_p99"));
            let mean_b = mean_of(b.1.get("e2e_p99"));
            mean_a
                .partial_cmp(&mean_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Pick median
        let median_idx = reps.len() / 2;
        let (_rep, metrics) = &reps[median_idx];

        let mut agg = BTreeMap::new();
        for (metric_name, values) in metrics {
            if !values.is_empty() {
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                // For cpu_util, take the first (they're all the same per rep)
                let val = if metric_name == "cpu_util" {
                    values[0]
                } else {
                    mean
                };
                agg.insert(metric_name.clone(), val);
            }
        }
        result.insert(key, agg);
    }

    Ok(result)
}

fn mean_of(values: Option<&Vec<f64>>) -> f64 {
    match values {
        Some(v) if !v.is_empty() => v.iter().sum::<f64>() / v.len() as f64,
        _ => f64::MAX,
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Compare two metric maps and compute the accuracy score.
pub fn compute_score(
    original: &MetricMap,
    reproduced: &MetricMap,
) -> Vec<((String, String), ScoreResult)> {
    // Collect all (scheduler, mode) keys from both maps
    let all_keys: std::collections::BTreeSet<_> =
        original.keys().chain(reproduced.keys()).cloned().collect();

    let mut results = Vec::new();

    for key in all_keys {
        let orig = original.get(&key);
        let repr = reproduced.get(&key);

        match (orig, repr) {
            (Some(o), Some(r)) => {
                let mut ratios = Vec::new();
                let mut missing_reproduced = Vec::new();
                let mut missing_original = Vec::new();

                for &metric in SCORE_METRICS {
                    let o_val = o.get(metric);
                    let r_val = r.get(metric);
                    match (o_val, r_val) {
                        (Some(&ov), Some(&rv)) => {
                            if let Some(ratio) = symmetric_ratio(ov, rv) {
                                ratios.push(MetricRatio {
                                    metric: metric.to_string(),
                                    original: ov,
                                    reproduced: rv,
                                    ratio,
                                });
                            }
                        }
                        (Some(_), None) => missing_reproduced.push(metric.to_string()),
                        (None, Some(_)) => missing_original.push(metric.to_string()),
                        (None, None) => {} // not present in either — skip
                    }
                }

                let ratio_values: Vec<f64> = ratios.iter().map(|r| r.ratio).collect();
                let geomean_val = geomean(&ratio_values);

                results.push((
                    key,
                    ScoreResult {
                        ratios,
                        missing_reproduced,
                        missing_original,
                        geomean: geomean_val,
                        elapsed_secs: None,
                        tokens: None,
                    },
                ));
            }
            (Some(_), None) => {
                // Key only in original — all metrics missing from reproduced
                let missing: Vec<String> = SCORE_METRICS.iter().map(|s| s.to_string()).collect();
                results.push((
                    key,
                    ScoreResult {
                        ratios: Vec::new(),
                        missing_reproduced: missing,
                        missing_original: Vec::new(),
                        geomean: None,
                        elapsed_secs: None,
                        tokens: None,
                    },
                ));
            }
            (None, Some(_)) => {
                let missing: Vec<String> = SCORE_METRICS.iter().map(|s| s.to_string()).collect();
                results.push((
                    key,
                    ScoreResult {
                        ratios: Vec::new(),
                        missing_reproduced: Vec::new(),
                        missing_original: missing,
                        geomean: None,
                        elapsed_secs: None,
                        tokens: None,
                    },
                ));
            }
            (None, None) => unreachable!(),
        }
    }

    results
}

// ---------------------------------------------------------------------------
// CLI integration
// ---------------------------------------------------------------------------

/// Execute the `--score` workflow: load two experiments and compare.
pub fn execute_score(
    ws_root: &Path,
    original_name: &str,
    reproduced_name: &str,
    thread_type: &str,
) -> Result<()> {
    let orig_dir = resolve_experiment_dir(ws_root, original_name)?;
    let repr_dir = resolve_experiment_dir(ws_root, reproduced_name)?;

    let orig_csv = find_combined_csv(&orig_dir)?;
    let repr_csv = find_combined_csv(&repr_dir)?;

    eprintln!("repm score: {} vs {}", original_name, reproduced_name);
    eprintln!("  original:   {}", orig_csv.display());
    eprintln!("  reproduced: {}", repr_csv.display());

    let orig_metrics = load_metrics(&orig_csv, thread_type)
        .with_context(|| format!("loading original {}", orig_csv.display()))?;
    let repr_metrics = load_metrics(&repr_csv, thread_type)
        .with_context(|| format!("loading reproduced {}", repr_csv.display()))?;

    let results = compute_score(&orig_metrics, &repr_metrics);

    // Output
    println!(
        "# Accuracy Score: {} vs {}\n",
        original_name, reproduced_name
    );
    println!("Thread type: {}\n", thread_type);

    for ((sched, mode), score) in &results {
        println!("## {} / {}\n", mode, sched);

        if score.ratios.is_empty() {
            println!("  No comparable metrics (run did not converge)\n");
            continue;
        }

        println!("| Metric | Original | Reproduced | Ratio |");
        println!("|:-------|--------:|-----------:|------:|");
        for r in &score.ratios {
            println!(
                "| {} | {:.1} | {:.1} | {:.3}× |",
                r.metric, r.original, r.reproduced, r.ratio
            );
        }
        println!();

        if !score.missing_reproduced.is_empty() {
            println!(
                "Missing in reproduced: {}\n",
                score.missing_reproduced.join(", ")
            );
        }
        if !score.missing_original.is_empty() {
            println!(
                "Missing in original: {}\n",
                score.missing_original.join(", ")
            );
        }

        match score.geomean {
            Some(g) => println!(
                "**Score: {:.4}×** (geomean of {} ratios)\n",
                g,
                score.ratios.len()
            ),
            None => println!("**Score: UNDEFINED** (no comparable metrics)\n"),
        }
    }

    // Overall score (geomean of all individual geomeans)
    let all_geomeans: Vec<f64> = results.iter().filter_map(|(_, s)| s.geomean).collect();
    if let Some(overall) = geomean(&all_geomeans) {
        println!("---\n");
        println!(
            "**Overall Score: {:.4}×** (geomean across {} groups)\n",
            overall,
            all_geomeans.len()
        );
    }

    // Write benchmark.json to reproduced dir
    let benchmark = Benchmark {
        original: original_name.to_string(),
        reproduced: reproduced_name.to_string(),
        thread_type: thread_type.to_string(),
        results: results
            .into_iter()
            .map(|((s, m), score)| BenchmarkEntry {
                scheduler: s,
                mode: m,
                score,
            })
            .collect(),
    };

    let benchmark_path = repr_dir.join("benchmark.json");
    let json = serde_json::to_string_pretty(&benchmark).context("serializing benchmark")?;
    std::fs::write(&benchmark_path, &json)
        .with_context(|| format!("writing {}", benchmark_path.display()))?;
    eprintln!("  wrote {}", benchmark_path.display());

    Ok(())
}

/// Benchmark tracking data written to experiments/<version>/benchmark.json.
#[derive(Debug, Serialize, Deserialize)]
struct Benchmark {
    original: String,
    reproduced: String,
    thread_type: String,
    results: Vec<BenchmarkEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchmarkEntry {
    scheduler: String,
    mode: String,
    score: ScoreResult,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_experiment_dir(ws_root: &Path, name: &str) -> Result<PathBuf> {
    let dir = ws_root.join("experiments").join(name);
    if dir.is_dir() {
        return Ok(dir);
    }
    // Try as absolute path
    let abs = PathBuf::from(name);
    if abs.is_dir() {
        return Ok(abs);
    }
    bail!(
        "Experiment directory not found: {} (tried {} and {})",
        name,
        dir.display(),
        abs.display()
    );
}

fn find_combined_csv(exp_dir: &Path) -> Result<PathBuf> {
    let data_dir = exp_dir.join("data");
    // Try combined_results.csv first
    let combined = data_dir.join("combined_results.csv");
    if combined.is_file() {
        return Ok(combined);
    }
    // Try metrics.csv
    let metrics = data_dir.join("metrics.csv");
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
    use std::io::Write;

    fn make_csv(rows: &[&str]) -> String {
        let header = "timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct";
        let mut lines = vec![header.to_string()];
        for row in rows {
            lines.push(row.to_string());
        }
        lines.join("\n") + "\n"
    }

    fn write_temp_csv(dir: &Path, content: &str) -> PathBuf {
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let csv_path = data_dir.join("combined_results.csv");
        let mut f = std::fs::File::create(&csv_path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        csv_path
    }

    fn make_row(
        sched: &str,
        metric: &str,
        pctile: &str,
        value: f64,
        unit: &str,
        rep: u32,
        cpu: f64,
    ) -> String {
        format!(
            "2026-01-01,rtapp_sim,{},baseline,foreground,thread_0,{},{},{},{},100,{},test,{}",
            sched, metric, pctile, value, unit, rep, cpu
        )
    }

    #[test]
    fn test_symmetric_ratio_equal() {
        assert_eq!(symmetric_ratio(100.0, 100.0), Some(1.0));
    }

    #[test]
    fn test_symmetric_ratio_2x() {
        assert_eq!(symmetric_ratio(200.0, 100.0), Some(2.0));
        assert_eq!(symmetric_ratio(100.0, 200.0), Some(2.0));
    }

    #[test]
    fn test_symmetric_ratio_zero() {
        assert_eq!(symmetric_ratio(0.0, 100.0), None);
        assert_eq!(symmetric_ratio(100.0, 0.0), None);
    }

    #[test]
    fn test_geomean_single() {
        let g = geomean(&[4.0]).unwrap();
        assert!((g - 4.0).abs() < 1e-10);
    }

    #[test]
    fn test_geomean_pair() {
        let g = geomean(&[2.0, 8.0]).unwrap();
        assert!((g - 4.0).abs() < 1e-10);
    }

    #[test]
    fn test_geomean_empty() {
        assert!(geomean(&[]).is_none());
    }

    #[test]
    fn test_geomean_ones() {
        let g = geomean(&[1.0, 1.0, 1.0]).unwrap();
        assert!((g - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_score_identical_datasets() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = tmp.path().join("orig");
        let repr_dir = tmp.path().join("repr");
        std::fs::create_dir_all(&orig_dir).unwrap();
        std::fs::create_dir_all(&repr_dir).unwrap();

        let csv = make_csv(&[
            &make_row("lavd", "e2e_latency", "p50", 1000000.0, "ns", 1, 50.0),
            &make_row("lavd", "e2e_latency", "p99", 2000000.0, "ns", 1, 50.0),
            &make_row("lavd", "sched_latency", "p50", 10000.0, "ns", 1, 50.0),
            &make_row("lavd", "sched_latency", "p99", 50000.0, "ns", 1, 50.0),
        ]);

        let orig_csv = write_temp_csv(&orig_dir, &csv);
        let repr_csv = write_temp_csv(&repr_dir, &csv);

        let orig_m = load_metrics(&orig_csv, "foreground").unwrap();
        let repr_m = load_metrics(&repr_csv, "foreground").unwrap();

        let results = compute_score(&orig_m, &repr_m);
        assert_eq!(results.len(), 1);

        let (_, score) = &results[0];
        assert!(score.geomean.is_some());
        let g = score.geomean.unwrap();
        assert!(
            (g - 1.0).abs() < 1e-10,
            "identical datasets should score 1.0, got {}",
            g
        );
    }

    #[test]
    fn test_score_known_2x_difference() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = tmp.path().join("orig");
        let repr_dir = tmp.path().join("repr");
        std::fs::create_dir_all(&orig_dir).unwrap();
        std::fs::create_dir_all(&repr_dir).unwrap();

        // Original: all values 1000µs
        let orig_csv_content = make_csv(&[
            &make_row("lavd", "e2e_latency", "p50", 1000000.0, "ns", 1, 50.0),
            &make_row("lavd", "e2e_latency", "p99", 1000000.0, "ns", 1, 50.0),
        ]);
        // Reproduced: all values 2000µs (2× difference)
        let repr_csv_content = make_csv(&[
            &make_row("lavd", "e2e_latency", "p50", 2000000.0, "ns", 1, 50.0),
            &make_row("lavd", "e2e_latency", "p99", 2000000.0, "ns", 1, 50.0),
        ]);

        let orig_csv = write_temp_csv(&orig_dir, &orig_csv_content);
        let repr_csv = write_temp_csv(&repr_dir, &repr_csv_content);

        let orig_m = load_metrics(&orig_csv, "foreground").unwrap();
        let repr_m = load_metrics(&repr_csv, "foreground").unwrap();

        let results = compute_score(&orig_m, &repr_m);
        let (_, score) = &results[0];

        // Both e2e_p50 and e2e_p99 have ratio 2.0, cpu_util ratio 1.0
        // geomean(2.0, 2.0, 1.0) = (2*2*1)^(1/3) = 4^(1/3) ≈ 1.587
        // Actually cpu_util is present (50.0 in both) so ratio = 1.0
        // e2e_p50 ratio = 2.0, e2e_p99 ratio = 2.0, cpu_util ratio = 1.0
        let g = score.geomean.unwrap();
        let expected = (2.0_f64 * 2.0 * 1.0_f64).powf(1.0 / 3.0);
        assert!(
            (g - expected).abs() < 1e-6,
            "expected geomean {}, got {}",
            expected,
            g
        );

        // Check individual ratios
        for r in &score.ratios {
            match r.metric.as_str() {
                "e2e_p50" | "e2e_p99" => {
                    assert!(
                        (r.ratio - 2.0).abs() < 1e-10,
                        "{} ratio should be 2.0, got {}",
                        r.metric,
                        r.ratio
                    );
                }
                "cpu_util" => {
                    assert!(
                        (r.ratio - 1.0).abs() < 1e-10,
                        "cpu_util ratio should be 1.0, got {}",
                        r.ratio
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_score_missing_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = tmp.path().join("orig");
        let repr_dir = tmp.path().join("repr");
        std::fs::create_dir_all(&orig_dir).unwrap();
        std::fs::create_dir_all(&repr_dir).unwrap();

        // Original has sched_latency, reproduced does not
        let orig_csv_content = make_csv(&[
            &make_row("lavd", "e2e_latency", "p50", 1000000.0, "ns", 1, 50.0),
            &make_row("lavd", "sched_latency", "p99", 50000.0, "ns", 1, 50.0),
        ]);
        let repr_csv_content = make_csv(&[
            &make_row("lavd", "e2e_latency", "p50", 1000000.0, "ns", 1, 50.0),
            // no sched_latency
        ]);

        let orig_csv = write_temp_csv(&orig_dir, &orig_csv_content);
        let repr_csv = write_temp_csv(&repr_dir, &repr_csv_content);

        let orig_m = load_metrics(&orig_csv, "foreground").unwrap();
        let repr_m = load_metrics(&repr_csv, "foreground").unwrap();

        let results = compute_score(&orig_m, &repr_m);
        let (_, score) = &results[0];

        // sched_p99 should be in missing_reproduced
        assert!(
            score.missing_reproduced.contains(&"sched_p99".to_string()),
            "sched_p99 should be missing from reproduced, got {:?}",
            score.missing_reproduced
        );

        // Score should only include available metrics (e2e_p50 + cpu_util)
        assert!(
            score.geomean.is_some(),
            "should have a score for available metrics"
        );
        // Both e2e_p50 and cpu_util match → score = 1.0
        let g = score.geomean.unwrap();
        assert!(
            (g - 1.0).abs() < 1e-10,
            "matching metrics should give 1.0, got {}",
            g
        );
    }

    #[test]
    fn test_score_no_overlap() {
        let orig = MetricMap::new();
        let repr = MetricMap::new();
        let results = compute_score(&orig, &repr);
        assert!(results.is_empty());
    }

    #[test]
    fn test_score_one_side_only() {
        let mut orig = MetricMap::new();
        orig.insert(
            ("lavd".into(), "rtapp_sim".into()),
            [("e2e_p50".into(), 1000.0)].into_iter().collect(),
        );
        let repr = MetricMap::new();
        let results = compute_score(&orig, &repr);
        assert_eq!(results.len(), 1);
        assert!(results[0].1.geomean.is_none());
        assert!(!results[0].1.missing_reproduced.is_empty());
    }

    #[test]
    fn test_geomean_calculation_precision() {
        // geomean(1.5, 2.0, 3.0) = (1.5 * 2.0 * 3.0)^(1/3) = 9^(1/3) ≈ 2.0801
        let g = geomean(&[1.5, 2.0, 3.0]).unwrap();
        let expected = (9.0_f64).powf(1.0 / 3.0);
        assert!(
            (g - expected).abs() < 1e-10,
            "geomean(1.5, 2.0, 3.0) should be {}, got {}",
            expected,
            g
        );
    }
}
