use scx_simulator::{
    workloads, CpuId, DynamicScheduler, ExitKind, IrqType, NativeConcurrentConfig, NoiseConfig,
    Phase, Pid, PmuEvent, PreemptMode, PreemptiveConfig, RepeatMode, Scenario, Simulator,
    TaskBehavior,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
enum ChaosMode {
    Off,
    Cooperative,
    NativeConcurrent,
    Preemptive,
}

#[derive(Debug, Clone, Copy)]
enum ScenarioKind {
    Throttle,
    R3Like,
    Churn,
    Lifecycle,
    Enomem,
}

#[derive(Debug, Clone)]
struct Args {
    scenario: ScenarioKind,
    mode: ChaosMode,
    seed: u32,
    cpus: u32,
    tasks: u32,
    duration_ms: u64,
    window_ns: u64,
    timeslice_min: u64,
    timeslice_max: u64,
    break_on: PmuEvent,
    fixed_priority: bool,
    tick_jitter_stddev_ns: u64,
    initial_tick_skew_ns: u64,
    run_jitter_cv_ppm: u64,
    max_cgroups: u32,
    expect_cgroup_exhausted: bool,
}

type ProbeFn = unsafe extern "C" fn() -> u64;

fn main() {
    let args = parse_args();
    let start = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| run_once(&args)));
    let wall_ms = start.elapsed().as_millis();

    match result {
        Ok(record) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "status": "ok",
                    "wall_ms": wall_ms,
                    "args": args_to_json(&args),
                    "result": record,
                }))
                .unwrap()
            );
        }
        Err(payload) => {
            let panic = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("panic payload was not a string");
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "status": "panic",
                    "wall_ms": wall_ms,
                    "args": args_to_json(&args),
                    "panic": panic,
                }))
                .unwrap()
            );
            std::process::exit(2);
        }
    }
}

fn run_once(args: &Args) -> serde_json::Value {
    let sched = DynamicScheduler::lavd(args.cpus);
    enable_cpu_bw(&sched);
    sched.lavd_set_cgroup_bw_max(args.max_cgroups);

    let throttle_count = get_probe(&sched, b"lavd_probe_cbw_throttle_count\0");
    let refill_count = get_probe(&sched, b"lavd_probe_cbw_refill_count\0");
    let put_aside_count = get_probe(&sched, b"lavd_probe_cbw_put_aside_count\0");
    let reenqueue_count = get_probe(&sched, b"lavd_probe_cbw_reenqueue_count\0");

    let scenario = build_scenario(args);
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    let exit_kind = format!("{:?}", trace.exit_kind());
    let has_error = trace.has_error();
    let expected = args.expect_cgroup_exhausted
        && matches!(trace.exit_kind(), ExitKind::ErrorCgroupExhausted { .. });

    let mut schedules = BTreeMap::new();
    for pid in 1..=args.tasks.min(32) {
        schedules.insert(pid, trace.schedule_count(Pid(pid as i32)));
    }

    json!({
        "classification": if expected {
            "expected_cgroup_exhausted"
        } else if has_error {
            "sim_error"
        } else {
            "pass"
        },
        "has_error": has_error,
        "exit_kind": exit_kind,
        "schedule_counts_first_32": schedules,
        "probes": {
            "throttles": call_probe(throttle_count),
            "refills": call_probe(refill_count),
            "put_asides": call_probe(put_aside_count),
            "reenqueues": call_probe(reenqueue_count),
        },
    })
}

