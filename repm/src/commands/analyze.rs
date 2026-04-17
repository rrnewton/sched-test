//! `repm analyze` — post-processing and comparison table generation.
//!
//! Reads experiment CSV files and generates markdown comparison tables with
//! source citations for every value. Implements median-rep selection and
//! cross-checks metrics from multiple sources.
//!
//! **Data integrity rule**: every number in the output must trace back to
//! a source CSV file with line numbers. No hardcoded values.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::workspace;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

/// Post-processing: results tables, cross-check metrics.
///
/// Reads experiment CSVs and generates markdown comparison tables.
/// Every cell cites its source file and computation.
#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Experiment version to analyze (e.g., "v1", "baseline").
    /// If omitted, auto-discovers all experiments.
    pub experiment: Option<String>,

    /// Compare two experiment versions (e.g., --compare v1 v2).
    #[arg(long, num_args = 2)]
    pub compare: Option<Vec<String>>,

    /// Thread type to filter on (e.g., "foreground", "cache_worker").
    /// Default: "foreground".
    #[arg(long, default_value = "foreground")]
    pub thread_type: String,

    /// Output format: markdown or csv.
    #[arg(long, default_value = "markdown")]
    pub format: OutputFormat,

    /// Show source citations inline in the output.
    #[arg(long)]
    pub citations: bool,

    /// Write output to experiments/<version>/RESULTS.md instead of stdout.
    #[arg(long)]
    pub write: bool,

    /// Run cross-check validation on metrics.
    #[arg(long)]
    pub cross_check: bool,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Markdown,
    Csv,
}

// ---------------------------------------------------------------------------
// Data model — provenance tracking
// ---------------------------------------------------------------------------

/// A numeric value with full provenance back to source CSV.
#[derive(Debug, Clone)]
struct CitedValue {
    value: f64,
    source_file: String,
    source_lines: Vec<usize>,
    computation: String,
}

impl CitedValue {
    fn citation(&self) -> String {
        let lines_str = if self.source_lines.len() <= 3 {
            self.source_lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(",")
        } else {
            format!(
                "{}..{}",
                self.source_lines.first().unwrap_or(&0),
                self.source_lines.last().unwrap_or(&0)
            )
        };
        format!("[{}:{} ({})]", self.source_file, lines_str, self.computation)
    }

    fn format_us(&self) -> String {
        // Value is in nanoseconds, display in microseconds
        let us = self.value / 1000.0;
        if us < 1.0 {
            format!("{us:.2}")
        } else if us < 100.0 {
            format!("{us:.1}")
        } else {
            format!("{us:.0}")
        }
    }

    fn format_pct(&self) -> String {
        format!("{:.1}%", self.value)
    }
}

/// Raw values collected from CSV rows for one metric cell.
#[derive(Debug, Clone, Default)]
struct CellData {
    /// (value, source_file, line_number)
    raw_values: Vec<(f64, String, usize)>,
}

impl CellData {
    fn push(&mut self, value: f64, source_file: &str, line_number: usize) {
        self.raw_values
            .push((value, source_file.to_string(), line_number));
    }

    fn is_empty(&self) -> bool {
        self.raw_values.is_empty()
    }

    /// Aggregate raw values into a single cited value via mean.
    fn aggregate_mean(&self) -> Option<CitedValue> {
        if self.raw_values.is_empty() {
            return None;
        }
        let vals: Vec<f64> = self.raw_values.iter().map(|v| v.0).collect();
        let files: BTreeSet<&str> = self.raw_values.iter().map(|v| v.1.as_str()).collect();
        let mut lines: Vec<usize> = self.raw_values.iter().map(|v| v.2).collect();
        lines.sort();

        let (value, computation) = if vals.len() == 1 {
            (vals[0], "direct".to_string())
        } else {
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            (mean, format!("mean(n={})", vals.len()))
        };

        Some(CitedValue {
            value,
            source_file: files.into_iter().collect::<Vec<_>>().join("; "),
            source_lines: lines,
            computation,
        })
    }
}

/// All metrics for one rep within a (scheduler, mode) group.
#[derive(Debug, Clone, Default)]
struct RepData {
    #[allow(dead_code)]
    rep_id: String,
    e2e_p50: CellData,
    e2e_p90: CellData,
    e2e_p99: CellData,
    e2e_p999: CellData,
    sched_p50: CellData,
    sched_p99: CellData,
    irq_exposure: CellData,
    cpu_util: CellData,
}

