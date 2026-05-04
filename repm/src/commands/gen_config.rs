// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Serialize;
use serde_json::Value;

use crate::synthesis;
use crate::trace;
use crate::workspace;

/// Generate parameterized rt-app JSON configs.
///
/// Two modes:
/// - **Parameterized** (default): specify thread counts and timing directly.
/// - **From trace** (`--from-trace`): infer thread parameters from captured
///   scheduling data (blind synthesis).
#[derive(Debug, Args)]
pub struct GenConfigArgs {
    /// Number of foreground threads (override config default).
    #[arg(long)]
    pub foreground: Option<u32>,

    /// Number of background threads (override config default).
    #[arg(long)]
    pub background: Option<u32>,

    /// Number of cores (override config default).
    #[arg(long)]
    pub cores: Option<u32>,

    /// Foreground thread compute time in microseconds.
    #[arg(long, default_value = "500")]
    pub fg_run_us: u32,

    /// Foreground thread sleep time in microseconds.
    #[arg(long, default_value = "1500")]
    pub fg_sleep_us: u32,

    /// Background thread compute time in microseconds.
    #[arg(long, default_value = "130")]
    pub bg_run_us: u32,

    /// Background thread sleep time in microseconds.
    #[arg(long, default_value = "950")]
    pub bg_sleep_us: u32,

    /// Background thread nice priority.
    #[arg(long, default_value = "10")]
    pub bg_priority: i32,

    /// Experiment duration in seconds.
    #[arg(long)]
    pub duration: Option<u32>,

    /// Output file path (default: configs/rtapp.json, or stdout if no workspace).
    #[arg(long, short = 'o')]
    pub output: Option<String>,

    /// Generate config from a captured trace (blind synthesis).
    ///
    /// Accepts: scxsim verbose-summary text, rt-app log directory,
    /// Perfetto JSON trace, or metrics CSV.
    #[arg(long)]
    pub from_trace: Option<String>,

    /// Log basename for rt-app log files.
    #[arg(long, default_value = "repromagic")]
    pub log_basename: String,

    /// Enable IRQ generator threads (pinned to even CPUs).
    #[arg(long)]
    pub with_irq: bool,

    /// IRQ generator run time in microseconds.
    #[arg(long, default_value = "5000")]
    pub irq_run_us: u32,

    /// IRQ generator sleep time in microseconds.
    #[arg(long, default_value = "5000")]
    pub irq_sleep_us: u32,

    /// Show verbose classification decisions during --from-trace.
    #[arg(long)]
    pub verbose: bool,

    /// Output format: rtapp (phased rt-app JSON) or sim (scxsim JSON).
    #[arg(long, default_value = "rtapp")]
    pub format: ConfigFormat,
}

/// Output config format.
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ConfigFormat {
    /// Phased rt-app JSON with runtime clockonly spec.
    Rtapp,
    /// Simple scxsim JSON with run/sleep integers.
    Sim,
}

/// Runtime clockonly spec — pure wall-clock spin, no calibration.
#[derive(Debug, Serialize)]
struct RuntimeSpec {
    duration: u32,
    mode: &'static str,
}

impl RuntimeSpec {
    fn clockonly(duration_us: u32) -> Self {
        Self {
            duration: duration_us,
            mode: "clockonly",
        }
    }
}

pub fn execute(args: &GenConfigArgs) -> Result<()> {
    if let Some(ref trace_path) = args.from_trace {
        return execute_from_trace(args, trace_path);
    }
    execute_parameterized(args)
}

// ---------------------------------------------------------------------------
// --from-trace: blind synthesis pipeline
// ---------------------------------------------------------------------------

