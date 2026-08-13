//! Trace ingestion — parse scheduling data into structured events and profiles.
//!
//! This module is the bridge between raw experiment data (rt-app logs, metrics
//! CSVs, Perfetto traces) and the synthesis pipeline. It extracts per-thread
//! scheduling characteristics that drive workload generation.
//!
//! Supported input formats:
//! - **rt-app log files**: Per-thread CSVs with columns: idx, perf, run, period,
//!   start, end, rel_st, slack, c_duration, c_period, wu_lat, cpu
//! - **Metrics CSV**: The 14-column canonical format from `repm run`/`repm analyze`
//! - **Perfetto JSON**: Chrome trace format with sched_switch/sched_waking events

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{bail, Context, Result};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single scheduling event extracted from trace data.
#[derive(Debug, Clone)]
pub struct SchedulingEvent {
    /// Absolute timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// Event type.
    pub event_type: EventType,
    /// CPU where the event occurred.
    pub cpu: u32,
    /// Process/thread ID.
    pub pid: u32,
    /// Thread name (e.g., "cache_worker_0", "bg_hog_3").
    pub thread_name: String,
    /// Duration in nanoseconds (for run/sleep phases).
    pub duration_ns: Option<u64>,
}

/// Scheduling event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// Thread was scheduled onto a CPU (start of run phase).
    SchedSwitch,
    /// Thread was woken up (end of sleep, start of wait).
    SchedWaking,
    /// Thread completed a compute phase.
    RunComplete,
    /// Thread entered sleep.
    SleepStart,
    /// IRQ or softirq event.
    Irq,
}

/// Per-thread scheduling profile summarizing behavior over the trace.
#[derive(Debug, Clone)]
pub struct ThreadProfile {
    /// Thread name (from rt-app log filename or CSV thread_id).
    pub name: String,
    /// Thread role classification.
    pub role: ThreadRole,
    /// Total runtime across all compute phases (nanoseconds).
    pub total_run_ns: u64,
    /// Total sleep time across all sleep phases (nanoseconds).
    pub total_sleep_ns: u64,
    /// Total scheduling latency (wait time between wakeup and running).
    pub total_sched_latency_ns: u64,
    /// Number of completed run+sleep iterations.
    pub iteration_count: u64,
    /// Set of CPUs this thread ran on.
    pub cpu_set: BTreeSet<u32>,
    /// Average compute burst duration (nanoseconds).
    pub avg_run_ns: f64,
    /// Average sleep duration (nanoseconds).
    pub avg_sleep_ns: f64,
    /// Average scheduling latency (nanoseconds).
    pub avg_sched_latency_ns: f64,
    /// Scheduling latency percentiles: p50, p90, p99, p999.
    pub sched_latency_percentiles: LatencyPercentiles,
    /// E2E latency percentiles (run + sleep + sched_latency per iteration).
    pub e2e_latency_percentiles: LatencyPercentiles,
}

/// Latency percentile summary.
#[derive(Debug, Clone, Default)]
pub struct LatencyPercentiles {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
}

/// Thread role classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRole {
    Foreground,
    Background,
    IrqGenerator,
    Unknown,
}

impl ThreadRole {
    /// Classify a thread by its name.
    pub fn from_name(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.contains("cache_worker") || lower.contains("fg_") || lower.contains("foreground") {
            Self::Foreground
        } else if lower.contains("hog") || lower.contains("bg_") || lower.contains("background") {
            Self::Background
        } else if lower.contains("irq") {
            Self::IrqGenerator
        } else {
            Self::Unknown
        }
    }

    /// String label for CSV output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
            Self::IrqGenerator => "irq_generator",
            Self::Unknown => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// rt-app log parser
// ---------------------------------------------------------------------------

/// A single row from an rt-app per-thread log file.
#[derive(Debug, Clone)]
struct RtAppLogRow {
    #[allow(dead_code)]
    idx: u32,
    run_us: u64,
    period_us: u64,
    start_ns: u64,
    end_ns: u64,
    slack_us: i64,
    wu_lat_us: u64,
    cpu: u32,
}

/// Parse a single rt-app log file into a ThreadProfile.
///
/// rt-app log format (space-separated, first line is `#`-prefixed header):
/// ```text
/// #idx  perf  run  period  start  end  rel_st  slack  c_duration  c_period  wu_lat  cpu
///    0     0  520     520  <ns>   <ns>  <ns>       0         500         0       0    9
/// ```
///
/// Key columns:
/// - `run`: actual runtime in microseconds for this phase
/// - `period`: total phase duration (run + overhead) in microseconds
/// - `start`/`end`: absolute timestamps in nanoseconds
/// - `slack`: scheduling slack in microseconds (negative = overrun)
/// - `wu_lat`: wakeup latency in microseconds
/// - `cpu`: CPU where this phase ran
pub fn parse_rtapp_log(path: &Path) -> Result<(Vec<SchedulingEvent>, ThreadProfile)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read rt-app log: {}", path.display()))?;