/// One row in the output table, fully cited.
#[derive(Debug, Clone)]
struct TableRow {
    scheduler: String,
    mode: String,
    experiment: String,
    e2e_p50: Option<CitedValue>,
    e2e_p90: Option<CitedValue>,
    e2e_p99: Option<CitedValue>,
    e2e_p999: Option<CitedValue>,
    sched_p50: Option<CitedValue>,
    sched_p99: Option<CitedValue>,
    irq_exposure: Option<CitedValue>,
    cpu_util: Option<CitedValue>,
    selected_rep: String,
    warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// CSV loading with line-level provenance
// ---------------------------------------------------------------------------

/// Key: (scheduler, mode, rep_id) → RepData
type RepMap = HashMap<(String, String, String), RepData>;

fn load_csv_with_provenance(
    csv_path: &Path,
    base_dir: &Path,
    thread_type: &str,
) -> Result<RepMap> {
    let rel_path = csv_path
        .strip_prefix(base_dir)
        .unwrap_or(csv_path)
        .to_string_lossy()
        .to_string();

    let content = std::fs::read_to_string(csv_path)
        .with_context(|| format!("Failed to read CSV: {}", csv_path.display()))?;

    let mut result = RepMap::new();
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(content.as_bytes());

    let headers = reader
        .headers()
        .context("CSV has no headers")?
        .clone();

    // Build column index map
    let col = |name: &str| -> Option<usize> {
        headers.iter().position(|h| h == name)
    };

    let col_mode = col("mode");
    let col_scheduler = col("scheduler");
    let col_thread_type = col("thread_type");
    let col_metric_name = col("metric_name");
    let col_percentile = col("percentile");
    let col_value = col("value");
    let col_rep = col("rep");
    let col_notes = col("notes");
    let col_cpu_util = col("avg_cpu_util_pct");

    // Validate required columns
    if col_mode.is_none() || col_scheduler.is_none() || col_value.is_none() {
        bail!(
            "CSV {} missing required columns (need: mode, scheduler, value)",
            csv_path.display()
        );
    }

    for (row_idx, record) in reader.records().enumerate() {
        let record = record.with_context(|| {
            format!("Failed to parse row {} of {}", row_idx + 2, csv_path.display())
        })?;
        let line_num = row_idx + 2; // 1-based, line 1 = header

        // Get field helper
        let get = |col_opt: Option<usize>| -> &str {
            col_opt
                .and_then(|i| record.get(i))
                .unwrap_or("")
        };

        // Filter by thread_type if column exists
        if let Some(tt_col) = col_thread_type {
            let tt = record.get(tt_col).unwrap_or("");
            if !tt.is_empty() && tt != thread_type {
                continue;
            }
        }

        let sched = get(col_scheduler).to_string();
        let mode = get(col_mode).to_string();
        let rep = get(col_rep);
        let rep_id = if rep.is_empty() { "1" } else { rep }.to_string();

        if sched.is_empty() || mode.is_empty() {
            continue;
        }

        let key = (sched.clone(), mode.clone(), rep_id.clone());
        let rd = result
            .entry(key)
            .or_insert_with(|| RepData {
                rep_id: rep_id.clone(),
                ..Default::default()
            });

        // Parse value
        let val_str = get(col_value);
        let val: f64 = match val_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let metric = get(col_metric_name);
        let percentile = get(col_percentile);
        let notes = get(col_notes);

        match metric {
            "e2e_latency" => match percentile {
                "p50" => rd.e2e_p50.push(val, &rel_path, line_num),
                "p90" => rd.e2e_p90.push(val, &rel_path, line_num),
                "p99" => rd.e2e_p99.push(val, &rel_path, line_num),
                "p999" => rd.e2e_p999.push(val, &rel_path, line_num),
                _ => {}
            },
            "sched_latency" => match percentile {
                "p50" => rd.sched_p50.push(val, &rel_path, line_num),
                "p99" => rd.sched_p99.push(val, &rel_path, line_num),
                _ => {}
            },
            "irq_avoidance" | "irq_exposure" => {
                if notes.contains("runtime_weighted")
                    || notes.contains("time-weighted")
                    || metric == "irq_exposure"
                {
                    rd.irq_exposure.push(val, &rel_path, line_num);
                }
            }
            _ => {}
        }

        // CPU utilization from row-level field
        if let Some(cpu_col) = col_cpu_util {
            if let Some(cpu_str) = record.get(cpu_col) {
                if let Ok(cpu_val) = cpu_str.parse::<f64>() {
                    // Only record once per rep (avoid duplicates)
                    if rd.cpu_util.is_empty() || (rd.cpu_util.raw_values[0].0 - cpu_val).abs() > f64::EPSILON {
                        rd.cpu_util.raw_values = vec![(cpu_val, rel_path.clone(), line_num)];
                    }
                }
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Experiment discovery
// ---------------------------------------------------------------------------

fn discover_experiments(ws_root: &Path) -> Vec<(String, PathBuf)> {
    let exp_dir = ws_root.join("experiments");
    if !exp_dir.is_dir() {
        return Vec::new();
    }

    let mut experiments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&exp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                experiments.push((name, path));
            }
        }
    }
    experiments.sort_by(|a, b| a.0.cmp(&b.0));
    experiments
}

fn find_csv_files(experiment_dir: &Path) -> Vec<PathBuf> {
    let data_dir = experiment_dir.join("data");
    if !data_dir.is_dir() {
        return Vec::new();
    }

    // Prefer combined_results.csv
    let combined = data_dir.join("combined_results.csv");
    if combined.is_file() {
        return vec![combined];
    }

    // Fall back to individual metrics.csv files
    let mut csvs = Vec::new();
    fn walk(dir: &Path, csvs: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, csvs);
                } else if path.file_name().map_or(false, |f| f == "metrics.csv") {
                    csvs.push(path);
                }
            }
        }
    }
    walk(&data_dir, &mut csvs);
    csvs.sort();
    csvs
}

// ---------------------------------------------------------------------------
// Aggregation: median-rep selection
// ---------------------------------------------------------------------------

fn select_median_rep(reps: &HashMap<String, &RepData>) -> Option<(String, usize)> {
    // Score each rep by its E2E P99 mean
    let mut scored: Vec<(String, f64)> = Vec::new();
    for (rep_id, rd) in reps {
        if let Some(cv) = rd.e2e_p99.aggregate_mean() {
            scored.push((rep_id.clone(), cv.value));
        }
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let median_idx = scored.len() / 2;
    Some((scored[median_idx].0.clone(), median_idx))
}

fn build_table_rows(all_data: &RepMap, experiment_tag: &str) -> Vec<TableRow> {
    // Group by (scheduler, mode) → {rep_id: RepData}
    let mut groups: HashMap<(String, String), HashMap<String, &RepData>> = HashMap::new();
    for ((sched, mode, rep_id), rd) in all_data {
        groups
            .entry((sched.clone(), mode.clone()))
            .or_default()
            .insert(rep_id.clone(), rd);
    }

    let mut rows = Vec::new();
    for ((sched, mode), reps) in &groups {
        let selected = select_median_rep(reps);
        let (rep_id, rd, warnings) = if let Some((ref rep_id, _)) = selected {
            let rd = reps[rep_id];
            let mut warnings = Vec::new();

            // Cross-check: flag suspicious combinations
            if mode.contains("sim") && sched == "EEVDF" {
                warnings.push(
                    "⚠ Simulator claiming EEVDF results — verify scheduler setup".to_string(),
                );
            }

            (rep_id.clone(), Some(rd), warnings)
        } else {
            ("".to_string(), None, Vec::new())
        };

        let tr = if let Some(rd) = rd {
            TableRow {
                scheduler: sched.clone(),
                mode: mode.clone(),
                experiment: experiment_tag.to_string(),
                e2e_p50: rd.e2e_p50.aggregate_mean(),
                e2e_p90: rd.e2e_p90.aggregate_mean(),
                e2e_p99: rd.e2e_p99.aggregate_mean(),
                e2e_p999: rd.e2e_p999.aggregate_mean(),
                sched_p50: rd.sched_p50.aggregate_mean(),
                sched_p99: rd.sched_p99.aggregate_mean(),
                irq_exposure: rd.irq_exposure.aggregate_mean(),
                cpu_util: rd.cpu_util.aggregate_mean(),
                selected_rep: rep_id,
                warnings,
            }
        } else {
            TableRow {
                scheduler: sched.clone(),
                mode: mode.clone(),
                experiment: experiment_tag.to_string(),
                e2e_p50: None,
                e2e_p90: None,
                e2e_p99: None,
                e2e_p999: None,
                sched_p50: None,
                sched_p99: None,
                irq_exposure: None,
                cpu_util: None,
                selected_rep: String::new(),
                warnings,
            }
        };
        rows.push(tr);
    }

    // Sort: mode first, then scheduler
    rows.sort_by(|a, b| {
        let mode_cmp = mode_sort_key(&a.mode).cmp(&mode_sort_key(&b.mode));
        if mode_cmp != std::cmp::Ordering::Equal {
            mode_cmp
        } else {
            sched_sort_key(&a.scheduler).cmp(&sched_sort_key(&b.scheduler))
        }
    });

    rows
}

fn mode_sort_key(mode: &str) -> u32 {
    match mode {
        m if m.contains("production") => 0,
        m if m.contains("purerust") && m.contains("vm") => 1,
        m if m.contains("purerust") && m.contains("pinned") => 2,
        m if m.contains("purerust") && m.contains("floating") => 3,
        m if m.starts_with("purerust") => 4,
        m if m.contains("rtapp") && m.contains("pinned") => 5,
        m if m.contains("rtapp") && m.contains("floating") => 6,
        m if m.contains("rtapp") && m.contains("vm") => 7,
        m if m.contains("sim") => 8,
        _ => 99,
    }
}

fn sched_sort_key(sched: &str) -> u32 {
    match sched {
        "EEVDF" => 0,
        s if s.contains("LAVD") && !s.contains("IRQ") => 1,
        s if s.contains("LAVD") && s.contains("IRQ") => 2,
        "Tickless" => 3,
        _ => 99,
    }
}

// ---------------------------------------------------------------------------
// Cross-check validation
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CrossCheckResult {
    check: String,
    status: CrossCheckStatus,
    detail: String,
}

#[derive(Debug)]
enum CrossCheckStatus {
    Pass,
    Warn,
    Fail,
}

impl fmt::Display for CrossCheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "✓ PASS"),
            Self::Warn => write!(f, "⚠ WARN"),
            Self::Fail => write!(f, "✗ FAIL"),
        }
    }
}