fn execute_from_trace(args: &GenConfigArgs, trace_path: &str) -> Result<()> {
    let path = std::path::Path::new(trace_path);
    if !path.exists() {
        bail!("Trace file not found: {}", trace_path);
    }

    let cores = args.cores.unwrap_or(4);
    let duration = args.duration.unwrap_or(30);

    eprintln!("repm gen-config --from-trace {}", trace_path);

    // Detect input type and run synthesis
    let result = if path.is_dir() {
        eprintln!("  input type: rt-app log directory");
        from_rtapp_logs(path, cores, duration)?
    } else {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", trace_path))?;

        if content.trim_start().starts_with('{') && content.contains("traceEvents") {
            eprintln!("  input type: Perfetto JSON trace");
            from_perfetto(&content, cores, duration)?
        } else if content.contains("Per-Task Statistics") || content.contains("Run duration:") {
            eprintln!("  input type: scxsim verbose-summary");
            synthesis::synthesize_from_summary(&content, cores, duration)?
        } else if content.contains("timestamp,") && content.contains("metric_name") {
            eprintln!("  input type: metrics CSV");
            from_metrics_csv(path, cores, duration)?
        } else {
            bail!(
                "Cannot determine trace format for {}.\n\
                 Supported: scxsim verbose-summary, Perfetto JSON, metrics CSV, rt-app log dir.",
                trace_path
            );
        }
    };

    // Print classification decisions if verbose
    if args.verbose {
        eprintln!();
        eprintln!("  Classification:");
        for class in &result.classes {
            eprintln!(
                "    {} ({}): run={}µs sleep={}µs count={}",
                class.name,
                if class.run_us < 1000 { "fast" } else { "slow" },
                class.run_us,
                class.sleep_us,
                class.count,
            );
            for m in &class.members {
                eprintln!("      <- {}", m);
            }
        }
    }

    // Generate output
    let json_output = match args.format {
        ConfigFormat::Sim => {
            let workload = synthesis::synthesize_workload(&result);
            serde_json::to_string_pretty(&workload).context("serialize")? + "\n"
        }
        ConfigFormat::Rtapp => {
            let workload = synthesize_rtapp_from_classes(&result, &args.log_basename);
            serde_json::to_string_pretty(&workload).context("serialize")? + "\n"
        }
    };

    // Write output
    if let Some(ref out) = args.output {
        let p = std::path::Path::new(out);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, &json_output).with_context(|| format!("Failed to write {}", out))?;
        eprintln!(
            "\nWrote {} ({} classes, {} threads, {} cores, {}s)",
            out,
            result.classes.len(),
            result.classes.iter().map(|c| c.count).sum::<u32>(),
            result.cpus,
            result.duration,
        );
    } else {
        print!("{}", json_output);
    }

    Ok(())
}

fn from_rtapp_logs(
    dir: &std::path::Path,
    cpus: u32,
    duration: u32,
) -> Result<synthesis::SynthesisResult> {
    let profiles = trace::parse_rtapp_log_dir(dir)?;
    let task_profiles: Vec<synthesis::TaskProfile> = profiles
        .iter()
        .map(|p| synthesis::TaskProfile {
            name: p.name.clone(),
            schedules: p.iteration_count,
            run_mean_us: p.avg_run_ns / 1000.0,
            run_stddev_us: 0.0,
            interarrival_mean_us: (p.avg_run_ns + p.avg_sleep_ns) / 1000.0,
            sleep_us: p.avg_sleep_ns / 1000.0,
            preemptions: 0,
            sleeps: p.iteration_count,
        })
        .collect();
    let classes = synthesis::classify_threads(&task_profiles);
    Ok(synthesis::SynthesisResult {
        classes,
        cpus,
        duration,
    })
}

fn from_perfetto(content: &str, cpus: u32, duration: u32) -> Result<synthesis::SynthesisResult> {
    let json: serde_json::Value = serde_json::from_str(content)?;
    let events = json
        .get("traceEvents")
        .and_then(|v| v.as_array())
        .context("Missing traceEvents")?;

    // Collect per-task run durations from B/E event pairs on same CPU (pid = CPU row)
    use std::collections::HashMap;
    // Key: pid (CPU row) → (task_name, begin_ts)
    let mut active: HashMap<u64, (String, f64)> = HashMap::new();
    let mut task_runs: HashMap<String, Vec<f64>> = HashMap::new();

    for event in events {
        let ph = event.get("ph").and_then(|v| v.as_str()).unwrap_or("");
        let cat = event.get("cat").and_then(|v| v.as_str()).unwrap_or("");
        let pid = event.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = event.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);

        if cat != "sched" {
            continue;
        }

        match ph {
            "B" => {
                let name = event
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    active.insert(pid, (name, ts));
                }
            }
            "E" => {
                if let Some((name, begin_ts)) = active.remove(&pid) {
                    let dur_us = ts - begin_ts;
                    if dur_us > 0.0 {
                        task_runs.entry(name).or_default().push(dur_us);
                    }
                }
            }
            _ => {}
        }
    }

    if task_runs.is_empty() {
        bail!("No scheduling events found in Perfetto trace");
    }

    let total_dur_us = duration as f64 * 1_000_000.0;
    let mut task_profiles = Vec::new();
    for (name, runs) in &task_runs {
        let n = runs.len() as f64;
        let mean = runs.iter().sum::<f64>() / n;
        let interarrival = if runs.len() > 1 {
            total_dur_us / n
        } else {
            total_dur_us
        };

        task_profiles.push(synthesis::TaskProfile {
            name: name.clone(),
            schedules: runs.len() as u64,
            run_mean_us: mean,
            run_stddev_us: 0.0,
            interarrival_mean_us: interarrival,
            sleep_us: (interarrival - mean).max(0.0),
            preemptions: 0,
            sleeps: runs.len() as u64,
        });
    }

    let classes = synthesis::classify_threads(&task_profiles);
    Ok(synthesis::SynthesisResult {
        classes,
        cpus,
        duration,
    })
}