    // Extract thread name from filename: "basename-thread_name-tid.log"
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let thread_name = extract_thread_name(&filename);
    let role = ThreadRole::from_name(&thread_name);

    let mut rows = Vec::new();
    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(row) = parse_rtapp_log_line(line) {
            rows.push(row);
        }
    }

    if rows.is_empty() {
        bail!("No data rows in rt-app log: {}", path.display());
    }

    // Build events — only for compute phases (run_us > 0)
    let mut events = Vec::new();
    for row in &rows {
        if row.run_us > 0 {
            events.push(SchedulingEvent {
                timestamp_ns: row.start_ns,
                event_type: EventType::RunComplete,
                cpu: row.cpu,
                pid: 0,
                thread_name: thread_name.clone(),
                duration_ns: Some(row.run_us * 1000),
            });
        }
    }

    // Build profile
    let profile = build_profile_from_rows(&thread_name, role, &rows);

    Ok((events, profile))
}

/// Parse all rt-app log files in a directory into ThreadProfiles.
pub fn parse_rtapp_log_dir(dir: &Path) -> Result<Vec<ThreadProfile>> {
    let mut profiles = Vec::new();

    if !dir.exists() {
        bail!("Log directory not found: {}", dir.display());
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "log") {
            match parse_rtapp_log(&path) {
                Ok((_events, profile)) => profiles.push(profile),
                Err(e) => {
                    eprintln!(
                        "  warning: skipping {}: {}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        e
                    );
                }
            }
        }
    }

    if profiles.is_empty() {
        bail!("No rt-app logs found in {}", dir.display());
    }

    // Sort by role then name for stable output
    profiles.sort_by(|a, b| (a.role.as_str(), &a.name).cmp(&(b.role.as_str(), &b.name)));

    Ok(profiles)
}

fn parse_rtapp_log_line(line: &str) -> Option<RtAppLogRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 12 {
        return None;
    }

    Some(RtAppLogRow {
        idx: fields[0].parse().ok()?,
        run_us: fields[2].parse().ok()?,
        period_us: fields[3].parse().ok()?,
        start_ns: fields[4].parse().ok()?,
        end_ns: fields[5].parse().ok()?,
        slack_us: fields[7].parse().ok()?,
        wu_lat_us: fields[10].parse().ok()?,
        cpu: fields[11].parse().ok()?,
    })
}

/// Extract thread name from rt-app log filename.
/// Format: `<basename>-<thread_name>-<tid>.log`
/// Example: `cache_prodscale-cache_worker_0-0.log` → `cache_worker_0`
/// Example: `repromagic-fg_thread_0-1234.log` → `fg_thread_0`
fn extract_thread_name(filename: &str) -> String {
    let stem = filename.strip_suffix(".log").unwrap_or(filename);
    // Split on `-` and find the thread name part.
    // Convention: basename-threadname-tid
    // The tid is the last segment, basename is the first.
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 3 {
        // Thread name is everything between first and last segment
        parts[1..parts.len() - 1].join("-")
    } else if parts.len() == 2 {
        parts[1].to_string()
    } else {
        stem.to_string()
    }
}