fn run_cross_checks(rows: &[TableRow]) -> Vec<CrossCheckResult> {
    let mut results = Vec::new();

    // Check 1: E2E P99 should be > E2E P50
    for tr in rows {
        if let (Some(p50), Some(p99)) = (&tr.e2e_p50, &tr.e2e_p99) {
            if p99.value < p50.value {
                results.push(CrossCheckResult {
                    check: "P99 ≥ P50".into(),
                    status: CrossCheckStatus::Fail,
                    detail: format!(
                        "{}/{}: E2E P99 ({:.0}ns) < P50 ({:.0}ns)",
                        tr.mode, tr.scheduler, p99.value, p50.value
                    ),
                });
            }
        }
    }

    // Check 2: Scheduler latency should be ≤ E2E latency
    for tr in rows {
        if let (Some(sched_p99), Some(e2e_p99)) = (&tr.sched_p99, &tr.e2e_p99) {
            if sched_p99.value > e2e_p99.value * 1.1 {
                results.push(CrossCheckResult {
                    check: "sched ≤ e2e".into(),
                    status: CrossCheckStatus::Warn,
                    detail: format!(
                        "{}/{}: sched P99 ({:.0}ns) > e2e P99 ({:.0}ns) — sched latency should be a component of e2e",
                        tr.mode, tr.scheduler, sched_p99.value, e2e_p99.value
                    ),
                });
            }
        }
    }

    // Check 3: IRQ exposure should be 0-100%
    for tr in rows {
        if let Some(irq) = &tr.irq_exposure {
            if !(0.0..=100.0).contains(&irq.value) {
                results.push(CrossCheckResult {
                    check: "IRQ range".into(),
                    status: CrossCheckStatus::Fail,
                    detail: format!(
                        "{}/{}: IRQ exposure {:.1}% out of 0-100 range",
                        tr.mode, tr.scheduler, irq.value
                    ),
                });
            }
        }
    }

    // Check 4: Simulator should not claim EEVDF results
    for tr in rows {
        if tr.mode.contains("sim") && tr.scheduler == "EEVDF" {
            results.push(CrossCheckResult {
                check: "sim+EEVDF".into(),
                status: CrossCheckStatus::Warn,
                detail: format!(
                    "{}/{}: simulator mode claiming EEVDF results — verify scheduler setup",
                    tr.mode, tr.scheduler
                ),
            });
        }
    }

    // Check 5: CPU utilization sanity
    for tr in rows {
        if let Some(cpu) = &tr.cpu_util {
            if cpu.value < 0.1 {
                results.push(CrossCheckResult {
                    check: "CPU util".into(),
                    status: CrossCheckStatus::Warn,
                    detail: format!(
                        "{}/{}: CPU utilization suspiciously low ({:.1}%)",
                        tr.mode, tr.scheduler, cpu.value
                    ),
                });
            }
        }
    }

    // If no issues found, add a pass
    if results.is_empty() {
        results.push(CrossCheckResult {
            check: "all checks".into(),
            status: CrossCheckStatus::Pass,
            detail: format!("All {} cells passed validation", rows.len()),
        });
    }

    results
}

// ---------------------------------------------------------------------------
// Summary statistics
// ---------------------------------------------------------------------------