fn from_metrics_csv(
    path: &std::path::Path,
    cpus: u32,
    duration: u32,
) -> Result<synthesis::SynthesisResult> {
    let profiles = trace::parse_metrics_csv(path, None)?;
    let task_profiles: Vec<synthesis::TaskProfile> = profiles
        .iter()
        .map(|p| {
            let run_us = if p.avg_run_ns > 0.0 {
                p.avg_run_ns / 1000.0
            } else {
                1.0 // fallback
            };
            let interarrival_us = p.e2e_latency_percentiles.avg / 1000.0;
            let sleep_us = (interarrival_us - run_us).max(0.0);

            synthesis::TaskProfile {
                name: p.name.clone(),
                schedules: p.iteration_count,
                run_mean_us: run_us.max(1.0),
                run_stddev_us: 0.0,
                interarrival_mean_us: interarrival_us,
                sleep_us,
                preemptions: 0,
                sleeps: p.iteration_count,
            }
        })
        .collect();

    let classes = synthesis::classify_threads(&task_profiles);
    Ok(synthesis::SynthesisResult {
        classes,
        cpus,
        duration,
    })
}

/// Convert synthesis result to rt-app phased JSON format.
fn synthesize_rtapp_from_classes(
    result: &synthesis::SynthesisResult,
    log_basename: &str,
) -> serde_json::Value {
    let mut config = serde_json::Map::new();

    let mut global = serde_json::Map::new();
    global.insert("duration".into(), Value::Number(result.duration.into()));
    global.insert("default_policy".into(), Value::String("SCHED_OTHER".into()));
    global.insert(
        "log_basename".into(),
        Value::String(log_basename.to_string()),
    );
    global.insert("logdir".into(), Value::String("./".into()));
    global.insert("log_size".into(), Value::Number(100.into()));
    config.insert("global".into(), Value::Object(global));

    let mut tasks = serde_json::Map::new();
    for class in &result.classes {
        for i in 0..class.count {
            let name = if class.count == 1 {
                class.name.clone()
            } else {
                format!("{}_{}", class.name, i)
            };
            let task = build_task(
                &(0..result.cpus).collect::<Vec<u32>>(),
                class.run_us,
                class.sleep_us,
                None,
                "compute",
            );
            tasks.insert(name, task);
        }
    }
    config.insert("tasks".into(), Value::Object(tasks));

    Value::Object(config)
}

// ---------------------------------------------------------------------------
// Parameterized generation (original path)
// ---------------------------------------------------------------------------