fn build_profile_from_rows(name: &str, role: ThreadRole, rows: &[RtAppLogRow]) -> ThreadProfile {
    let mut total_run_ns: u64 = 0;
    let mut total_sleep_ns: u64 = 0;
    let mut total_sched_latency_ns: u64 = 0;
    let mut cpus = BTreeSet::new();
    let mut sched_latencies_ns = Vec::new();
    let mut e2e_latencies_ns = Vec::new();

    // rt-app logs alternate between compute and sleep phases.
    // Each row is one phase. We pair consecutive rows to get
    // run+sleep iterations. The `period` field is the phase duration.
    //
    // For scheduling latency, we use `wu_lat` (wakeup latency).
    // For e2e latency, we use `period` (full iteration time).

    let mut iteration_count: u64 = 0;

    for (i, row) in rows.iter().enumerate() {
        cpus.insert(row.cpu);

        if row.run_us > 0 {
            // This is a compute phase
            total_run_ns += row.run_us * 1000;
            iteration_count += 1;

            // wu_lat is the scheduling latency for this wakeup
            let wulat_ns = row.wu_lat_us * 1000;
            total_sched_latency_ns += wulat_ns;
            if wulat_ns > 0 || i > 0 {
                // Skip the very first wakeup (no prior sleep)
                sched_latencies_ns.push(wulat_ns as f64);
            }
        } else {
            // This is a sleep/idle phase
            total_sleep_ns += row.period_us * 1000;
        }

        // E2E latency: use period_us for non-zero-run phases as the
        // inter-arrival time proxy
        if row.period_us > 0 && row.run_us > 0 {
            e2e_latencies_ns.push(row.period_us as f64 * 1000.0);
        }
    }

    let avg_run = if iteration_count > 0 {
        total_run_ns as f64 / iteration_count as f64
    } else {
        0.0
    };

    let sleep_count = rows.iter().filter(|r| r.run_us == 0).count() as u64;
    let avg_sleep = if sleep_count > 0 {
        total_sleep_ns as f64 / sleep_count as f64
    } else {
        0.0
    };

    let sched_count = sched_latencies_ns.len();
    let avg_sched = if sched_count > 0 {
        total_sched_latency_ns as f64 / sched_count as f64
    } else {
        0.0
    };

    ThreadProfile {
        name: name.to_string(),
        role,
        total_run_ns,
        total_sleep_ns,
        total_sched_latency_ns,
        iteration_count,
        cpu_set: cpus,
        avg_run_ns: avg_run,
        avg_sleep_ns: avg_sleep,
        avg_sched_latency_ns: avg_sched,
        sched_latency_percentiles: compute_percentiles(&mut sched_latencies_ns),
        e2e_latency_percentiles: compute_percentiles(&mut e2e_latencies_ns),
    }
}

// ---------------------------------------------------------------------------
// Metrics CSV parser
// ---------------------------------------------------------------------------

