use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use serde_json::Value;

use crate::workspace;

/// Generate parameterized rt-app JSON configs.
///
/// Core abstraction: N foreground threads (characteristics + relationships)
/// + M background threads (CPU pressure). All compute phases use `runtime`
/// with `clockonly` mode (wall-clock spinning, no CPU calibration needed).
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

    /// Generate config from a captured trace.
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
    if args.from_trace.is_some() {
        eprintln!("TODO: trace-based config generation not yet implemented");
        eprintln!("For now, use parameterized generation with --foreground/--background flags.");
        return Ok(());
    }

    // Try to load workspace config for defaults; fall back to CLI defaults
    let (ws_root, ws_config) = match workspace::load_config_from_cwd() {
        Ok(pair) => (Some(pair.0), Some(pair.1)),
        Err(_) => (None, None),
    };

    // Resolve parameters: CLI overrides > workspace config > hardcoded defaults
    let cores = args.cores
        .or_else(|| ws_config.as_ref().map(|c| c.defaults.cores))
        .unwrap_or(8);
    let duration = args.duration
        .or_else(|| ws_config.as_ref().map(|c| c.defaults.duration))
        .unwrap_or(30);
    let foreground = args.foreground
        .or_else(|| ws_config.as_ref().map(|c| c.workload.foreground_threads))
        .unwrap_or(4);
    let background = args.background
        .or_else(|| ws_config.as_ref().map(|c| c.workload.background_threads))
        .unwrap_or(16);

    let all_cpus: Vec<u32> = (0..cores).collect();

    // Build the rt-app config
    let mut config = serde_json::Map::new();

    // Global section
    let mut global = serde_json::Map::new();
    global.insert("duration".into(), Value::Number(duration.into()));
    global.insert("default_policy".into(), Value::String("SCHED_OTHER".into()));
    global.insert("log_basename".into(), Value::String(args.log_basename.clone()));
    global.insert("logdir".into(), Value::String("./".into()));
    global.insert("log_size".into(), Value::Number(100.into()));

    config.insert("global".into(), Value::Object(global));

    // Tasks
    let mut tasks = serde_json::Map::new();

    // Foreground threads: defined scheduling characteristics
    for i in 0..foreground {
        let name = format!("fg_thread_{}", i);
        let task = build_task(
            &all_cpus,
            args.fg_run_us,
            args.fg_sleep_us,
            None, // default priority
            "compute",
        );
        tasks.insert(name, task);
    }

    // Background threads: CPU pressure / hog threads
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

    // Optional: IRQ generator threads (pinned to even CPUs)
    if args.with_irq {
        let irq_cpus: Vec<u32> = all_cpus.iter()
            .copied()
            .filter(|c| c % 2 == 0)
            .collect();

        // Add softirq config to global
        if let Some(Value::Object(ref mut global)) = config.get_mut("global") {
            let irq_cpu_values: Vec<Value> = irq_cpus.iter()
                .map(|&c| Value::Number(c.into()))
                .collect();
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

    // Serialize to JSON
    let json_output = serde_json::to_string_pretty(&config)
        .context("Failed to serialize rt-app config")?;
    let json_output = json_output + "\n";

    // Determine output destination
    let output_path = if let Some(ref path) = args.output {
        Some(std::path::PathBuf::from(path))
    } else if let Some(ref root) = ws_root {
        Some(root.join("configs/rtapp.json"))
    } else {
        None // stdout
    };

    if let Some(ref path) = output_path {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &json_output)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        eprintln!("Wrote {}", path.display());
        eprintln!("  {} foreground threads, {} background threads, {} cores, {}s duration",
            foreground, background, cores, duration);
        if args.with_irq {
            let irq_count = (0..cores).filter(|c| c % 2 == 0).count();
            eprintln!("  {} IRQ generator threads (pinned to even CPUs)", irq_count);
        }
        eprintln!();
        eprintln!("Next step: run `repm run` to execute experiments");
    } else {
        print!("{}", json_output);
    }

    Ok(())
}

/// Build a standard task with compute + sleep phases (runtime clockonly).
fn build_task(
    cpus: &[u32],
    run_us: u32,
    sleep_us: u32,
    priority: Option<i32>,
    phase_name: &str,
) -> Value {
    let cpu_values: Vec<Value> = cpus.iter()
        .map(|&c| Value::Number(c.into()))
        .collect();

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

/// Build an IRQ generator task (pinned to specific CPU, SCHED_FIFO).
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
