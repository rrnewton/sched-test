// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! Blind synthesis — infer workload parameters from scxsim trace data.
//!
//! This module implements the trace→config pipeline:
//! 1. Parse scxsim verbose-summary output to extract per-task timing
//! 2. Classify threads into distinct behavioral groups
//! 3. Synthesize a new rt-app/scxsim workload JSON from inferred parameters
//!
//! The goal: given ONLY the output metrics from a simulation run,
//! reconstruct a workload config that produces similar scheduling behavior.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

// ---------------------------------------------------------------------------
// Inferred task profile (extracted from trace data)
// ---------------------------------------------------------------------------

/// Per-task timing profile extracted from scxsim verbose-summary.
#[derive(Debug, Clone)]
pub struct TaskProfile {
    /// Task name or PID identifier.
    pub name: String,
    /// Number of scheduling events.
    pub schedules: u64,
    /// Mean run duration in microseconds.
    pub run_mean_us: f64,
    /// Stddev of run duration in microseconds.
    pub run_stddev_us: f64,
    /// Mean inter-arrival time in microseconds.
    pub interarrival_mean_us: f64,
    /// Inferred sleep time = interarrival - run (microseconds).
    pub sleep_us: f64,
    /// Number of preemptions (indicates compute-bound behavior).
    pub preemptions: u64,
    /// Number of voluntary sleeps.
    pub sleeps: u64,
}

/// A synthesized thread class — a group of similar tasks.
#[derive(Debug, Clone)]
pub struct ThreadClass {
    /// Descriptive name for the class (e.g., "fast_fg", "slow_bg").
    pub name: String,
    /// Number of threads in this class.
    pub count: u32,
    /// Inferred run duration in microseconds (for workload config).
    pub run_us: u32,
    /// Inferred sleep duration in microseconds.
    pub sleep_us: u32,
    /// Representative task profiles in this class.
    pub members: Vec<String>,
}

/// Result of the synthesis pipeline.
#[derive(Debug)]
pub struct SynthesisResult {
    /// Inferred thread classes.
    pub classes: Vec<ThreadClass>,
    /// Number of CPUs to use.
    pub cpus: u32,
    /// Duration in seconds.
    pub duration: u32,
}

// ---------------------------------------------------------------------------
// Step 1: Parse scxsim verbose-summary
// ---------------------------------------------------------------------------

/// Parse scxsim verbose-summary text output into TaskProfiles.
///
/// Expected format (from --verbose-summary):
/// ```text
/// --- Per-Task Statistics ---
///   Task PID=1:
///     Schedules:       1798
///     Run duration:    0.102ms mean, 0.020ms stddev, CV=19.5%
///     Inter-arrival:   5.007ms mean, 0.021ms stddev
///     Preemptions:     0
///     Sleeps:          1798
/// ```
pub fn parse_verbose_summary(text: &str) -> Result<Vec<TaskProfile>> {
    let mut profiles = Vec::new();
    let mut current_pid: Option<String> = None;
    let mut schedules: u64 = 0;
    let mut run_mean_us: f64 = 0.0;
    let mut run_stddev_us: f64 = 0.0;
    let mut interarrival_mean_us: f64 = 0.0;
    let mut preemptions: u64 = 0;
    let mut sleeps: u64 = 0;

    for line in text.lines() {
        let trimmed = line.trim();

        // New task block
        if trimmed.starts_with("Task PID=") {
            // Save previous task if any
            if let Some(ref pid) = current_pid {
                let sleep = (interarrival_mean_us - run_mean_us).max(0.0);
                profiles.push(TaskProfile {
                    name: pid.clone(),
                    schedules,
                    run_mean_us,
                    run_stddev_us,
                    interarrival_mean_us,
                    sleep_us: sleep,
                    preemptions,
                    sleeps,
                });
            }
            // Parse PID
            let pid_str = trimmed
                .strip_prefix("Task PID=")
                .and_then(|s| s.strip_suffix(':'))
                .unwrap_or("unknown");
            current_pid = Some(format!("pid_{}", pid_str));
            schedules = 0;
            run_mean_us = 0.0;
            run_stddev_us = 0.0;
            interarrival_mean_us = 0.0;
            preemptions = 0;
            sleeps = 0;
        }

        // Parse fields
        if let Some(rest) = trimmed.strip_prefix("Schedules:") {
            schedules = rest.trim().parse().unwrap_or(0);
        }

        // "Run duration:    0.102ms mean, 0.020ms stddev, CV=19.5%"
        if let Some(rest) = trimmed.strip_prefix("Run duration:") {
            let parts: Vec<&str> = rest.split(',').collect();
            if let Some(mean_part) = parts.first() {
                run_mean_us =
                    parse_duration_to_us(mean_part.trim().trim_end_matches("mean").trim());
            }
            if let Some(std_part) = parts.get(1) {
                run_stddev_us =
                    parse_duration_to_us(std_part.trim().trim_end_matches("stddev").trim());
            }
        }

        // "Inter-arrival:   5.007ms mean, 0.021ms stddev"
        if let Some(rest) = trimmed.strip_prefix("Inter-arrival:") {
            let parts: Vec<&str> = rest.split(',').collect();
            if let Some(mean_part) = parts.first() {
                interarrival_mean_us =
                    parse_duration_to_us(mean_part.trim().trim_end_matches("mean").trim());
            }
        }

        if let Some(rest) = trimmed.strip_prefix("Preemptions:") {
            preemptions = rest.trim().parse().unwrap_or(0);
        }

        if let Some(rest) = trimmed.strip_prefix("Sleeps:") {
            sleeps = rest.trim().parse().unwrap_or(0);
        }
    }

    // Save last task
    if let Some(ref pid) = current_pid {
        let sleep = (interarrival_mean_us - run_mean_us).max(0.0);
        profiles.push(TaskProfile {
            name: pid.clone(),
            schedules,
            run_mean_us,
            run_stddev_us,
            interarrival_mean_us,
            sleep_us: sleep,
            preemptions,
            sleeps,
        });
    }

    if profiles.is_empty() {
        bail!("No task profiles found in verbose-summary output");
    }

    Ok(profiles)
}