/// Parse a metrics CSV (14-column canonical format) into ThreadProfiles.
///
/// This reads the summarized metrics that `repm run` or manual pipelines produce.
/// Each row has a (scheduler, mode, thread_type, thread_id, metric, percentile, value).
/// We aggregate across rows to build ThreadProfiles.
pub fn parse_metrics_csv(
    path: &Path,
    thread_type_filter: Option<&str>,
) -> Result<Vec<ThreadProfile>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read metrics CSV: {}", path.display()))?;

    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(content.as_bytes());

    let headers = reader.headers().context("CSV has no headers")?.clone();
    let col = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };

    let col_thread_type = col("thread_type");
    let col_thread_id = col("thread_id");
    let col_metric = col("metric_name");
    let col_percentile = col("percentile");
    let col_value = col("value");
    let col_unit = col("unit");
    let col_sample_count = col("sample_count");

    if col_value.is_none() || col_metric.is_none() {
        bail!(
            "CSV {} missing required columns (metric_name, value)",
            path.display()
        );
    }

    // Collect per-thread metric values
    // Key: thread_id → { metric_percentile → value_ns }
    let mut thread_metrics: HashMap<String, BTreeMap<String, f64>> = HashMap::new();
    let mut thread_types: HashMap<String, String> = HashMap::new();
    let mut thread_sample_counts: HashMap<String, u64> = HashMap::new();

    for record in reader.records() {
        let record = record?;
        let get =
            |col_opt: Option<usize>| -> &str { col_opt.and_then(|i| record.get(i)).unwrap_or("") };

        let thread_type = get(col_thread_type);
        if let Some(filter) = thread_type_filter {
            if !thread_type.is_empty() && thread_type != filter {
                continue;
            }
        }

        let thread_id = get(col_thread_id).to_string();
        let metric = get(col_metric);
        let percentile = get(col_percentile);
        let val_str = get(col_value);
        let unit = get(col_unit);

        let val: f64 = match val_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Normalize value to nanoseconds
        let val_ns = match unit {
            "us" => val * 1000.0,
            "ms" => val * 1_000_000.0,
            "ns" => val,
            "s" => val * 1_000_000_000.0,
            _ => val, // assume ns
        };

        let key = format!("{}_{}", metric, percentile);
        thread_metrics
            .entry(thread_id.clone())
            .or_default()
            .insert(key, val_ns);

        if !thread_type.is_empty() {
            thread_types.insert(thread_id.clone(), thread_type.to_string());
        }

        if let Some(sc_col) = col_sample_count {
            if let Some(sc_str) = record.get(sc_col) {
                if let Ok(sc) = sc_str.parse::<u64>() {
                    thread_sample_counts.insert(thread_id, sc);
                }
            }
        }
    }

    // Build profiles from collected metrics
    let mut profiles = Vec::new();
    for (thread_id, metrics) in &thread_metrics {
        let thread_type_str = thread_types
            .get(thread_id)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let role = match thread_type_str {
            "foreground" | "cache_worker" => ThreadRole::Foreground,
            "background" => ThreadRole::Background,
            "irq_generator" => ThreadRole::IrqGenerator,
            _ => ThreadRole::from_name(thread_id),
        };

        let sample_count = thread_sample_counts.get(thread_id).copied().unwrap_or(0);

        let get_metric = |key: &str| -> f64 { metrics.get(key).copied().unwrap_or(0.0) };

        profiles.push(ThreadProfile {
            name: thread_id.clone(),
            role,
            total_run_ns: 0, // Not available from summary CSV
            total_sleep_ns: 0,
            total_sched_latency_ns: 0,
            iteration_count: sample_count,
            cpu_set: BTreeSet::new(),
            avg_run_ns: 0.0,
            avg_sleep_ns: 0.0,
            avg_sched_latency_ns: get_metric("sched_latency_avg"),
            sched_latency_percentiles: LatencyPercentiles {
                p50: get_metric("sched_latency_p50"),
                p90: get_metric("sched_latency_p90"),
                p99: get_metric("sched_latency_p99"),
                p999: get_metric("sched_latency_p999"),
                avg: get_metric("sched_latency_avg"),
                min: get_metric("sched_latency_min"),
                max: get_metric("sched_latency_max"),
            },
            e2e_latency_percentiles: LatencyPercentiles {
                p50: get_metric("e2e_latency_p50"),
                p90: get_metric("e2e_latency_p90"),
                p99: get_metric("e2e_latency_p99"),
                p999: get_metric("e2e_latency_p999"),
                avg: get_metric("e2e_latency_avg"),
                min: get_metric("e2e_latency_min"),
                max: get_metric("e2e_latency_max"),
            },
        });
    }

    profiles.sort_by(|a, b| (a.role.as_str(), &a.name).cmp(&(b.role.as_str(), &b.name)));
    Ok(profiles)
}

// ---------------------------------------------------------------------------
// Perfetto JSON trace parser (Chrome trace format)
// ---------------------------------------------------------------------------