fn execute_parameterized(args: &GenConfigArgs) -> Result<()> {
    let (ws_root, ws_config) = match workspace::load_config_from_cwd() {
        Ok(pair) => (Some(pair.0), Some(pair.1)),
        Err(_) => (None, None),
    };

    let cores = args
        .cores
        .or_else(|| ws_config.as_ref().map(|c| c.defaults.cores))
        .unwrap_or(8);
    let duration = args
        .duration
        .or_else(|| ws_config.as_ref().map(|c| c.defaults.duration))
        .unwrap_or(30);
    let foreground = args
        .foreground
        .or_else(|| ws_config.as_ref().map(|c| c.workload.foreground_threads))
        .unwrap_or(4);
    let background = args
        .background
        .or_else(|| ws_config.as_ref().map(|c| c.workload.background_threads))
        .unwrap_or(16);

    let all_cpus: Vec<u32> = (0..cores).collect();
    let mut config = serde_json::Map::new();

    let mut global = serde_json::Map::new();
    global.insert("duration".into(), Value::Number(duration.into()));
    global.insert("default_policy".into(), Value::String("SCHED_OTHER".into()));
    global.insert(
        "log_basename".into(),
        Value::String(args.log_basename.clone()),
    );
    global.insert("logdir".into(), Value::String("./".into()));
    global.insert("log_size".into(), Value::Number(100.into()));
    config.insert("global".into(), Value::Object(global));

    let mut tasks = serde_json::Map::new();

    for i in 0..foreground {
        let name = format!("fg_thread_{}", i);
        let task = build_task(&all_cpus, args.fg_run_us, args.fg_sleep_us, None, "compute");
        tasks.insert(name, task);
    }

    for i in 0..background {
        let name = format!("bg_hog_{}", i);
        let task = build_task(
            &all_cpus,
            args.bg_run_us,
            args.bg_sleep_us,
            Some(args.bg_priority),
            "work",
        );
        tasks.insert(name, task);
    }

    if args.with_irq {
        let irq_cpus: Vec<u32> = all_cpus.iter().copied().filter(|c| c % 2 == 0).collect();
        if let Some(Value::Object(ref mut global)) = config.get_mut("global") {
            let irq_cpu_values: Vec<Value> =
                irq_cpus.iter().map(|&c| Value::Number(c.into())).collect();
            global.insert("softirq_target_cpus".into(), Value::Array(irq_cpu_values));
            global.insert("softirq_packet_size".into(), Value::Number(64.into()));
        }
        for (i, &cpu) in irq_cpus.iter().enumerate() {
            let name = format!("irq_gen_{}", i);
            let task = build_irq_task(cpu, args.irq_run_us, args.irq_sleep_us);
            tasks.insert(name, task);
        }
    }

    config.insert("tasks".into(), Value::Object(tasks));

    let json_output = serde_json::to_string_pretty(&config).context("Failed to serialize")? + "\n";

    let output_path = if let Some(ref path) = args.output {
        Some(std::path::PathBuf::from(path))
    } else {
        ws_root.as_ref().map(|root| root.join("configs/rtapp.json"))
    };

    if let Some(ref path) = output_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &json_output)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        eprintln!("Wrote {}", path.display());
        eprintln!(
            "  {} foreground, {} background, {} cores, {}s",
            foreground, background, cores, duration
        );
    } else {
        print!("{}", json_output);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Task builders
// ---------------------------------------------------------------------------

fn build_task(
    cpus: &[u32],
    run_us: u32,
    sleep_us: u32,
    priority: Option<i32>,
    phase_name: &str,
) -> Value {
    let cpu_values: Vec<Value> = cpus.iter().map(|&c| Value::Number(c.into())).collect();
    let runtime = serde_json::to_value(RuntimeSpec::clockonly(run_us)).unwrap();

    let mut phases = serde_json::Map::new();
    let mut compute_phase = serde_json::Map::new();
    compute_phase.insert("runtime".into(), runtime);
    compute_phase.insert("loop".into(), Value::Number(1.into()));
    phases.insert(phase_name.into(), Value::Object(compute_phase));

    let mut idle_phase = serde_json::Map::new();
    idle_phase.insert("sleep".into(), Value::Number(sleep_us.into()));
    idle_phase.insert("loop".into(), Value::Number(1.into()));
    phases.insert("idle".into(), Value::Object(idle_phase));

    let mut task = serde_json::Map::new();
    task.insert("cpus".into(), Value::Array(cpu_values));
    task.insert("loop".into(), Value::Number((-1_i64).into()));
    task.insert("phases".into(), Value::Object(phases));

    if let Some(prio) = priority {
        task.insert("priority".into(), Value::Number(prio.into()));
    }

    Value::Object(task)
}

fn build_irq_task(cpu: u32, run_us: u32, sleep_us: u32) -> Value {
    let runtime = serde_json::to_value(RuntimeSpec::clockonly(run_us)).unwrap();

    let mut phases = serde_json::Map::new();
    let mut irq_phase = serde_json::Map::new();
    irq_phase.insert("runtime".into(), runtime);
    irq_phase.insert("loop".into(), Value::Number(1.into()));
    phases.insert("irq_work".into(), Value::Object(irq_phase));

    let mut idle_phase = serde_json::Map::new();
    idle_phase.insert("sleep".into(), Value::Number(sleep_us.into()));
    idle_phase.insert("loop".into(), Value::Number(1.into()));
    phases.insert("idle".into(), Value::Object(idle_phase));

    let mut task = serde_json::Map::new();
    task.insert("cpus".into(), Value::Array(vec![Value::Number(cpu.into())]));
    task.insert("policy".into(), Value::String("SCHED_FIFO".into()));
    task.insert("priority".into(), Value::Number(1.into()));
    task.insert("loop".into(), Value::Number((-1_i64).into()));
    task.insert("phases".into(), Value::Object(phases));

    Value::Object(task)
}