/// Parse a duration string like "0.102ms", "5.007ms", "1.070us" to microseconds.
fn parse_duration_to_us(s: &str) -> f64 {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("ms") {
        v.parse::<f64>().unwrap_or(0.0) * 1000.0
    } else if let Some(v) = s.strip_suffix("us") {
        v.parse::<f64>().unwrap_or(0.0)
    } else if let Some(v) = s.strip_suffix("ns") {
        v.parse::<f64>().unwrap_or(0.0) / 1000.0
    } else if let Some(v) = s.strip_suffix('s') {
        v.parse::<f64>().unwrap_or(0.0) * 1_000_000.0
    } else {
        s.parse::<f64>().unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// Step 2: Classify threads into groups
// ---------------------------------------------------------------------------

/// Classify tasks into thread classes based on timing similarity.
///
/// Two tasks are in the same class if their run durations are within
/// a factor of 2× and their sleep durations are within 2×.
pub fn classify_threads(profiles: &[TaskProfile]) -> Vec<ThreadClass> {
    if profiles.is_empty() {
        return Vec::new();
    }

    // Sort by run_mean descending so we process the most distinctive first
    let mut sorted = profiles.to_vec();
    sorted.sort_by(|a, b| {
        b.run_mean_us
            .partial_cmp(&a.run_mean_us)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut classes: Vec<ThreadClass> = Vec::new();

    for task in &sorted {
        // Try to find an existing class this task fits into
        let mut matched = false;
        for class in &mut classes {
            let run_ratio = if class.run_us as f64 > 0.0 && task.run_mean_us > 0.0 {
                (class.run_us as f64 / task.run_mean_us).max(task.run_mean_us / class.run_us as f64)
            } else {
                f64::MAX
            };
            let sleep_ratio = if class.sleep_us as f64 > 0.0 && task.sleep_us > 0.0 {
                (class.sleep_us as f64 / task.sleep_us).max(task.sleep_us / class.sleep_us as f64)
            } else if class.sleep_us == 0 && task.sleep_us < 1.0 {
                1.0 // both ~zero
            } else {
                f64::MAX
            };

            if run_ratio <= 2.0 && sleep_ratio <= 2.0 {
                class.count += 1;
                class.members.push(task.name.clone());
                matched = true;
                break;
            }
        }

        if !matched {
            // Create a new class
            let name = if task.run_mean_us < 1000.0 {
                format!("fast_{}", classes.len())
            } else {
                format!("slow_{}", classes.len())
            };

            classes.push(ThreadClass {
                name,
                count: 1,
                run_us: task.run_mean_us.round() as u32,
                sleep_us: task.sleep_us.round() as u32,
                members: vec![task.name.clone()],
            });
        }
    }

    classes
}

// ---------------------------------------------------------------------------
// Step 3: Synthesize workload config
// ---------------------------------------------------------------------------

/// Synthesize a scxsim-compatible workload JSON from thread classes.
pub fn synthesize_workload(result: &SynthesisResult) -> serde_json::Value {
    let mut tasks = serde_json::Map::new();
    let cpus: Vec<serde_json::Value> = (0..result.cpus)
        .map(|c| serde_json::Value::Number(c.into()))
        .collect();

    for class in &result.classes {
        for i in 0..class.count {
            let name = if class.count == 1 {
                class.name.clone()
            } else {
                format!("{}_{}", class.name, i)
            };

            let mut task = serde_json::Map::new();
            task.insert("run".into(), serde_json::Value::Number(class.run_us.into()));
            task.insert(
                "sleep".into(),
                serde_json::Value::Number(class.sleep_us.into()),
            );
            task.insert("loop".into(), serde_json::Value::Number((-1_i64).into()));
            task.insert("cpus".into(), serde_json::Value::Array(cpus.clone()));
            tasks.insert(name, serde_json::Value::Object(task));
        }
    }

    let mut global = serde_json::Map::new();
    global.insert(
        "default_policy".into(),
        serde_json::Value::String("SCHED_OTHER".into()),
    );
    global.insert(
        "duration".into(),
        serde_json::Value::Number(result.duration.into()),
    );

    let mut workload = serde_json::Map::new();
    workload.insert("global".into(), serde_json::Value::Object(global));
    workload.insert("tasks".into(), serde_json::Value::Object(tasks));
    serde_json::Value::Object(workload)
}

// ---------------------------------------------------------------------------
// Full pipeline: text → classify → synthesize
// ---------------------------------------------------------------------------

/// Run the full synthesis pipeline from scxsim verbose-summary text.
pub fn synthesize_from_summary(
    summary_text: &str,
    cpus: u32,
    duration: u32,
) -> Result<SynthesisResult> {
    let profiles = parse_verbose_summary(summary_text)?;
    let classes = classify_threads(&profiles);

    if classes.is_empty() {
        bail!("No thread classes inferred from trace data");
    }

    Ok(SynthesisResult {
        classes,
        cpus,
        duration,
    })
}

/// Score how well a synthesized config matches the ground truth.
/// Returns per-class scores and an overall score (0.0 = perfect, higher = worse).
pub fn score_synthesis(
    ground_truth: &serde_json::Value,
    synthesized: &serde_json::Value,
) -> SynthesisScore {
    let gt_tasks = extract_task_params(ground_truth);
    let syn_tasks = extract_task_params(synthesized);

    let mut class_scores = Vec::new();
    let mut total_error = 0.0;

    // For each ground truth task, find the best-matching synthesized task
    for (gt_name, (gt_run, gt_sleep)) in &gt_tasks {
        let mut best_error = f64::MAX;
        let mut best_match = String::new();

        for (syn_name, (syn_run, syn_sleep)) in &syn_tasks {
            let run_ratio = symmetric_ratio(*gt_run as f64, *syn_run as f64);
            let sleep_ratio = symmetric_ratio(*gt_sleep as f64, *syn_sleep as f64);
            let error = (run_ratio + sleep_ratio) / 2.0;

            if error < best_error {
                best_error = error;
                best_match = syn_name.clone();
            }
        }

        class_scores.push(ClassScore {
            ground_truth_name: gt_name.clone(),
            matched_name: best_match,
            error: best_error,
        });
        total_error += best_error;
    }

    // Check thread count match
    let count_match = gt_tasks.len() == syn_tasks.len();

    SynthesisScore {
        class_scores,
        overall_error: if gt_tasks.is_empty() {
            0.0
        } else {
            total_error / gt_tasks.len() as f64
        },
        thread_count_match: count_match,
        ground_truth_count: gt_tasks.len(),
        synthesized_count: syn_tasks.len(),
    }
}

/// Symmetric ratio: max(a/b, b/a) - 1.0. Returns 0.0 for perfect match.
fn symmetric_ratio(a: f64, b: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 {
        if a <= 0.0 && b <= 0.0 {
            return 0.0;
        }
        return f64::MAX;
    }
    (a / b).max(b / a) - 1.0
}

fn extract_task_params(config: &serde_json::Value) -> BTreeMap<String, (u32, u32)> {
    let mut tasks = BTreeMap::new();
    if let Some(task_map) = config.get("tasks").and_then(|t| t.as_object()) {
        for (name, task) in task_map {
            let run = task.get("run").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let sleep = task.get("sleep").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            tasks.insert(name.clone(), (run, sleep));
        }
    }
    tasks
}

#[derive(Debug)]
pub struct SynthesisScore {
    pub class_scores: Vec<ClassScore>,
    pub overall_error: f64,
    pub thread_count_match: bool,
    pub ground_truth_count: usize,
    pub synthesized_count: usize,
}

#[derive(Debug)]
pub struct ClassScore {
    pub ground_truth_name: String,
    pub matched_name: String,
    pub error: f64,
}

impl std::fmt::Display for SynthesisScore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Synthesis Score:")?;
        writeln!(
            f,
            "  Thread count: {} ground truth, {} synthesized → {}",
            self.ground_truth_count,
            self.synthesized_count,
            if self.thread_count_match {
                "MATCH"
            } else {
                "MISMATCH"
            }
        )?;
        writeln!(
            f,
            "  Overall error: {:.3} (0.0 = perfect)",
            self.overall_error
        )?;
        for cs in &self.class_scores {
            let status = if cs.error < 1.0 { "OK" } else { "MISS" };
            writeln!(
                f,
                "    {} → {} : error={:.3} [{}]",
                cs.ground_truth_name, cs.matched_name, cs.error, status
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SUMMARY: &str = r#"
=== Trace Statistics ===

Duration: 9000.000ms

--- Per-Task Statistics ---
  Task PID=1:
    Schedules:       1798
    Run duration:    0.102ms mean, 0.020ms stddev, CV=19.5%
    Inter-arrival:   5.007ms mean, 0.021ms stddev
    Direct dispatch: 0
    Enqueue calls:   0
    Yields:          0
    Preemptions:     0
    Sleeps:          1798
  Task PID=2:
    Schedules:       134
    Run duration:    33.224ms mean, 18.143ms stddev, CV=54.6%
    Inter-arrival:   66.961ms mean, 19.714ms stddev
    Direct dispatch: 43
    Enqueue calls:   0
    Yields:          0
    Preemptions:     43
    Sleeps:          90

--- Per-CPU Statistics ---
  CPU 0:
    Ticks:         41
"#;

    #[test]
    fn test_parse_verbose_summary() {
        let profiles = parse_verbose_summary(SAMPLE_SUMMARY).unwrap();
        assert_eq!(profiles.len(), 2);

        // Task PID=1 (fast_waker): run ~102µs, interarrival ~5007µs
        assert_eq!(profiles[0].name, "pid_1");
        assert!((profiles[0].run_mean_us - 102.0).abs() < 1.0);
        assert!((profiles[0].interarrival_mean_us - 5007.0).abs() < 1.0);
        assert!((profiles[0].sleep_us - 4905.0).abs() < 5.0);
        assert_eq!(profiles[0].preemptions, 0);
        assert_eq!(profiles[0].sleeps, 1798);

        // Task PID=2 (slow_hog): run ~33224µs, interarrival ~66961µs
        assert_eq!(profiles[1].name, "pid_2");
        assert!((profiles[1].run_mean_us - 33224.0).abs() < 1.0);
        assert!((profiles[1].interarrival_mean_us - 66961.0).abs() < 1.0);
        assert_eq!(profiles[1].preemptions, 43);
    }

    #[test]
    fn test_classify_two_distinct_threads() {
        let profiles = parse_verbose_summary(SAMPLE_SUMMARY).unwrap();
        let classes = classify_threads(&profiles);

        // Should get 2 distinct classes (100µs vs 33ms run are >2× apart)
        assert_eq!(
            classes.len(),
            2,
            "Expected 2 thread classes, got {}: {:?}",
            classes.len(),
            classes.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_synthesize_workload() {
        let profiles = parse_verbose_summary(SAMPLE_SUMMARY).unwrap();
        let classes = classify_threads(&profiles);
        let result = SynthesisResult {
            classes,
            cpus: 4,
            duration: 10,
        };

        let workload = synthesize_workload(&result);

        // Should have 2 tasks
        let tasks = workload.get("tasks").unwrap().as_object().unwrap();
        assert_eq!(tasks.len(), 2);

        // Global should have duration
        let duration = workload["global"]["duration"].as_u64().unwrap();
        assert_eq!(duration, 10);
    }

    #[test]
    fn test_full_pipeline() {
        let result = synthesize_from_summary(SAMPLE_SUMMARY, 4, 10).unwrap();
        assert_eq!(result.classes.len(), 2);

        let workload = synthesize_workload(&result);
        let json = serde_json::to_string_pretty(&workload).unwrap();

        eprintln!("Synthesized workload:\n{}", json);

        // Verify the synthesized config has reasonable values
        let tasks = workload["tasks"].as_object().unwrap();
        for (name, task) in tasks {
            let run = task["run"].as_u64().unwrap();
            let sleep = task["sleep"].as_u64().unwrap();
            eprintln!("  {}: run={}µs, sleep={}µs", name, run, sleep);
            assert!(run > 0, "run must be > 0 for {}", name);
            assert!(sleep > 0, "sleep must be > 0 for {}", name);
        }
    }

    #[test]
    fn test_score_perfect_match() {
        let config = serde_json::json!({
            "tasks": {
                "a": {"run": 100, "sleep": 4900},
                "b": {"run": 50000, "sleep": 50000}
            }
        });
        let score = score_synthesis(&config, &config);
        assert!(score.thread_count_match);
        assert!(
            score.overall_error < 0.01,
            "Perfect match should have ~0 error, got {}",
            score.overall_error
        );
    }

    #[test]
    fn test_score_2x_off() {
        let gt = serde_json::json!({
            "tasks": {
                "a": {"run": 100, "sleep": 4900}
            }
        });
        let syn = serde_json::json!({
            "tasks": {
                "a": {"run": 200, "sleep": 9800}
            }
        });
        let score = score_synthesis(&gt, &syn);
        assert!(score.thread_count_match);
        // 2x off → symmetric_ratio = 2.0 - 1.0 = 1.0
        assert!(
            (score.overall_error - 1.0).abs() < 0.01,
            "2x off should have error ~1.0, got {}",
            score.overall_error
        );
    }

    #[test]
    fn test_score_against_ground_truth() {
        // The key test: does our pipeline produce a config close to ground truth?
        let result = synthesize_from_summary(SAMPLE_SUMMARY, 4, 10).unwrap();
        let synthesized = synthesize_workload(&result);

        let ground_truth = serde_json::json!({
            "tasks": {
                "fast_waker": {"run": 100, "sleep": 4900},
                "slow_hog": {"run": 50000, "sleep": 50000}
            }
        });

        let score = score_synthesis(&ground_truth, &synthesized);
        eprintln!("{}", score);

        assert!(
            score.thread_count_match,
            "Should detect exactly 2 thread classes"
        );

        // Each class should be within 2× (error < 1.0)
        for cs in &score.class_scores {
            assert!(
                cs.error < 1.0,
                "Class {} → {} has error {:.3} (>1.0 means >2× off)",
                cs.ground_truth_name,
                cs.matched_name,
                cs.error
            );
        }
    }

    #[test]
    fn test_parse_duration_to_us() {
        assert!((parse_duration_to_us("0.102ms") - 102.0).abs() < 0.1);
        assert!((parse_duration_to_us("5.007ms") - 5007.0).abs() < 0.1);
        assert!((parse_duration_to_us("1.070us") - 1.07).abs() < 0.01);
        assert!((parse_duration_to_us("500ns") - 0.5).abs() < 0.01);
        assert!((parse_duration_to_us("1s") - 1_000_000.0).abs() < 0.1);
    }

    #[test]
    fn test_symmetric_ratio() {
        assert!((symmetric_ratio(100.0, 100.0)).abs() < f64::EPSILON);
        assert!((symmetric_ratio(100.0, 200.0) - 1.0).abs() < f64::EPSILON);
        assert!((symmetric_ratio(200.0, 100.0) - 1.0).abs() < f64::EPSILON);
        assert!((symmetric_ratio(100.0, 300.0) - 2.0).abs() < f64::EPSILON);
    }

    /// Integration test: parse the REAL scxsim output and score against ground truth.
    #[test]
    fn test_real_scxsim_output() {
        let output_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/blind_simple/scxsim_output.txt"
        ));
        if !output_path.exists() {
            eprintln!("Skipping real scxsim output test (run scxsim first)");
            return;
        }

        let text = std::fs::read_to_string(output_path).unwrap();
        let result = synthesize_from_summary(&text, 4, 10).unwrap();
        let synthesized = synthesize_workload(&result);

        let ground_truth = serde_json::json!({
            "global": {"duration": 10, "default_policy": "SCHED_OTHER"},
            "tasks": {
                "fast_waker": {"run": 100, "sleep": 4900, "loop": -1, "cpus": [0,1,2,3]},
                "slow_hog": {"run": 50000, "sleep": 50000, "loop": -1, "cpus": [0,1,2,3]}
            }
        });

        let score = score_synthesis(&ground_truth, &synthesized);
        eprintln!("=== Real scxsim blind synthesis test ===");
        eprintln!("{}", score);
        eprintln!("Synthesized config:");
        eprintln!("{}", serde_json::to_string_pretty(&synthesized).unwrap());

        assert!(
            score.thread_count_match,
            "Must detect exactly 2 thread classes"
        );

        for cs in &score.class_scores {
            assert!(
                cs.error < 1.0,
                "Class '{}' → '{}' error={:.3} exceeds 2× threshold",
                cs.ground_truth_name,
                cs.matched_name,
                cs.error,
            );
        }
    }
}