fn build_scenario(args: &Args) -> Scenario {
    let all_cpus: Vec<CpuId> = (0..args.cpus).map(CpuId).collect();
    let mut builder = Scenario::builder()
        .cpus(args.cpus)
        .seed(args.seed)
        .fixed_priority(args.fixed_priority)
        .noise_config(noise_config(args))
        .max_cgroups(args.max_cgroups)
        .detect_bpf_errors()
        .watchdog_timeout_ns(Some(500_000_000))
        .duration_ms(args.duration_ms);

    builder = match args.mode {
        ChaosMode::Off => builder,
        ChaosMode::Cooperative => builder.interleave(true),
        ChaosMode::NativeConcurrent => builder.native_concurrent(NativeConcurrentConfig {
            window_ns: args.window_ns,
        }),
        ChaosMode::Preemptive => builder.preemptive(PreemptiveConfig {
            timeslice_min: args.timeslice_min,
            timeslice_max: args.timeslice_max,
            cooperative_only: false,
            break_on: args.break_on,
            preempt_mode: PreemptMode::Pmu,
        }),
    };

    match args.scenario {
        ScenarioKind::Throttle => build_throttle(builder, args, &all_cpus),
        ScenarioKind::R3Like => build_r3_like(builder, args, &all_cpus),
        ScenarioKind::Churn => build_churn(builder, args, &all_cpus),
        ScenarioKind::Lifecycle => build_lifecycle(builder, args),
        ScenarioKind::Enomem => build_enomem(builder, args),
    }
    .build()
}

fn build_throttle(
    mut builder: scx_simulator::scenario::ScenarioBuilder,
    args: &Args,
    cpus: &[CpuId],
) -> scx_simulator::scenario::ScenarioBuilder {
    builder = builder.cgroup_with_bandwidth("limited", cpus, 10_000, 2_000, 0);
    for task in 0..args.tasks.max(2) {
        builder = builder.add_task_in_cgroup(
            &format!("hog-{task}"),
            0,
            workloads::cpu_bound(args.duration_ms * 1_000_000),
            "limited",
        );
    }
    builder
}

fn build_r3_like(
    mut builder: scx_simulator::scenario::ScenarioBuilder,
    args: &Args,
    cpus: &[CpuId],
) -> scx_simulator::scenario::ScenarioBuilder {
    builder = builder
        .cgroup_with_bandwidth("limited", cpus, 10_000, 3_000, 0)
        .cgroup("unlimited", cpus);
    for task in 0..args.tasks {
        builder = builder.add_task_in_cgroup(
            &format!("yes-like-{task}"),
            0,
            workloads::cpu_bound(args.duration_ms * 1_000_000),
            "limited",
        );
    }
    for pid in 1..=args.tasks.min(64) {
        let pid = Pid(pid as i32);
        let offset = pid.0 as u64 * 50_000;
        builder = builder
            .cgroup_migrate(pid, "limited", "unlimited", 25_000_000 + offset)
            .cgroup_migrate(pid, "unlimited", "limited", 45_000_000 + offset);
    }
    for cpu in 0..args.cpus.min(8) {
        builder = builder.periodic_irq(
            CpuId(cpu),
            IrqType::SoftIrq,
            5_000_000 + cpu as u64 * 250_000,
            2_000_000,
            25_000,
            &[],
        );
    }
    builder
}

fn build_churn(
    mut builder: scx_simulator::scenario::ScenarioBuilder,
    args: &Args,
    cpus: &[CpuId],
) -> scx_simulator::scenario::ScenarioBuilder {
    let groups = (args.tasks / 4).clamp(4, 32);
    for group in 0..groups {
        builder =
            builder.cgroup_with_bandwidth(&format!("group_{group}"), cpus, 100_000, 80_000, 10_000);
    }
    for task in 0..args.tasks {
        builder = builder.add_task_in_cgroup(
            &format!("task-{task}"),
            0,
            workloads::cpu_bound(args.duration_ms * 1_000_000),
            &format!("group_{}", task % groups),
        );
    }
    for round in 0..20u64 {
        let at_ns = 10_000_000 + round * 5_000_000;
        for task in 1..=args.tasks.min(32) {
            let from = format!("group_{}", ((task - 1) + round as u32) % groups);
            let to = format!("group_{}", ((task - 1) + round as u32 + 1) % groups);
            builder = builder.cgroup_migrate(Pid(task as i32), &from, &to, at_ns);
        }
    }
    builder
}