/// Compute summary statistics for a set of values.
fn compute_stats(values: &[f64]) -> BTreeMap<String, f64> {
    let mut stats = BTreeMap::new();
    if values.is_empty() {
        return stats;
    }

    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    stats.insert("mean".into(), mean);
    stats.insert("count".into(), n);

    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    stats.insert("min".into(), sorted[0]);
    stats.insert("max".into(), *sorted.last().unwrap());

    // Percentiles
    let p = |pct: f64| -> f64 {
        let idx = (pct / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    stats.insert("p50".into(), p(50.0));
    stats.insert("p90".into(), p(90.0));
    stats.insert("p99".into(), p(99.0));

    // Standard deviation
    if values.len() > 1 {
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        stats.insert("std_dev".into(), variance.sqrt());
    }

    stats
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_no_data() -> &'static str {
    "NO DATA"
}

fn format_markdown_table(rows: &[TableRow], thread_type: &str, show_citations: bool) -> String {
    let mut lines = Vec::new();

    lines.push(
        "| Mode | Scheduler | E2E P50 (µs) | E2E P99 (µs) | Sched P99 (µs) | IRQ Exp | CPU% | Rep |".to_string()
    );
    lines.push(
        "|:-----|:----------|:------------:|:------------:|:--------------:|:-------:|:----:|:---:|".to_string()
    );

    for tr in rows {
        let e2e_p50 = tr.e2e_p50.as_ref().map_or_else(|| format_no_data().into(), |v| v.format_us());
        let e2e_p99 = tr.e2e_p99.as_ref().map_or_else(|| format_no_data().into(), |v| v.format_us());
        let sched_p99 = tr.sched_p99.as_ref().map_or_else(|| format_no_data().into(), |v| v.format_us());
        let irq = tr.irq_exposure.as_ref().map_or_else(|| format_no_data().into(), |v| v.format_pct());
        let cpu = tr.cpu_util.as_ref().map_or_else(|| format_no_data().into(), |v| v.format_pct());

        lines.push(format!(
            "| {:<20} | {:<9} | {:>12} | {:>12} | {:>14} | {:>7} | {:>4} | {:>3} |",
            tr.mode, tr.scheduler, e2e_p50, e2e_p99, sched_p99, irq, cpu, tr.selected_rep
        ));

        for w in &tr.warnings {
            lines.push(format!("| | | | | | | | {} |", w));
        }
    }

    lines.push(String::new());
    lines.push(format!(
        "*Thread type: {thread_type}. Aggregation: median rep by E2E P99, \
         mean across threads within rep. Values in µs unless noted.*"
    ));

    if show_citations {
        lines.push(String::new());
        lines.push("### Source Citations".into());
        lines.push(String::new());
        for tr in rows {
            lines.push(format!(
                "**{} / {}** (experiment: {}, rep: {})",
                tr.mode, tr.scheduler, tr.experiment, tr.selected_rep
            ));
            let fields: &[(&str, &Option<CitedValue>)] = &[
                ("E2E P50", &tr.e2e_p50),
                ("E2E P90", &tr.e2e_p90),
                ("E2E P99", &tr.e2e_p99),
                ("E2E P999", &tr.e2e_p999),
                ("Sched P50", &tr.sched_p50),
                ("Sched P99", &tr.sched_p99),
                ("IRQ Exp", &tr.irq_exposure),
                ("CPU%", &tr.cpu_util),
            ];
            for (label, cv) in fields {
                if let Some(v) = cv {
                    lines.push(format!("  - {}: {:.2} {}", label, v.value, v.citation()));
                } else {
                    lines.push(format!("  - {}: NO DATA", label));
                }
            }
            lines.push(String::new());
        }
    }

    lines.join("\n")
}

fn format_csv_output(rows: &[TableRow], thread_type: &str) -> String {
    let mut lines = Vec::new();
    lines.push(
        "experiment,mode,scheduler,thread_type,rep,\
         e2e_p50_ns,e2e_p50_citation,\
         e2e_p99_ns,e2e_p99_citation,\
         sched_p99_ns,sched_p99_citation,\
         irq_exposure_pct,irq_citation,\
         cpu_util_pct,cpu_citation"
            .to_string(),
    );

    for tr in rows {
        let fmt = |cv: &Option<CitedValue>| -> (String, String) {
            match cv {
                Some(v) => (format!("{:.2}", v.value), v.citation()),
                None => ("NO DATA".into(), "NO DATA".into()),
            }
        };

        let (p50_v, p50_c) = fmt(&tr.e2e_p50);
        let (p99_v, p99_c) = fmt(&tr.e2e_p99);
        let (sp99_v, sp99_c) = fmt(&tr.sched_p99);
        let (irq_v, irq_c) = fmt(&tr.irq_exposure);
        let (cpu_v, cpu_c) = fmt(&tr.cpu_util);

        lines.push(format!(
            "{},{},{},{},{},\
             {},\"{}\",\
             {},\"{}\",\
             {},\"{}\",\
             {},\"{}\",\
             {},\"{}\"",
            tr.experiment, tr.mode, tr.scheduler, thread_type, tr.selected_rep,
            p50_v, p50_c,
            p99_v, p99_c,
            sp99_v, sp99_c,
            irq_v, irq_c,
            cpu_v, cpu_c,
        ));
    }

    lines.join("\n")
}

fn format_comparison_matrix(
    tables: &BTreeMap<String, Vec<TableRow>>,
    thread_type: &str,
) -> String {
    let experiments: Vec<&String> = tables.keys().collect();
    if experiments.len() <= 1 {
        return String::new();
    }

    // Collect all (mode, scheduler) combos
    let mut combos: BTreeSet<(String, String)> = BTreeSet::new();
    for rows in tables.values() {
        for tr in rows {
            combos.insert((tr.mode.clone(), tr.scheduler.clone()));
        }
    }

    let mut lines = Vec::new();
    lines.push(format!("## Cross-Experiment Comparison (E2E P99, µs)"));
    lines.push(format!(
        "*Thread type: {thread_type}. Values: E2E P99 in µs.*"
    ));
    lines.push(String::new());

    // Header
    let mut header = "| Mode | Scheduler |".to_string();
    let mut sep = "|:-----|:----------|".to_string();
    for exp in &experiments {
        header.push_str(&format!(" {} |", exp));
        sep.push_str(":----------:|");
    }
    lines.push(header);
    lines.push(sep);

    // Rows
    for (mode, sched) in &combos {
        let mut row = format!("| {:<20} | {:<9} |", mode, sched);
        for exp in &experiments {
            let val = tables[*exp]
                .iter()
                .find(|tr| tr.mode == *mode && tr.scheduler == *sched)
                .and_then(|tr| tr.e2e_p99.as_ref())
                .map_or_else(|| format_no_data().into(), |v| v.format_us());
            row.push_str(&format!(" {:>10} |", val));
        }
        lines.push(row);
    }

    lines.push(String::new());
    lines.join("\n")
}

fn format_data_coverage(tables: &BTreeMap<String, Vec<TableRow>>) -> String {
    let mut lines = Vec::new();
    lines.push("## Data Coverage Matrix".into());
    lines.push(String::new());

    for (exp, rows) in tables {
        let with_data = rows
            .iter()
            .filter(|tr| tr.e2e_p99.is_some())
            .count();
        let total = rows.len();
        lines.push(format!(
            "- **{}**: {}/{} cells with E2E P99 data",
            exp, with_data, total
        ));

        // List modes × schedulers
        for tr in rows {
            let status = if tr.e2e_p99.is_some() { "✓" } else { "✗" };
            lines.push(format!("  {} {} / {}", status, tr.mode, tr.scheduler));
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

fn format_cross_checks(checks: &[CrossCheckResult]) -> String {
    let mut lines = Vec::new();
    lines.push("## Cross-Check Validation".into());
    lines.push(String::new());

    for c in checks {
        lines.push(format!("- [{}] **{}**: {}", c.status, c.check, c.detail));
    }

    lines.push(String::new());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Main execute
// ---------------------------------------------------------------------------

pub fn execute(args: &AnalyzeArgs) -> Result<()> {
    let (ws_root, _config) = workspace::load_config_from_cwd()
        .context("repm analyze requires a workspace (run `repm init` first)")?;

    if let Some(ref versions) = args.compare {
        execute_comparison(&ws_root, versions, args)
    } else if let Some(ref version) = args.experiment {
        execute_single(&ws_root, version, args)
    } else {
        execute_auto_discover(&ws_root, args)
    }
}

fn execute_single(ws_root: &Path, version: &str, args: &AnalyzeArgs) -> Result<()> {
    let exp_dir = ws_root.join("experiments").join(version);
    if !exp_dir.is_dir() {
        bail!(
            "Experiment directory not found: {}\n\
             Available: {:?}",
            exp_dir.display(),
            discover_experiments(ws_root)
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
        );
    }

    let csv_files = find_csv_files(&exp_dir);
    if csv_files.is_empty() {
        bail!(
            "No CSV data files found in {}/data/\n\
             Expected: combined_results.csv or metrics.csv files",
            exp_dir.display()
        );
    }

    eprintln!(
        "repm analyze: {} ({} CSV files)",
        version,
        csv_files.len()
    );

    // Load all CSVs
    let mut all_data = RepMap::new();
    for csv_path in &csv_files {
        let data = load_csv_with_provenance(csv_path, ws_root, &args.thread_type)?;
        eprintln!(
            "  loaded {} from {} ({} groups)",
            csv_path.file_name().unwrap_or_default().to_string_lossy(),
            csv_path.parent().unwrap_or(csv_path).display(),
            data.len()
        );
        all_data.extend(data);
    }

    if all_data.is_empty() {
        bail!(
            "No data found for thread_type='{}'. Try --thread-type=all or check CSV contents.",
            args.thread_type
        );
    }

    let rows = build_table_rows(&all_data, version);
    eprintln!("  {} table rows generated", rows.len());

    // Generate output
    let mut output = String::new();
    output.push_str(&format!("# Experiment Results: {}\n\n", version));
    output.push_str(&format!(
        "Generated by `repm analyze` — all values traced to source CSVs.\n\n"
    ));

    match args.format {
        OutputFormat::Markdown => {
            output.push_str(&format_markdown_table(&rows, &args.thread_type, args.citations));
        }
        OutputFormat::Csv => {
            output.push_str(&format_csv_output(&rows, &args.thread_type));
        }
    }

    if args.cross_check {
        let checks = run_cross_checks(&rows);
        output.push_str("\n");
        output.push_str(&format_cross_checks(&checks));
    }

    // Output
    if args.write {
        let results_path = exp_dir.join("RESULTS.md");
        std::fs::write(&results_path, &output)
            .with_context(|| format!("Failed to write {}", results_path.display()))?;
        eprintln!("  wrote {}", results_path.display());
    } else {
        println!("{}", output);
    }

    Ok(())
}

fn execute_comparison(ws_root: &Path, versions: &[String], args: &AnalyzeArgs) -> Result<()> {
    eprintln!(
        "repm analyze --compare {} {}",
        versions[0], versions[1]
    );

    let mut all_tables: BTreeMap<String, Vec<TableRow>> = BTreeMap::new();
    for version in versions {
        let exp_dir = ws_root.join("experiments").join(version);
        if !exp_dir.is_dir() {
            bail!("Experiment directory not found: {}", exp_dir.display());
        }

        let csv_files = find_csv_files(&exp_dir);
        let mut data = RepMap::new();
        for csv_path in &csv_files {
            data.extend(load_csv_with_provenance(csv_path, ws_root, &args.thread_type)?);
        }

        let rows = build_table_rows(&data, version);
        eprintln!("  {}: {} rows", version, rows.len());
        all_tables.insert(version.clone(), rows);
    }

    let mut output = String::new();
    output.push_str(&format!(
        "# Comparison: {} vs {}\n\n",
        versions[0], versions[1]
    ));
    output.push_str("Generated by `repm analyze` — all values traced to source CSVs.\n\n");

    // Per-experiment tables
    for (version, rows) in &all_tables {
        output.push_str(&format!("## {}\n\n", version));
        output.push_str(&format_markdown_table(rows, &args.thread_type, args.citations));
        output.push_str("\n\n");
    }

    // Cross-experiment comparison matrix
    output.push_str(&format_comparison_matrix(&all_tables, &args.thread_type));

    // Data coverage
    output.push_str(&format_data_coverage(&all_tables));

    if args.cross_check {
        for (version, rows) in &all_tables {
            let checks = run_cross_checks(rows);
            output.push_str(&format!("\n### Cross-Checks: {}\n\n", version));
            output.push_str(&format_cross_checks(&checks));
        }
    }

    if args.write {
        let results_path = ws_root.join("experiments").join("COMPARISON.md");
        std::fs::write(&results_path, &output)
            .with_context(|| format!("Failed to write {}", results_path.display()))?;
        eprintln!("  wrote {}", results_path.display());
    } else {
        println!("{}", output);
    }

    Ok(())
}

fn execute_auto_discover(ws_root: &Path, args: &AnalyzeArgs) -> Result<()> {
    let experiments = discover_experiments(ws_root);
    if experiments.is_empty() {
        bail!(
            "No experiment directories found under {}/experiments/\n\
             Run `repm capture` or `repm run` first.",
            ws_root.display()
        );
    }

    eprintln!(
        "repm analyze: auto-discovered {} experiments",
        experiments.len()
    );

    let mut all_tables: BTreeMap<String, Vec<TableRow>> = BTreeMap::new();
    for (name, exp_dir) in &experiments {
        let csv_files = find_csv_files(exp_dir);
        if csv_files.is_empty() {
            eprintln!("  {}: no CSV data, skipping", name);
            continue;
        }

        let mut data = RepMap::new();
        for csv_path in &csv_files {
            match load_csv_with_provenance(csv_path, ws_root, &args.thread_type) {
                Ok(d) => data.extend(d),
                Err(e) => eprintln!("  warning: {}: {}", csv_path.display(), e),
            }
        }

        if !data.is_empty() {
            let rows = build_table_rows(&data, name);
            eprintln!("  {}: {} rows from {} CSVs", name, rows.len(), csv_files.len());
            all_tables.insert(name.clone(), rows);
        }
    }

    if all_tables.is_empty() {
        bail!("No experiment data found for thread_type='{}'", args.thread_type);
    }

    let mut output = String::new();
    output.push_str("# Experiment Results\n\n");
    output.push_str("Generated by `repm analyze` — all values traced to source CSVs.\n\n");

    for (name, rows) in &all_tables {
        output.push_str(&format!("## {}\n\n", name));
        output.push_str(&format_markdown_table(rows, &args.thread_type, args.citations));
        output.push_str("\n\n");
    }

    if all_tables.len() > 1 {
        output.push_str(&format_comparison_matrix(&all_tables, &args.thread_type));
        output.push_str(&format_data_coverage(&all_tables));
    }

    if args.cross_check {
        for (name, rows) in &all_tables {
            let checks = run_cross_checks(rows);
            output.push_str(&format!("\n### Cross-Checks: {}\n\n", name));
            output.push_str(&format_cross_checks(&checks));
        }
    }

    if args.write {
        let results_path = ws_root.join("experiments").join("RESULTS.md");
        std::fs::write(&results_path, &output)
            .with_context(|| format!("Failed to write {}", results_path.display()))?;
        eprintln!("  wrote {}", results_path.display());
    } else {
        println!("{}", output);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cell(vals: &[(f64, &str, usize)]) -> CellData {
        CellData {
            raw_values: vals
                .iter()
                .map(|(v, f, l)| (*v, f.to_string(), *l))
                .collect(),
        }
    }

    #[test]
    fn test_cell_aggregate_mean_single() {
        let cell = make_cell(&[(1000.0, "test.csv", 5)]);
        let cv = cell.aggregate_mean().unwrap();
        assert_eq!(cv.value, 1000.0);
        assert_eq!(cv.computation, "direct");
        assert_eq!(cv.source_lines, vec![5]);
    }

    #[test]
    fn test_cell_aggregate_mean_multiple() {
        let cell = make_cell(&[
            (100.0, "test.csv", 2),
            (200.0, "test.csv", 3),
            (300.0, "test.csv", 4),
        ]);
        let cv = cell.aggregate_mean().unwrap();
        assert!((cv.value - 200.0).abs() < f64::EPSILON);
        assert_eq!(cv.computation, "mean(n=3)");
        assert_eq!(cv.source_lines, vec![2, 3, 4]);
    }

    #[test]
    fn test_cell_aggregate_empty() {
        let cell = CellData::default();
        assert!(cell.aggregate_mean().is_none());
    }

    #[test]
    fn test_cited_value_format_us() {
        let cv = CitedValue {
            value: 500.0, // 500ns = 0.50µs
            source_file: "test.csv".into(),
            source_lines: vec![1],
            computation: "direct".into(),
        };
        assert_eq!(cv.format_us(), "0.50");

        let cv2 = CitedValue {
            value: 50_000.0, // 50µs
            source_file: "test.csv".into(),
            source_lines: vec![1],
            computation: "direct".into(),
        };
        assert_eq!(cv2.format_us(), "50.0");

        let cv3 = CitedValue {
            value: 500_000.0, // 500µs
            source_file: "test.csv".into(),
            source_lines: vec![1],
            computation: "direct".into(),
        };
        assert_eq!(cv3.format_us(), "500");
    }

    #[test]
    fn test_cited_value_format_pct() {
        let cv = CitedValue {
            value: 42.5,
            source_file: "test.csv".into(),
            source_lines: vec![1],
            computation: "direct".into(),
        };
        assert_eq!(cv.format_pct(), "42.5%");
    }

    #[test]
    fn test_citation_short_lines() {
        let cv = CitedValue {
            value: 100.0,
            source_file: "data/metrics.csv".into(),
            source_lines: vec![5, 10],
            computation: "mean(n=2)".into(),
        };
        assert_eq!(cv.citation(), "[data/metrics.csv:5,10 (mean(n=2))]");
    }

    #[test]
    fn test_citation_long_lines() {
        let cv = CitedValue {
            value: 100.0,
            source_file: "data/metrics.csv".into(),
            source_lines: vec![2, 5, 8, 11],
            computation: "mean(n=4)".into(),
        };
        assert_eq!(cv.citation(), "[data/metrics.csv:2..11 (mean(n=4))]");
    }

    #[test]
    fn test_compute_stats() {
        let vals = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        let stats = compute_stats(&vals);
        assert_eq!(stats["mean"], 30.0);
        assert_eq!(stats["min"], 10.0);
        assert_eq!(stats["max"], 50.0);
        assert_eq!(stats["p50"], 30.0);
        assert_eq!(stats["count"], 5.0);
        assert!(stats.contains_key("std_dev"));
    }

    #[test]
    fn test_compute_stats_empty() {
        let stats = compute_stats(&[]);
        assert!(stats.is_empty());
    }

    #[test]
    fn test_compute_stats_single() {
        let stats = compute_stats(&[42.0]);
        assert_eq!(stats["mean"], 42.0);
        assert_eq!(stats["p50"], 42.0);
        assert!(!stats.contains_key("std_dev")); // need n>1
    }

    #[test]
    fn test_cross_check_p99_lt_p50() {
        let rows = vec![TableRow {
            scheduler: "EEVDF".into(),
            mode: "rtapp_pinned".into(),
            experiment: "test".into(),
            e2e_p50: Some(CitedValue {
                value: 1000.0,
                source_file: "t.csv".into(),
                source_lines: vec![1],
                computation: "direct".into(),
            }),
            e2e_p99: Some(CitedValue {
                value: 500.0, // P99 < P50 — bad!
                source_file: "t.csv".into(),
                source_lines: vec![2],
                computation: "direct".into(),
            }),
            e2e_p90: None,
            e2e_p999: None,
            sched_p50: None,
            sched_p99: None,
            irq_exposure: None,
            cpu_util: None,
            selected_rep: "1".into(),
            warnings: vec![],
        }];
        let checks = run_cross_checks(&rows);
        assert!(checks.iter().any(|c| matches!(c.status, CrossCheckStatus::Fail)));
    }

    #[test]
    fn test_cross_check_sim_eevdf_warning() {
        let rows = vec![TableRow {
            scheduler: "EEVDF".into(),
            mode: "rtapp_sim".into(),
            experiment: "test".into(),
            e2e_p50: None,
            e2e_p90: None,
            e2e_p99: None,
            e2e_p999: None,
            sched_p50: None,
            sched_p99: None,
            irq_exposure: None,
            cpu_util: None,
            selected_rep: "1".into(),
            warnings: vec![],
        }];
        let checks = run_cross_checks(&rows);
        assert!(checks.iter().any(|c| c.check == "sim+EEVDF"));
    }

    #[test]
    fn test_mode_sort_key() {
        assert!(mode_sort_key("production_traces") < mode_sort_key("rtapp_pinned"));
        assert!(mode_sort_key("purerust_vm") < mode_sort_key("rtapp_vm"));
        assert!(mode_sort_key("rtapp_pinned") < mode_sort_key("simulator"));
    }

    #[test]
    fn test_sched_sort_key() {
        assert!(sched_sort_key("EEVDF") < sched_sort_key("LAVD"));
        assert!(sched_sort_key("LAVD") < sched_sort_key("LAVD+IRQ"));
    }

    #[test]
    fn test_build_table_rows_empty() {
        let data = RepMap::new();
        let rows = build_table_rows(&data, "test");
        assert!(rows.is_empty());
    }

    #[test]
    fn test_build_table_rows_single_rep() {
        let mut data = RepMap::new();
        let mut rd = RepData {
            rep_id: "1".into(),
            ..Default::default()
        };
        rd.e2e_p50.push(1000.0, "test.csv", 2);
        rd.e2e_p99.push(5000.0, "test.csv", 3);
        data.insert(("EEVDF".into(), "rtapp_pinned".into(), "1".into()), rd);

        let rows = build_table_rows(&data, "exp1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scheduler, "EEVDF");
        assert_eq!(rows[0].mode, "rtapp_pinned");
        assert_eq!(rows[0].selected_rep, "1");
        assert!(rows[0].e2e_p50.is_some());
        assert!(rows[0].e2e_p99.is_some());
    }

    #[test]
    fn test_build_table_rows_median_rep_selection() {
        let mut data = RepMap::new();

        // Rep 1: P99 = 1000
        let mut rd1 = RepData { rep_id: "1".into(), ..Default::default() };
        rd1.e2e_p99.push(1000.0, "t.csv", 2);
        data.insert(("EEVDF".into(), "mode".into(), "1".into()), rd1);

        // Rep 2: P99 = 5000 (highest)
        let mut rd2 = RepData { rep_id: "2".into(), ..Default::default() };
        rd2.e2e_p99.push(5000.0, "t.csv", 3);
        data.insert(("EEVDF".into(), "mode".into(), "2".into()), rd2);

        // Rep 3: P99 = 3000 (median)
        let mut rd3 = RepData { rep_id: "3".into(), ..Default::default() };
        rd3.e2e_p99.push(3000.0, "t.csv", 4);
        data.insert(("EEVDF".into(), "mode".into(), "3".into()), rd3);

        let rows = build_table_rows(&data, "exp1");
        assert_eq!(rows.len(), 1);
        // Median of [1000, 3000, 5000] → index 1 → rep with 3000
        assert_eq!(rows[0].selected_rep, "3");
    }

    #[test]
    fn test_format_markdown_basic() {
        let rows = vec![TableRow {
            scheduler: "EEVDF".into(),
            mode: "rtapp_pinned".into(),
            experiment: "test".into(),
            e2e_p50: Some(CitedValue {
                value: 50_000.0,
                source_file: "t.csv".into(),
                source_lines: vec![2],
                computation: "direct".into(),
            }),
            e2e_p90: None,
            e2e_p99: Some(CitedValue {
                value: 200_000.0,
                source_file: "t.csv".into(),
                source_lines: vec![3],
                computation: "direct".into(),
            }),
            e2e_p999: None,
            sched_p50: None,
            sched_p99: None,
            irq_exposure: None,
            cpu_util: None,
            selected_rep: "1".into(),
            warnings: vec![],
        }];
        let md = format_markdown_table(&rows, "foreground", false);
        assert!(md.contains("EEVDF"));
        assert!(md.contains("rtapp_pinned"));
        assert!(md.contains("50.0"));  // 50_000ns = 50µs
        assert!(md.contains("200"));   // 200_000ns = 200µs
        assert!(md.contains("NO DATA")); // sched_p99 is None
        assert!(!md.contains("Source Citations")); // no citations
    }

    #[test]
    fn test_format_markdown_with_citations() {
        let rows = vec![TableRow {
            scheduler: "LAVD".into(),
            mode: "rtapp_vm".into(),
            experiment: "v1".into(),
            e2e_p50: Some(CitedValue {
                value: 30_000.0,
                source_file: "data.csv".into(),
                source_lines: vec![5],
                computation: "direct".into(),
            }),
            e2e_p90: None,
            e2e_p99: None,
            e2e_p999: None,
            sched_p50: None,
            sched_p99: None,
            irq_exposure: None,
            cpu_util: None,
            selected_rep: "2".into(),
            warnings: vec![],
        }];
        let md = format_markdown_table(&rows, "foreground", true);
        assert!(md.contains("Source Citations"));
        assert!(md.contains("data.csv:5"));
        assert!(md.contains("direct"));
    }

    #[test]
    fn test_format_csv_output() {
        let rows = vec![TableRow {
            scheduler: "EEVDF".into(),
            mode: "rtapp_pinned".into(),
            experiment: "v1".into(),
            e2e_p50: Some(CitedValue {
                value: 1000.0,
                source_file: "t.csv".into(),
                source_lines: vec![2],
                computation: "direct".into(),
            }),
            e2e_p90: None,
            e2e_p99: None,
            e2e_p999: None,
            sched_p50: None,
            sched_p99: None,
            irq_exposure: None,
            cpu_util: None,
            selected_rep: "1".into(),
            warnings: vec![],
        }];
        let csv = format_csv_output(&rows, "foreground");
        assert!(csv.contains("e2e_p50_ns"));
        assert!(csv.contains("1000.00"));
        assert!(csv.contains("NO DATA")); // missing fields
    }

    #[test]
    fn test_load_csv_with_provenance() {
        let csv_content = "\
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
2026-04-14,rtapp_pinned,EEVDF,baseline,foreground,0,e2e_latency,p50,50000.0,ns,1000,1,,85.0
2026-04-14,rtapp_pinned,EEVDF,baseline,foreground,0,e2e_latency,p99,200000.0,ns,1000,1,,85.0
2026-04-14,rtapp_pinned,EEVDF,baseline,foreground,1,e2e_latency,p50,55000.0,ns,1000,1,,85.0
2026-04-14,rtapp_pinned,EEVDF,baseline,foreground,1,e2e_latency,p99,210000.0,ns,1000,1,,85.0
2026-04-14,rtapp_pinned,EEVDF,baseline,background,0,e2e_latency,p50,100000.0,ns,1000,1,,85.0
";
        // Write to temp file
        let dir = std::env::temp_dir().join("repm_test_csv");
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("test_metrics.csv");
        std::fs::write(&csv_path, csv_content).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        // Should have 1 group: (EEVDF, rtapp_pinned, 1)
        assert_eq!(data.len(), 1);
        let key = ("EEVDF".into(), "rtapp_pinned".into(), "1".into());
        let rd = &data[&key];
        // P50: two values (thread 0 and thread 1)
        assert_eq!(rd.e2e_p50.raw_values.len(), 2);
        assert_eq!(rd.e2e_p99.raw_values.len(), 2);
        // CPU util
        assert!(!rd.cpu_util.is_empty());

        // Background thread should be filtered out
        let bg_key = ("EEVDF".into(), "rtapp_pinned".into(), "1".into());
        let rd = &data[&bg_key];
        // Only foreground data
        assert_eq!(rd.e2e_p50.raw_values.len(), 2); // 2 foreground threads

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // Integration tests: run.rs CSV output → analyze.rs parsing
    //
    // These tests construct CSV data in the EXACT format that run.rs produces
    // and verify that analyze.rs can parse, aggregate, and cross-check it.
    // This is the test gap identified by the skeptic review.
    // =======================================================================

    /// The canonical 14-column CSV header that both rtapp and scxsim paths
    /// in run.rs must produce. Defined once here as the contract.
    const CANONICAL_CSV_HEADER: &str =
        "timestamp,mode,scheduler,condition,thread_type,thread_id,\
         metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct";

    /// Integration: rtapp CSV with sched_latency metric is parseable by analyze.
    /// This is the exact format collect_rtapp_metrics() produces after the C1/C2 fix.
    #[test]
    fn test_integration_rtapp_csv_sched_latency_parsed() {
        let dir = std::env::temp_dir().join("repm_integ_rtapp_sched");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Construct CSV in exact run.rs rtapp format (14 columns, sched_latency metric)
        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,foreground,fg_thread_0,sched_latency,p50,95.50,us,800,1,rtapp_slack,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,foreground,fg_thread_0,sched_latency,p99,320.00,us,800,1,rtapp_slack,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,foreground,fg_thread_0,sched_latency,avg,110.25,us,800,1,rtapp_slack,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,background,bg_hog_0,sched_latency,p50,50.00,us,800,1,rtapp_slack,\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        // Parse with analyze's loader — filtering on foreground
        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();

        // Should find exactly one group: (eevdf, rtapp_pinned, 1)
        assert_eq!(data.len(), 1, "Expected 1 group, got {}", data.len());
        let key = ("eevdf".into(), "rtapp_pinned".into(), "1".into());
        let rd = &data[&key];

        // sched_latency p50 and p99 must be populated
        assert!(
            !rd.sched_p50.is_empty(),
            "sched_p50 must be parsed from 'sched_latency' metric"
        );
        assert!(
            !rd.sched_p99.is_empty(),
            "sched_p99 must be parsed from 'sched_latency' metric"
        );
        assert_eq!(rd.sched_p50.raw_values[0].0, 95.50);
        assert_eq!(rd.sched_p99.raw_values[0].0, 320.00);

        // Background thread must be filtered out
        // (only 1 group, all foreground)

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: scxsim CSV with both e2e_latency and sched_latency metrics.
    /// Verifies analyze can handle the full scxsim output format.
    #[test]
    fn test_integration_scxsim_csv_dual_metrics() {
        let dir = std::env::temp_dir().join("repm_integ_scxsim");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_sim,lavd_v1,baseline,foreground,fg_thread_0,e2e_latency,p50,50000.0,ns,1000,1,scxsim_inter_arrival,85.2\n\
             2026-04-17T10:00:00Z,rtapp_sim,lavd_v1,baseline,foreground,fg_thread_0,e2e_latency,p99,200000.0,ns,1000,1,scxsim_inter_arrival,85.2\n\
             2026-04-17T10:00:00Z,rtapp_sim,lavd_v1,baseline,foreground,fg_thread_0,sched_latency,p50,12000.0,ns,1000,1,scxsim_summary,85.2\n\
             2026-04-17T10:00:00Z,rtapp_sim,lavd_v1,baseline,foreground,fg_thread_0,sched_latency,p99,80000.0,ns,1000,1,scxsim_summary,85.2\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        assert_eq!(data.len(), 1);

        let key = ("lavd_v1".into(), "rtapp_sim".into(), "1".into());
        let rd = &data[&key];

        // Both e2e and sched latency must be parsed
        assert!(!rd.e2e_p50.is_empty(), "e2e_p50 must be parsed");
        assert!(!rd.e2e_p99.is_empty(), "e2e_p99 must be parsed");
        assert!(!rd.sched_p50.is_empty(), "sched_p50 must be parsed");
        assert!(!rd.sched_p99.is_empty(), "sched_p99 must be parsed");

        // Values in nanoseconds
        assert_eq!(rd.e2e_p50.raw_values[0].0, 50000.0);
        assert_eq!(rd.e2e_p99.raw_values[0].0, 200000.0);
        assert_eq!(rd.sched_p50.raw_values[0].0, 12000.0);
        assert_eq!(rd.sched_p99.raw_values[0].0, 80000.0);

        // CPU utilization must be parsed from the row-level field
        assert!(!rd.cpu_util.is_empty(), "cpu_util must be parsed from avg_cpu_util_pct column");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: multi-rep CSV produces correct median-rep selection and
    /// table rows through the full analyze pipeline.
    #[test]
    fn test_integration_multi_rep_median_selection() {
        let dir = std::env::temp_dir().join("repm_integ_multirep");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 3 reps with different e2e P99 values: 100k, 300k, 200k
        // Median should select rep 3 (200k is middle)
        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p50,40000.0,ns,500,1,test,80.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,100000.0,ns,500,1,test,80.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p50,45000.0,ns,500,2,test,80.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,300000.0,ns,500,2,test,80.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p50,42000.0,ns,500,3,test,80.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,200000.0,ns,500,3,test,80.0\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        assert_eq!(data.len(), 3, "Should have 3 groups (one per rep)");

        // Build table rows and verify median selection
        let rows = build_table_rows(&data, "test_exp");
        assert_eq!(rows.len(), 1, "3 reps of same scheduler+mode → 1 table row");
        assert_eq!(rows[0].scheduler, "EEVDF");
        assert_eq!(rows[0].mode, "rtapp_pinned");

        // Median of [100k, 200k, 300k] → rep with 200k → rep 3
        assert_eq!(
            rows[0].selected_rep, "3",
            "Median rep should be '3' (P99=200k is middle of [100k,200k,300k])"
        );

        // The selected rep's P99 should be 200000
        let p99_val = rows[0].e2e_p99.as_ref().unwrap().value;
        assert!(
            (p99_val - 200000.0).abs() < f64::EPSILON,
            "Selected rep P99 should be 200000, got {}",
            p99_val
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: multi-scheduler CSV produces sorted table rows with
    /// correct mode×scheduler ordering.
    #[test]
    fn test_integration_multi_scheduler_table_ordering() {
        let dir = std::env::temp_dir().join("repm_integ_multisched");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,LAVD+IRQ,baseline,foreground,fg0,e2e_latency,p99,150000.0,ns,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,200000.0,ns,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,LAVD,baseline,foreground,fg0,e2e_latency,p99,180000.0,ns,500,1,test,\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        let rows = build_table_rows(&data, "test");

        assert_eq!(rows.len(), 3);
        // sched_sort_key: EEVDF=0, LAVD=1, LAVD+IRQ=2
        assert_eq!(rows[0].scheduler, "EEVDF", "EEVDF should sort first");
        assert_eq!(rows[1].scheduler, "LAVD", "LAVD should sort second");
        assert_eq!(rows[2].scheduler, "LAVD+IRQ", "LAVD+IRQ should sort third");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: cross-check catches sim×EEVDF violation in CSV data
    /// produced by the pipeline.
    #[test]
    fn test_integration_cross_check_catches_sim_eevdf() {
        let dir = std::env::temp_dir().join("repm_integ_crosscheck");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // This CSV should never exist in practice (sim + EEVDF is impossible)
        // but the cross-check must catch it
        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_sim,EEVDF,baseline,foreground,fg0,e2e_latency,p99,200000.0,ns,500,1,test,\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        let rows = build_table_rows(&data, "test");
        let checks = run_cross_checks(&rows);

        // Must flag the sim+EEVDF combination
        assert!(
            checks.iter().any(|c| c.check == "sim+EEVDF"),
            "Cross-check must flag rtapp_sim × EEVDF. Checks: {:?}",
            checks.iter().map(|c| &c.check).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: CSV with mixed thread types correctly filters to foreground only.
    #[test]
    fn test_integration_thread_type_filtering() {
        let dir = std::env::temp_dir().join("repm_integ_filter");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,foreground,fg_0,sched_latency,p99,100.0,us,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,background,bg_0,sched_latency,p99,50.0,us,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,irq_generator,irq_0,sched_latency,p99,200.0,us,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,eevdf,baseline,foreground,fg_1,sched_latency,p99,110.0,us,500,1,test,\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        // Filter on foreground
        let fg_data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        let key = ("eevdf".into(), "rtapp_pinned".into(), "1".into());
        let rd = &fg_data[&key];
        // Should have 2 foreground entries (fg_0 and fg_1), not bg or irq
        assert_eq!(
            rd.sched_p99.raw_values.len(),
            2,
            "Should have 2 foreground sched_p99 values, got {}",
            rd.sched_p99.raw_values.len()
        );

        // Filter on background
        let bg_data = load_csv_with_provenance(&csv_path, &dir, "background").unwrap();
        let rd_bg = &bg_data[&key];
        assert_eq!(
            rd_bg.sched_p99.raw_values.len(),
            1,
            "Should have 1 background sched_p99 value"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: markdown output from analyze contains correct values
    /// when fed CSV in run.rs format.
    #[test]
    fn test_integration_markdown_output_from_run_csv() {
        let dir = std::env::temp_dir().join("repm_integ_markdown");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let csv = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p50,50000.0,ns,500,1,test,85.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,200000.0,ns,500,1,test,85.0\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,sched_latency,p99,80000.0,ns,500,1,test,85.0\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        let rows = build_table_rows(&data, "v001");
        let md = format_markdown_table(&rows, "foreground", true);

        // Must contain the scheduler name
        assert!(md.contains("EEVDF"), "Markdown must contain scheduler name");
        // Must contain the mode
        assert!(md.contains("rtapp_pinned"), "Markdown must contain mode");
        // Must contain formatted values (ns → µs conversion)
        // 50000ns = 50µs → "50.0"
        assert!(md.contains("50.0"), "Markdown must contain E2E P50 value (50.0µs)");
        // 200000ns = 200µs → "200"
        assert!(md.contains("200"), "Markdown must contain E2E P99 value (200µs)");
        // Must contain source citations section
        assert!(md.contains("Source Citations"), "Markdown with citations=true must have citations");
        assert!(md.contains("metrics.csv"), "Citations must reference source file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration: CSV output mode produces valid CSV with citations.
    #[test]
    fn test_integration_csv_output_from_run_csv() {
        let dir = std::env::temp_dir().join("repm_integ_csvout");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let csv_input = format!(
            "{}\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p50,50000.0,ns,500,1,test,\n\
             2026-04-17T10:00:00Z,rtapp_pinned,EEVDF,baseline,foreground,fg0,e2e_latency,p99,200000.0,ns,500,1,test,\n",
            CANONICAL_CSV_HEADER
        );

        let csv_path = dir.join("metrics.csv");
        std::fs::write(&csv_path, &csv_input).unwrap();

        let data = load_csv_with_provenance(&csv_path, &dir, "foreground").unwrap();
        let rows = build_table_rows(&data, "v001");
        let csv_out = format_csv_output(&rows, "foreground");

        // Must have a header row
        let lines: Vec<&str> = csv_out.lines().collect();
        assert!(lines.len() >= 2, "CSV output must have header + data");
        assert!(
            lines[0].contains("e2e_p50_ns"),
            "CSV header must contain e2e_p50_ns"
        );
        assert!(
            lines[0].contains("e2e_p99_ns"),
            "CSV header must contain e2e_p99_ns"
        );
        // Data row must contain the values
        assert!(
            lines[1].contains("50000.00"),
            "CSV data must contain P50 value 50000.00"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