/// Parse a Perfetto/Chrome JSON trace into scheduling events.
///
/// Supports the "traceEvents" format:
/// ```json
/// { "traceEvents": [
///   {"name":"sched_switch","ph":"X","ts":1234,"dur":500,"pid":1,"tid":100,
///    "args":{"prev_comm":"worker","next_comm":"idle"}},
///   ...
/// ]}
/// ```
pub fn parse_perfetto_json(path: &Path) -> Result<Vec<SchedulingEvent>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read Perfetto trace: {}", path.display()))?;

    let json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse Perfetto JSON: {}", path.display()))?;

    let events_array = json
        .get("traceEvents")
        .and_then(|v| v.as_array())
        .with_context(|| "Perfetto JSON missing 'traceEvents' array")?;

    let mut events = Vec::new();
    for entry in events_array {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let ts_us = entry.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dur_us = entry.get("dur").and_then(|v| v.as_f64());
        let tid = entry.get("tid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let args = entry.get("args");

        let event_type = match name {
            "sched_switch" => EventType::SchedSwitch,
            "sched_waking" | "sched_wakeup" => EventType::SchedWaking,
            n if n.contains("irq") || n.contains("softirq") => EventType::Irq,
            _ => continue, // Skip non-scheduling events
        };

        let thread_name = args
            .and_then(|a| {
                a.get("next_comm")
                    .or_else(|| a.get("comm"))
                    .or_else(|| a.get("name"))
            })
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let cpu = args
            .and_then(|a| a.get("cpu").or_else(|| a.get("target_cpu")))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        events.push(SchedulingEvent {
            timestamp_ns: (ts_us * 1000.0) as u64,
            event_type,
            cpu,
            pid: tid,
            thread_name,
            duration_ns: dur_us.map(|d| (d * 1000.0) as u64),
        });
    }

    events.sort_by_key(|e| e.timestamp_ns);
    Ok(events)
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn compute_percentiles(values: &mut Vec<f64>) -> LatencyPercentiles {
    if values.is_empty() {
        return LatencyPercentiles::default();
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let pct = |p: f64| -> f64 {
        let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
        values[idx.min(n - 1)]
    };
    let avg = values.iter().sum::<f64>() / n as f64;
    LatencyPercentiles {
        p50: pct(50.0),
        p90: pct(90.0),
        p99: pct(99.0),
        p999: pct(99.9),
        avg,
        min: values[0],
        max: values[n - 1],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_file(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_extract_thread_name() {
        assert_eq!(
            extract_thread_name("cache_prodscale-cache_worker_0-0.log"),
            "cache_worker_0"
        );
        assert_eq!(
            extract_thread_name("repromagic-fg_thread_0-1234.log"),
            "fg_thread_0"
        );
        assert_eq!(
            extract_thread_name("repromagic-bg_hog_3-5678.log"),
            "bg_hog_3"
        );
        assert_eq!(
            extract_thread_name("cache_prodscale-background_hog_10-24.log"),
            "background_hog_10"
        );
    }

    #[test]
    fn test_thread_role_from_name() {
        assert_eq!(
            ThreadRole::from_name("cache_worker_0"),
            ThreadRole::Foreground
        );
        assert_eq!(ThreadRole::from_name("fg_thread_2"), ThreadRole::Foreground);
        assert_eq!(
            ThreadRole::from_name("background_hog_10"),
            ThreadRole::Background
        );
        assert_eq!(ThreadRole::from_name("bg_hog_3"), ThreadRole::Background);
        assert_eq!(ThreadRole::from_name("irq_gen_0"), ThreadRole::IrqGenerator);
        assert_eq!(ThreadRole::from_name("random_thread"), ThreadRole::Unknown);
    }

    #[test]
    fn test_parse_rtapp_log_basic() {
        let tmp = std::env::temp_dir().join("repm_test_trace_rtapp");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let log = "\
#idx     perf      run   period           start             end          rel_st      slack c_duration   c_period     wu_lat  cpu
   0        0      500      500 1000000000000000 1000000000500000          0          0        500          0          0    2
   0        0        0     1500 1000000000550000 1000000002050000          0          0          0          0        200    2
   0        0      500      500 1000000002100000 1000000002600000          0          0        500          0         50    3
   0        0        0     1500 1000000002650000 1000000004150000          0          0          0          0        210    3
";
        let path = write_temp_file(&tmp, "repromagic-fg_thread_0-100.log", log);

        let (events, profile) = parse_rtapp_log(&path).unwrap();

        assert_eq!(profile.name, "fg_thread_0");
        assert_eq!(profile.role, ThreadRole::Foreground);
        assert_eq!(profile.iteration_count, 2); // 2 compute phases
        assert_eq!(events.len(), 2); // 2 RunComplete events

        // Check CPU set
        assert!(profile.cpu_set.contains(&2));
        assert!(profile.cpu_set.contains(&3));

        // Average run should be 500µs = 500000ns
        assert!((profile.avg_run_ns - 500_000.0).abs() < 1.0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_rtapp_log_dir() {
        let tmp = std::env::temp_dir().join("repm_test_trace_dir");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let fg_log = "\
#idx     perf      run   period           start             end          rel_st      slack c_duration   c_period     wu_lat  cpu
   0        0      500      500 1000000000000 1000000500000          0          0        500          0        100    0
";
        let bg_log = "\
#idx     perf      run   period           start             end          rel_st      slack c_duration   c_period     wu_lat  cpu
   0        0      130      130 1000000000000 1000000130000          0          0        130          0         50    1
";
        write_temp_file(&tmp, "repromagic-fg_thread_0-100.log", fg_log);
        write_temp_file(&tmp, "repromagic-bg_hog_0-200.log", bg_log);
        write_temp_file(&tmp, "not_a_log.txt", "ignore me");

        let profiles = parse_rtapp_log_dir(&tmp).unwrap();
        assert_eq!(profiles.len(), 2);

        // Sorted by role (background < foreground alphabetically)
        assert_eq!(profiles[0].role, ThreadRole::Background);
        assert_eq!(profiles[1].role, ThreadRole::Foreground);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_metrics_csv() {
        let tmp = std::env::temp_dir().join("repm_test_trace_csv");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let csv = "\
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,e2e_latency,p50,500000.0,ns,4000,1,,85.0
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,e2e_latency,p99,1200000.0,ns,4000,1,,85.0
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,sched_latency,p50,10000.0,ns,4000,1,,85.0
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,sched_latency,p99,80000.0,ns,4000,1,,85.0
2026-04-16,rtapp_pinned,EEVDF,baseline,background,bg_0,sched_latency,p50,5000.0,ns,4000,1,,85.0
";
        let path = write_temp_file(&tmp, "metrics.csv", csv);

        let profiles = parse_metrics_csv(&path, Some("foreground")).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "fg_0");
        assert_eq!(profiles[0].role, ThreadRole::Foreground);
        assert_eq!(profiles[0].e2e_latency_percentiles.p50, 500_000.0);
        assert_eq!(profiles[0].e2e_latency_percentiles.p99, 1_200_000.0);
        assert_eq!(profiles[0].sched_latency_percentiles.p50, 10_000.0);
        assert_eq!(profiles[0].sched_latency_percentiles.p99, 80_000.0);
        assert_eq!(profiles[0].iteration_count, 4000);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_metrics_csv_unit_conversion() {
        let tmp = std::env::temp_dir().join("repm_test_trace_units");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let csv = "\
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,sched_latency,p50,100.0,us,1000,1,,
2026-04-16,rtapp_pinned,EEVDF,baseline,foreground,fg_0,sched_latency,p99,500.0,us,1000,1,,
";
        let path = write_temp_file(&tmp, "metrics.csv", csv);

        let profiles = parse_metrics_csv(&path, None).unwrap();
        assert_eq!(profiles.len(), 1);
        // 100µs = 100_000ns
        assert_eq!(profiles[0].sched_latency_percentiles.p50, 100_000.0);
        // 500µs = 500_000ns
        assert_eq!(profiles[0].sched_latency_percentiles.p99, 500_000.0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_metrics_csv_cache_worker_type() {
        let tmp = std::env::temp_dir().join("repm_test_trace_cacheworker");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Legacy format uses "cache_worker" instead of "foreground"
        let csv = "\
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
2026-04-16,rtapp_pinned,EEVDF,level1,cache_worker,0,e2e_latency,p99,1000000.0,ns,4000,1,,85.0
";
        let path = write_temp_file(&tmp, "metrics.csv", csv);

        // Filter on "cache_worker" should work
        let profiles = parse_metrics_csv(&path, Some("cache_worker")).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].role, ThreadRole::Foreground); // cache_worker → Foreground

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_parse_perfetto_json() {
        let tmp = std::env::temp_dir().join("repm_test_trace_perfetto");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let json = r#"{"traceEvents": [
            {"name":"sched_switch","ph":"X","ts":1000000,"dur":500,"pid":1,"tid":100,
             "args":{"next_comm":"worker_0","cpu":2}},
            {"name":"sched_waking","ph":"i","ts":1000600,"pid":1,"tid":101,
             "args":{"comm":"worker_1","target_cpu":3}},
            {"name":"some_other_event","ph":"X","ts":1001000,"pid":1,"tid":102,
             "args":{}},
            {"name":"softirq_entry","ph":"X","ts":1002000,"dur":50,"pid":1,"tid":0,
             "args":{"cpu":0}}
        ]}"#;
        let path = write_temp_file(&tmp, "trace.json", json);

        let events = parse_perfetto_json(&path).unwrap();
        // Should skip "some_other_event", keep sched_switch, sched_waking, softirq
        assert_eq!(events.len(), 3);

        assert_eq!(events[0].event_type, EventType::SchedSwitch);
        assert_eq!(events[0].thread_name, "worker_0");
        assert_eq!(events[0].cpu, 2);
        assert_eq!(events[0].timestamp_ns, 1_000_000_000); // 1000000µs → ns
        assert_eq!(events[0].duration_ns, Some(500_000)); // 500µs → ns

        assert_eq!(events[1].event_type, EventType::SchedWaking);
        assert_eq!(events[1].thread_name, "worker_1");
        assert_eq!(events[1].cpu, 3);

        assert_eq!(events[2].event_type, EventType::Irq);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_compute_percentiles() {
        let mut vals: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let pcts = compute_percentiles(&mut vals);
        assert!((pcts.p50 - 50.0).abs() < 1.1);
        assert!((pcts.p99 - 99.0).abs() < 1.1);
        assert_eq!(pcts.min, 1.0);
        assert_eq!(pcts.max, 100.0);
        assert!((pcts.avg - 50.5).abs() < 0.01);
    }

    #[test]
    fn test_compute_percentiles_empty() {
        let mut vals: Vec<f64> = Vec::new();
        let pcts = compute_percentiles(&mut vals);
        assert_eq!(pcts.p50, 0.0);
        assert_eq!(pcts.avg, 0.0);
    }

    /// Integration: parse a real rt-app log, if one is pointed at.
    ///
    /// Opt in by setting `REPM_REAL_RTAPP_LOG` to an rt-app log from a real
    /// capture, e.g. `<capture>/logs/<workload>-<task>-0.log`. Skipped when
    /// unset. This used to hardcode one machine's capture directory, which
    /// meant the test silently skipped everywhere else.
    #[test]
    fn test_parse_real_rtapp_log() {
        let Ok(real_log) = std::env::var("REPM_REAL_RTAPP_LOG") else {
            eprintln!("Skipping real rt-app log test (REPM_REAL_RTAPP_LOG not set)");
            return;
        };
        let real_log = std::path::Path::new(&real_log);
        if !real_log.exists() {
            eprintln!("Skipping real rt-app log test (file not found)");
            return;
        }

        let (events, profile) = parse_rtapp_log(real_log).unwrap();

        assert_eq!(profile.name, "cache_worker_0");
        assert_eq!(profile.role, ThreadRole::Foreground);
        assert!(
            profile.iteration_count > 100,
            "Expected >100 iterations, got {}",
            profile.iteration_count
        );
        assert!(
            events.len() > 100,
            "Expected >100 events, got {}",
            events.len()
        );
        assert!(!profile.cpu_set.is_empty(), "CPU set should not be empty");
        assert!(profile.avg_run_ns > 0.0, "avg_run should be > 0");

        eprintln!("  Real rt-app log parsed successfully:");
        eprintln!("    iterations: {}", profile.iteration_count);
        eprintln!("    avg_run:    {:.1}µs", profile.avg_run_ns / 1000.0);
        eprintln!("    avg_sleep:  {:.1}µs", profile.avg_sleep_ns / 1000.0);
        eprintln!(
            "    sched_lat p50: {:.1}µs",
            profile.sched_latency_percentiles.p50 / 1000.0
        );
        eprintln!(
            "    sched_lat p99: {:.1}µs",
            profile.sched_latency_percentiles.p99 / 1000.0
        );
        eprintln!("    cpus: {:?}", profile.cpu_set);
    }

    /// Integration: parse a real metrics CSV, if one is pointed at.
    ///
    /// Opt in by setting `REPM_REAL_METRICS_CSV` to a `metrics.csv` from a
    /// real capture. Skipped when unset.
    #[test]
    fn test_parse_real_metrics_csv() {
        let Ok(real_csv) = std::env::var("REPM_REAL_METRICS_CSV") else {
            eprintln!("Skipping real metrics CSV test (REPM_REAL_METRICS_CSV not set)");
            return;
        };
        let real_csv = std::path::Path::new(&real_csv);
        if !real_csv.exists() {
            eprintln!("Skipping real metrics CSV test (file not found)");
            return;
        }

        let profiles = parse_metrics_csv(real_csv, Some("cache_worker")).unwrap();
        assert!(!profiles.is_empty(), "Should find cache_worker profiles");

        for p in &profiles {
            assert_eq!(p.role, ThreadRole::Foreground);
            assert!(
                p.e2e_latency_percentiles.p50 > 0.0,
                "E2E P50 should be > 0 for {}",
                p.name
            );
        }

        eprintln!("  Real metrics CSV parsed: {} profiles", profiles.len());
        for p in &profiles {
            eprintln!(
                "    {}: e2e_p50={:.0}ns e2e_p99={:.0}ns",
                p.name, p.e2e_latency_percentiles.p50, p.e2e_latency_percentiles.p99,
            );
        }
    }
}