fn build_lifecycle(
    mut builder: scx_simulator::scenario::ScenarioBuilder,
    args: &Args,
) -> scx_simulator::scenario::ScenarioBuilder {
    builder = builder.add_task(
        "worker",
        0,
        TaskBehavior {
            phases: vec![Phase::Run(500_000), Phase::Sleep(500_000)],
            repeat: RepeatMode::Forever,
        },
    );
    let cycles = args.tasks.clamp(8, 64) / 4;
    for cycle in 0..cycles {
        let base = cycle as u64 * 15_000_000;
        for i in 0..4 {
            let name = format!("cycle{cycle}_{i}");
            builder = builder.cgroup_create_at(&name, None, None, base + i as u64 * 1_000_000);
            builder = builder.cgroup_destroy_at(&name, base + 8_000_000 + i as u64 * 1_000_000);
        }
    }
    builder
}

fn build_enomem(
    mut builder: scx_simulator::scenario::ScenarioBuilder,
    args: &Args,
) -> scx_simulator::scenario::ScenarioBuilder {
    builder = builder
        .add_task(
            "worker",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(1_000_000), Phase::Sleep(1_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .max_cgroups(args.max_cgroups.min(16));
    for i in 0..args.tasks.max(20) {
        builder =
            builder.cgroup_create_at(&format!("container_{i}"), None, None, i as u64 * 1_000_000);
    }
    builder
}

fn noise_config(args: &Args) -> NoiseConfig {
    let mut noise = NoiseConfig::default();
    noise.run_jitter_cv_ppm = args.run_jitter_cv_ppm;
    noise.tick_jitter_stddev_ns = args.tick_jitter_stddev_ns;
    noise.initial_tick_skew_ns = args.initial_tick_skew_ns;
    noise
}

fn enable_cpu_bw(sched: &DynamicScheduler) {
    unsafe {
        let sym = sched
            .get_symbol::<*mut bool>(b"enable_cpu_bw\0")
            .expect("enable_cpu_bw symbol not found");
        std::ptr::write_volatile(*sym, true);
    }
}

fn get_probe(sched: &DynamicScheduler, name: &[u8]) -> Option<ProbeFn> {
    unsafe { sched.get_symbol::<ProbeFn>(name).map(|sym| *sym) }
}

fn call_probe(probe: Option<ProbeFn>) -> Option<u64> {
    probe.map(|f| unsafe { f() })
}

fn parse_args() -> Args {
    let mut raw = env::args().skip(1);
    let mut args = Args {
        scenario: ScenarioKind::Throttle,
        mode: ChaosMode::Off,
        seed: 42,
        cpus: 4,
        tasks: 8,
        duration_ms: 100,
        window_ns: 10_000_000,
        timeslice_min: 300,
        timeslice_max: 1500,
        break_on: PmuEvent::RetiredBranchConditional,
        fixed_priority: false,
        tick_jitter_stddev_ns: 2_000,
        initial_tick_skew_ns: 0,
        run_jitter_cv_ppm: 200_000,
        max_cgroups: 10_000,
        expect_cgroup_exhausted: false,
    };

    while let Some(flag) = raw.next() {
        match flag.as_str() {
            "--scenario" => args.scenario = parse_scenario(&next_value(&mut raw, &flag)),
            "--mode" => args.mode = parse_mode(&next_value(&mut raw, &flag)),
            "--seed" => args.seed = parse_value(&next_value(&mut raw, &flag), &flag),
            "--cpus" => args.cpus = parse_value(&next_value(&mut raw, &flag), &flag),
            "--tasks" => args.tasks = parse_value(&next_value(&mut raw, &flag), &flag),
            "--duration-ms" => args.duration_ms = parse_value(&next_value(&mut raw, &flag), &flag),
            "--window-ns" => args.window_ns = parse_value(&next_value(&mut raw, &flag), &flag),
            "--timeslice-min" => {
                args.timeslice_min = parse_value(&next_value(&mut raw, &flag), &flag);
            }
            "--timeslice-max" => {
                args.timeslice_max = parse_value(&next_value(&mut raw, &flag), &flag);
            }
            "--break-on" => args.break_on = parse_break_on(&next_value(&mut raw, &flag)),
            "--fixed-priority" => args.fixed_priority = true,
            "--tick-jitter-stddev-ns" => {
                args.tick_jitter_stddev_ns = parse_value(&next_value(&mut raw, &flag), &flag);
            }
            "--initial-tick-skew-ns" => {
                args.initial_tick_skew_ns = parse_value(&next_value(&mut raw, &flag), &flag);
            }
            "--run-jitter-cv-ppm" => {
                args.run_jitter_cv_ppm = parse_value(&next_value(&mut raw, &flag), &flag);
            }
            "--max-cgroups" => args.max_cgroups = parse_value(&next_value(&mut raw, &flag), &flag),
            "--expect-cgroup-exhausted" => args.expect_cgroup_exhausted = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => panic!("unknown flag: {other}"),
        }
    }

    if args.timeslice_max < args.timeslice_min {
        panic!("--timeslice-max must be >= --timeslice-min");
    }
    args
}

fn next_value(raw: &mut impl Iterator<Item = String>, flag: &str) -> String {
    raw.next()
        .unwrap_or_else(|| panic!("{flag} requires a value"))
}

fn parse_value<T>(value: &str, flag: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .unwrap_or_else(|err| panic!("{flag}: failed to parse {value:?}: {err}"))
}

fn parse_scenario(value: &str) -> ScenarioKind {
    match value {
        "throttle" => ScenarioKind::Throttle,
        "r3-like" => ScenarioKind::R3Like,
        "churn" => ScenarioKind::Churn,
        "lifecycle" => ScenarioKind::Lifecycle,
        "enomem" => ScenarioKind::Enomem,
        _ => panic!("unknown scenario: {value}"),
    }
}

fn parse_mode(value: &str) -> ChaosMode {
    match value {
        "off" => ChaosMode::Off,
        "cooperative" => ChaosMode::Cooperative,
        "native-concurrent" => ChaosMode::NativeConcurrent,
        "preemptive" => ChaosMode::Preemptive,
        _ => panic!("unknown mode: {value}"),
    }
}

fn parse_break_on(value: &str) -> PmuEvent {
    match value {
        "rbc" => PmuEvent::RetiredBranchConditional,
        "insn" => PmuEvent::InstructionsRetired,
        _ => panic!("unknown --break-on value: {value}"),
    }
}

fn break_on_name(value: PmuEvent) -> &'static str {
    match value {
        PmuEvent::RetiredBranchConditional => "rbc",
        PmuEvent::InstructionsRetired => "insn",
    }
}

fn args_to_json(args: &Args) -> serde_json::Value {
    json!({
        "scenario": format!("{:?}", args.scenario),
        "mode": format!("{:?}", args.mode),
        "seed": args.seed,
        "cpus": args.cpus,
        "tasks": args.tasks,
        "duration_ms": args.duration_ms,
        "window_ns": args.window_ns,
        "timeslice_min": args.timeslice_min,
        "timeslice_max": args.timeslice_max,
        "break_on": break_on_name(args.break_on),
        "fixed_priority": args.fixed_priority,
        "tick_jitter_stddev_ns": args.tick_jitter_stddev_ns,
        "initial_tick_skew_ns": args.initial_tick_skew_ns,
        "run_jitter_cv_ppm": args.run_jitter_cv_ppm,
        "max_cgroups": args.max_cgroups,
        "expect_cgroup_exhausted": args.expect_cgroup_exhausted,
    })
}

fn print_help() {
    println!(
        "lavd_cgroup_bw_chaos --scenario throttle|r3-like|churn|lifecycle|enomem \
         --mode off|cooperative|native-concurrent|preemptive [options]"
    );
}
