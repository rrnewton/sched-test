//! Parser for rt-app JSON workload descriptions.
//!
//! Converts rt-app JSON files into simulator [`Scenario`] objects, enabling
//! the same workload to run both for real (via rt-app) and in simulation.
//!
//! # Supported rt-app features
//!
//! - `run` / `runtime` — CPU-bound work (mapped to [`Phase::Run`])
//! - `sleep` — fixed-duration sleep (mapped to [`Phase::Sleep`])
//! - `suspend` — self-suspend until resumed (mapped to `Phase::Sleep(u64::MAX)`)
//! - `resume` — wake another task (mapped to [`Phase::Wake`])
//! - `yield` — call `sched_yield()` (mapped to [`Phase::Yield`])
//! - `iorun` — buffered `write(2)` volume, resolved through the frozen
//!   `rtapp-iorun-devnull-v1` profile to one [`Phase::SystemCpu`] interval.
//!   See [`crate::safe::rtapp_iorun`]; the profile **refuses** any declaration
//!   outside the regime it was calibrated on rather than reusing the
//!   coefficient.
//! - `timer` — periodic timer (approximated as `Phase::Sleep(period)`)
//! - `priority` — nice value
//! - `loop` — repetition control
//! - `phases` — multi-phase task definitions
//! - `instance` — multiple task instances
//! - `cpus` — CPU affinity mask (parsed into `TaskDef::allowed_cpus`)
//! - task-level `taskgroup` — mapped to simulator cgroups with all CPUs allowed.
//!   Both the legacy string form (`"taskgroup": "/tg1"`) and the cgroup v2
//!   object form (`"taskgroup": { "path": "/tg1", "cpu.max": "Q P", ... }`)
//!   are accepted. Implicit ancestor cgroups along the path are synthesized.
//! - `global.duration` — scenario duration
//!
//! # rt-app++ extensions
//!
//! These keys are **not** stock rt-app. They exist because scx_layered
//! classifies tasks by attributes rt-app's language has no way to name, so a
//! real layer configuration cannot be driven from a stock spec at all. Each is
//! rejected by `scenario_to_rtapp_json` on the way back out rather than
//! silently dropped — see [`RTAPP_PP_KEYS`].
//!
//! - `thread_of: "<task>"` — **thread groups.** Every instance of this task
//!   becomes a *thread* of the process led by the first instance of `<task>`,
//!   which drives `p->group_leader` and hence `MATCH_PCOMM_PREFIX`. Naming
//!   your own task is the compact spelling of "my instances are one process".
//!   Threads inherit their process's cgroup (cgroup v2 domain mode); a thread
//!   declaring a *different* `taskgroup` is refused, because that is threaded
//!   mode and scxsim does not model it.
//! - `parent: "<task>"` — drives `p->real_parent`, hence `MATCH_PPID_EQUALS`.
//! - `uid` / `gid` — drive `real_cred->{euid,egid}`, hence
//!   `MATCH_USER_ID_EQUALS` / `MATCH_GROUP_ID_EQUALS`.
//! - `kthread: true` — sets `PF_KTHREAD`, hence `MATCH_IS_KTHREAD`.
//!
//! # Limitations
//!
//! - JSON files with duplicate keys (common in rt-app) must be preprocessed
//!   with rt-app's `workgen` script or use suffixed keys (`"run0"`, `"run1"`).
//! - Unsupported events (`lock`, `unlock`, `wait`, `signal`, `broad`, `sync`,
//!   `mem`, `barrier`, `fork`) are skipped with a warning.
//! - `iorun` is **not** skipped, and an out-of-domain `iorun` is an error
//!   rather than a warning. A dropped event fails visibly; a wrongly-modelled
//!   one does not, so the profile refuses instead of guessing.
//! - Phase-level `taskgroup` migration is not modeled.
//! - **Instance naming diverges from stock rt-app.** rt-app names every thread
//!   `<task>-<i>` where `i` is a *global* thread index, so a single-instance
//!   task still gets a suffix; this parser uses the bare name at
//!   `instance == 1` and a per-task index otherwise. Prefix rules are
//!   unaffected; exact and longer-prefix rules are not. Pinned by
//!   `known_gap_instance_naming_diverges_from_stock_rtapp`.
//! - **Stock rt-app has exactly one thread group.** It runs every task as a
//!   pthread of one process whose comm is `rt-app`, so `pcomm` is `rt-app` for
//!   every task in every stock spec. `thread_of` exists because there is
//!   nothing in rt-app to map a real thread group from.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::safe::rtapp_iorun::{self, IoRunGlobals, IoRunRefusal};
use crate::scenario::{
    sched_overhead_rbc_ns_from_env, seed_from_env, CgroupBandwidth, CgroupDef, IrqEvent, IrqType,
    NoiseConfig, OverheadConfig, Scenario, DEFAULT_WATCHDOG_TIMEOUT_NS,
};
use crate::task::{Phase, RepeatMode, TaskBehavior, TaskDef};
use crate::types::{CpuId, Gid, Pid, Uid};

/// `PF_KTHREAD` from `include/linux/sched.h`, the flag `MATCH_IS_KTHREAD`
/// tests.
const PF_KTHREAD: u32 = 0x0020_0000;

/// Errors from parsing rt-app JSON.
#[derive(Debug)]
pub enum RtAppError {
    /// JSON parse error.
    Json(serde_json::Error),
    /// Missing required field.
    MissingField(&'static str),
    /// Invalid field value.
    InvalidValue(String),
    /// Unresolved task reference in `resume`.
    UnresolvedResume(String),
    /// An rt-app++ key naming a task that does not exist.
    ///
    /// Carries the key (`thread_of` / `parent`) and the name it named. This is
    /// an error rather than a warning for the same reason `resume` is: a
    /// mistyped target would otherwise leave the relation silently unset, and
    /// the whole point of the relation is a match kind that then quietly stops
    /// firing.
    UnresolvedTaskRef {
        /// The rt-app++ key that carried the reference.
        key: &'static str,
        /// The task name it pointed at.
        name: String,
    },
    /// A `thread_of` chain — the named leader is itself a thread of a third
    /// task. The kernel has no nested thread groups.
    NestedThreadGroup {
        /// The task that declared `thread_of`.
        task: String,
        /// The task it named.
        leader: String,
        /// The task that `leader` is itself a thread of.
        leaders_leader: String,
    },
    /// A `resume` that lowered to a wake of a pid no task carries.
    ///
    /// Distinct from [`RtAppError::UnresolvedResume`], which is a name that
    /// resolved to nothing at all. This one resolved — and then the pid it
    /// resolved to turned out to belong to no task, which is what a
    /// pid-accounting slip between the two passes over `tasks` produces.
    UnresolvedWakeTarget {
        /// The task whose `resume` this was.
        waker: String,
        /// The pid it lowered to.
        target: Pid,
    },
    /// A task-level-only rt-app++ key found inside a `phases` object, where it
    /// would have no effect. Identity is a property of a task for its whole
    /// life, not of one phase.
    TaskLevelKeyInPhase(&'static str),
    /// A `thread_of` task declaring a `taskgroup` other than its leader's.
    /// That is cgroup-v2 threaded mode, which scxsim does not model.
    ThreadInForeignCgroup {
        /// The thread.
        task: String,
        /// The `taskgroup` it declared.
        own: String,
        /// Its thread-group leader.
        leader: String,
        /// The leader's `taskgroup`, if it has one.
        leaders: Option<String>,
    },
    /// An `iorun` declaration the frozen profile is not calibrated for.
    ///
    /// This is deliberately an error rather than a skipped event: silently
    /// dropping declared I/O work produces a scenario that is quietly missing
    /// it, and silently modelling it with the wrong coefficients produces a
    /// confident number about a different workload.
    UnmodelledIoRun(IoRunRefusal),
}

impl std::fmt::Display for RtAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtAppError::Json(e) => write!(f, "JSON parse error: {e}"),
            RtAppError::MissingField(field) => write!(f, "missing required field: {field}"),
            RtAppError::InvalidValue(msg) => write!(f, "invalid value: {msg}"),
            RtAppError::UnmodelledIoRun(why) => {
                write!(f, "cannot model rt-app `iorun`: {why}")
            }
            RtAppError::UnresolvedResume(name) => {
                write!(f, "unresolved resume target: {name:?}")
            }
            RtAppError::UnresolvedTaskRef { key, name } => {
                write!(f, "{key}: no task named {name:?} in this spec")
            }
            RtAppError::NestedThreadGroup {
                task,
                leader,
                leaders_leader,
            } => write!(
                f,
                "task {task:?} declares thread_of {leader:?}, but {leader:?} is itself a \
                 thread of {leaders_leader:?}; thread groups do not nest — name \
                 {leaders_leader:?} directly"
            ),
            RtAppError::UnresolvedWakeTarget { waker, target } => write!(
                f,
                "task {waker:?} resumes {target:?}, which belongs to no task in this \
                 scenario; the wake would be delivered to nothing"
            ),
            RtAppError::TaskLevelKeyInPhase(key) => write!(
                f,
                "{key:?} is a task-level key and has no meaning inside \"phases\"; \
                 move it up to the task object"
            ),
            RtAppError::ThreadInForeignCgroup {
                task,
                own,
                leader,
                leaders,
            } => write!(
                f,
                "task {task:?} is a thread of {leader:?} but declares taskgroup {own:?} \
                 while {leader:?} is in {}; under cgroup v2 every thread of a process \
                 shares its cgroup unless the cgroup is in THREADED mode, which scxsim \
                 does not model — drop the taskgroup to inherit, or make them agree",
                leaders
                    .as_deref()
                    .map_or_else(|| "the root cgroup".to_string(), |c| format!("{c:?}"))
            ),
        }
    }
}

impl From<serde_json::Error> for RtAppError {
    fn from(e: serde_json::Error) -> Self {
        RtAppError::Json(e)
    }
}

/// Strip C-style block comments (`/* ... */`) from input.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' {
            if chars.peek() == Some(&'*') {
                chars.next(); // consume '*'
                              // Skip until '*/'
                loop {
                    match chars.next() {
                        Some('*') if chars.peek() == Some(&'/') => {
                            chars.next(); // consume '/'
                            break;
                        }
                        Some(_) => continue,
                        None => break, // unterminated comment, just stop
                    }
                }
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Identify which rt-app event type a key represents, using prefix matching.
///
/// Returns `None` for non-event keys (like `loop`, `cpus`, `policy`, etc.).
fn classify_event(key: &str) -> Option<&'static str> {
    // Order matters: check longer prefixes first to avoid "run" matching "runtime"
    const EVENT_PREFIXES: &[&str] = &[
        "runtime", "run", "sleep", "timer", "suspend", "resume", "lock", "unlock", "wait",
        "signal", "broad", "sync", "mem", "iorun", "yield", "barrier", "fork",
    ];
    EVENT_PREFIXES
        .iter()
        .find(|&&prefix| key.len() >= prefix.len() && &key[..prefix.len()] == prefix)
        .copied()
}

/// Non-event keys that are valid at phase/task level.
const TASK_PHASE_KEYS: &[&str] = &[
    "loop",
    "phases",
    "instance",
    "delay",
    "policy",
    "priority",
    "cpus",
    "nodes_membind",
    "taskgroup",
    "dl-runtime",
    "dl-period",
    "dl-deadline",
    "util_min",
    "util_max",
];

/// The rt-app++ extension keys: task attributes stock rt-app has no syntax
/// for, added because scx_layered matches on them.
///
/// Kept as one list so the three places that must agree cannot drift: the
/// parser (which must not classify them as events), the phase-level rejection
/// (identity belongs to a task, not a phase), and the rt-app **export**, which
/// refuses a scenario carrying any of them rather than emitting a spec that
/// silently means something different on real hardware.
pub const RTAPP_PP_KEYS: &[&str] = &["thread_of", "parent", "uid", "gid", "kthread"];

/// Parse events from a phase/task object's key-value pairs (in insertion order).
fn parse_events(
    obj: &Map<String, Value>,
    name_to_pid: &HashMap<String, Pid>,
    io_globals: &IoRunGlobals,
) -> Result<Vec<Phase>, RtAppError> {
    let mut phases = Vec::new();

    for (key, value) in obj.iter() {
        // Skip non-event keys
        if TASK_PHASE_KEYS.contains(&key.as_str()) || RTAPP_PP_KEYS.contains(&key.as_str()) {
            continue;
        }

        let event_type = match classify_event(key) {
            Some(t) => t,
            None => {
                // Unknown key — might be a phase-level property we don't know about
                continue;
            }
        };

        match event_type {
            "run" | "runtime" => {
                let usec = value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|v| v as u64))
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected integer")))?;
                phases.push(Phase::Run(usec * 1_000)); // usec → ns
            }
            "sleep" => {
                let usec = value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|v| v as u64))
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected integer")))?;
                phases.push(Phase::Sleep(usec * 1_000));
            }
            "timer" => {
                // Timer: approximate as sleep for the period duration
                let period_usec = if let Some(obj) = value.as_object() {
                    obj.get("period").and_then(|v| v.as_u64()).unwrap_or(0)
                } else {
                    value.as_u64().unwrap_or(0)
                };
                if period_usec > 0 {
                    phases.push(Phase::Sleep(period_usec * 1_000));
                }
            }
            "suspend" => {
                // Self-suspend: sleep indefinitely until woken by resume
                phases.push(Phase::Sleep(u64::MAX));
            }
            "resume" => {
                let target_name = value
                    .as_str()
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected string")))?;
                let target_pid = name_to_pid
                    .get(target_name)
                    .ok_or_else(|| RtAppError::UnresolvedResume(target_name.to_string()))?;
                phases.push(Phase::Wake(*target_pid));
            }
            "yield" => {
                // rt-app calls sched_yield() once per loop iteration for a
                // `yield` event; the JSON value is parsed but ignored (verified
                // by strace: `"yield": 1` and `"yield": 7` both yield once).
                phases.push(Phase::Yield);
            }
            "iorun" => {
                // Declared in bytes, exactly as I/O model v1's input is — and
                // with opposite physics. Cost tracks the number of `write(2)`
                // calls, `ceil(bytes / mem_buffer_size)`, not the byte count:
                // `/dev/null` consumes the iterator without copying, so a call
                // costs the same whatever its size. `mem_buffer_size` is
                // therefore load-bearing, and the profile refuses a regime it
                // was not calibrated on instead of reusing the coefficient.
                let declared = value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|v| v as u64))
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected integer")))?;
                let resolved = rtapp_iorun::resolve(declared, io_globals)
                    .map_err(RtAppError::UnmodelledIoRun)?;
                info!(
                    profile_id = resolved.profile_id,
                    calibration_manifest_sha256 = resolved.calibration_manifest_sha256,
                    declared_bytes = resolved.declared_bytes,
                    mem_buffer_size = resolved.mem_buffer_size,
                    write_calls = resolved.write_calls,
                    system_cpu_ns = resolved.system_cpu_ns,
                    "resolved rt-app iorun through the frozen calibrated profile"
                );
                phases.push(Phase::SystemCpu(resolved.system_cpu_ns));
            }
            unsupported => {
                warn!(
                    event = unsupported,
                    key = key.as_str(),
                    "skipping unsupported rt-app event"
                );
            }
        }
    }

    Ok(phases)
}

/// Parse an rt-app CPU affinity string into a list of CPU IDs.
///
/// rt-app supports several formats:
/// - Single CPU: `"0"` or `0`
/// - Comma-separated: `"0,2,4"`
/// - Range: `"0-3"` (expands to 0,1,2,3)
/// - Mixed: `"0,2-4,6"` (expands to 0,2,3,4,6)
/// - JSON array: `[0, 1, 2]`
fn parse_cpus(value: &Value) -> Result<Option<Vec<CpuId>>, RtAppError> {
    match value {
        Value::String(s) => {
            let mut cpus = Vec::new();
            for part in s.split(',') {
                let part = part.trim();
                if let Some((start, end)) = part.split_once('-') {
                    let start: u32 = start.trim().parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid range start {start:?}"))
                    })?;
                    let end: u32 = end.trim().parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid range end {end:?}"))
                    })?;
                    for cpu in start..=end {
                        cpus.push(CpuId(cpu));
                    }
                } else {
                    let cpu: u32 = part.parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid cpu {part:?}"))
                    })?;
                    cpus.push(CpuId(cpu));
                }
            }
            if cpus.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cpus))
            }
        }
        Value::Number(n) => {
            let cpu = n.as_u64().ok_or_else(|| {
                RtAppError::InvalidValue(format!("cpus: expected unsigned integer, got {n}"))
            })? as u32;
            Ok(Some(vec![CpuId(cpu)]))
        }
        Value::Array(arr) => {
            let mut cpus = Vec::new();
            for v in arr {
                let cpu = v.as_u64().ok_or_else(|| {
                    RtAppError::InvalidValue(format!("cpus: array element not an integer: {v}"))
                })? as u32;
                cpus.push(CpuId(cpu));
            }
            if cpus.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cpus))
            }
        }
        _ => {
            warn!("cpus: unexpected type, ignoring");
            Ok(None)
        }
    }
}

/// Normalize a taskgroup path into a leading-slash form with no trailing/empty
/// segments. Returns `None` for empty / root-only inputs.
fn normalize_taskgroup_name(raw: &str) -> Option<String> {
    let parts: Vec<_> = raw
        .trim()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(format!("/{}", parts.join("/")))
    }
}

/// Parsed taskgroup spec: a normalized path plus optional `cpu.max` bandwidth.
#[derive(Debug, Clone)]
struct RtTaskgroupSpec {
    name: String,
    bandwidth: Option<CgroupBandwidth>,
}

/// Parse a `cpu.max`-style string ("`QUOTA PERIOD`" or "`max PERIOD`",
/// microseconds) into a [`CgroupBandwidth`]. Returns `Ok(None)` for the
/// unlimited (`max`) form; in that case the cgroup is created without a
/// bandwidth entry, matching the simulator's "no kernel CFS gate" semantics.
fn parse_cpu_max(value: &str) -> Result<Option<CgroupBandwidth>, RtAppError> {
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: expected 'max PERIOD' or 'QUOTA PERIOD', got {value:?}"
        )));
    }

    let period_us = parts[1].parse::<u64>().map_err(|_| {
        RtAppError::InvalidValue(format!("taskgroup.cpu.max: invalid period in {value:?}"))
    })?;
    if period_us == 0 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: period must be nonzero in {value:?}"
        )));
    }

    if parts[0] == "max" {
        return Ok(None);
    }

    let quota_us = parts[0].parse::<u64>().map_err(|_| {
        RtAppError::InvalidValue(format!("taskgroup.cpu.max: invalid quota in {value:?}"))
    })?;
    if quota_us == 0 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: quota must be nonzero in {value:?}"
        )));
    }

    Ok(Some(CgroupBandwidth {
        period_us,
        quota_us,
        burst_us: 0,
    }))
}

/// Parse a task's `taskgroup` field. Accepts:
///
/// - `"taskgroup": "/path"` (string form)
/// - `"taskgroup": { "path": "/path", "cpu.max": "QUOTA PERIOD", ... }`
///   (cgroup v2 object form, optional `cpu.weight` validated but ignored)
fn parse_taskgroup(obj: &Map<String, Value>) -> Result<Option<RtTaskgroupSpec>, RtAppError> {
    match obj.get("taskgroup") {
        Some(Value::String(name)) => {
            Ok(normalize_taskgroup_name(name).map(|name| RtTaskgroupSpec {
                name,
                bandwidth: None,
            }))
        }
        Some(Value::Object(spec)) => {
            let path = spec
                .get("path")
                .or_else(|| spec.get("name"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    RtAppError::InvalidValue(
                        "taskgroup: object requires string 'path' or 'name'".into(),
                    )
                })?;

            let Some(name) = normalize_taskgroup_name(path) else {
                return Ok(None);
            };

            if let Some(weight) = spec.get("cpu.weight").or_else(|| spec.get("cpu_weight")) {
                let weight = weight.as_u64().ok_or_else(|| {
                    RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.weight: expected integer, got {weight}"
                    ))
                })?;
                if !(1..=10_000).contains(&weight) {
                    return Err(RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.weight: expected 1..=10000, got {weight}"
                    )));
                }
            }

            let bandwidth = match spec.get("cpu.max").or_else(|| spec.get("cpu_max")) {
                Some(Value::String(cpu_max)) => parse_cpu_max(cpu_max)?,
                Some(v) => {
                    return Err(RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.max: expected string, got {v}"
                    )));
                }
                None => None,
            };

            Ok(Some(RtTaskgroupSpec { name, bandwidth }))
        }
        Some(Value::Null) | None => Ok(None),
        Some(v) => Err(RtAppError::InvalidValue(format!(
            "taskgroup: expected string or object, got {v}"
        ))),
    }
}

fn same_bandwidth(a: &CgroupBandwidth, b: &CgroupBandwidth) -> bool {
    a.period_us == b.period_us && a.quota_us == b.quota_us && a.burst_us == b.burst_us
}

/// Stash the bandwidth values from a parsed `taskgroup` so they can be
/// attached to the synthesized `CgroupDef` after all tasks are parsed. If the
/// same taskgroup is referenced by multiple tasks with conflicting `cpu.max`
/// values, fail loudly rather than silently picking one.
fn record_taskgroup_bandwidth(
    cgroup_bandwidth: &mut HashMap<String, CgroupBandwidth>,
    spec: &RtTaskgroupSpec,
) -> Result<(), RtAppError> {
    let Some(bandwidth) = &spec.bandwidth else {
        return Ok(());
    };

    if let Some(existing) = cgroup_bandwidth.get(&spec.name) {
        if !same_bandwidth(existing, bandwidth) {
            return Err(RtAppError::InvalidValue(format!(
                "taskgroup {:?}: conflicting cpu.max values across tasks",
                spec.name
            )));
        }
        return Ok(());
    }

    cgroup_bandwidth.insert(spec.name.clone(), bandwidth.clone());
    Ok(())
}

/// Synthesize implicit `CgroupDef` entries for every taskgroup path referenced
/// by a task. Each path component (e.g. `/a/b/c`) materializes one cgroup at
/// each level (`/a`, `/a/b`, `/a/b/c`), parented to the previous level so the
/// engine's hierarchy walk finds the correct ancestors. All synthesized
/// cgroups inherit the full CPU set; bandwidth is attached only to the leaf
/// path that originally carried `cpu.max` data.
fn cgroup_defs_for_tasks(
    tasks: &[TaskDef],
    nr_cpus: u32,
    cgroup_bandwidth: &HashMap<String, CgroupBandwidth>,
) -> Vec<CgroupDef> {
    let mut names = BTreeSet::new();
    for task in tasks {
        let Some(name) = &task.cgroup_name else {
            continue;
        };

        let mut path = String::new();
        for part in name.split('/').filter(|part| !part.is_empty()) {
            path.push('/');
            path.push_str(part);
            names.insert(path.clone());
        }
    }

    let all_cpus: Vec<_> = (0..nr_cpus).map(CpuId).collect();
    names
        .into_iter()
        .map(|name| {
            let parent_name = name
                .rsplit_once('/')
                .and_then(|(parent, _)| (!parent.is_empty()).then(|| parent.to_string()));
            CgroupDef {
                bandwidth: cgroup_bandwidth.get(&name).cloned(),
                name,
                parent_name,
                cpuset: Some(all_cpus.clone()),
            }
        })
        .collect()
}

/// Resolve an rt-app++ key whose value names another task, to that task's pid.
///
/// `name_to_pid` maps a bare task key to its **first** instance, which is the
/// right answer for both uses: the first instance is the thread-group leader
/// of a multi-instance task, and it is the process a `parent` reference means.
fn resolve_task_ref(
    key: &'static str,
    obj: &Map<String, Value>,
    name_to_pid: &HashMap<String, Pid>,
) -> Result<Option<Pid>, RtAppError> {
    let Some(value) = obj.get(key) else {
        return Ok(None);
    };
    let target = value
        .as_str()
        .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected a task name string")))?;
    name_to_pid
        .get(target)
        .copied()
        .map(Some)
        .ok_or_else(|| RtAppError::UnresolvedTaskRef {
            key,
            name: target.to_string(),
        })
}

/// Parse an rt-app++ `uid` / `gid` value.
fn parse_cred_id(key: &'static str, obj: &Map<String, Value>) -> Result<Option<u32>, RtAppError> {
    let Some(value) = obj.get(key) else {
        return Ok(None);
    };
    let raw = value.as_u64().ok_or_else(|| {
        RtAppError::InvalidValue(format!("{key}: expected a non-negative integer"))
    })?;
    u32::try_from(raw)
        .map(Some)
        .map_err(|_| RtAppError::InvalidValue(format!("{key}: {raw} does not fit in a uid_t")))
}

/// Reject a task-level-only rt-app++ key that appears inside a `phases`
/// object, where the parser would otherwise skip it without a word.
fn reject_task_level_keys_in_phase(phase: &Map<String, Value>) -> Result<(), RtAppError> {
    for key in RTAPP_PP_KEYS {
        if phase.contains_key(*key) {
            return Err(RtAppError::TaskLevelKeyInPhase(key));
        }
    }
    Ok(())
}

/// Parse a single rt-app task object into one or more `TaskDef`s.
///
/// Multiple `TaskDef`s are produced when `instance > 1`.
fn parse_task(
    name: &str,
    obj: &Map<String, Value>,
    pid_start: &mut i32,
    name_to_pid: &HashMap<String, Pid>,
    cgroup_bandwidth: &mut HashMap<String, CgroupBandwidth>,
    io_globals: &IoRunGlobals,
) -> Result<Vec<TaskDef>, RtAppError> {
    let instance_count = obj.get("instance").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

    let nice = obj
        .get("priority")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .clamp(-20, 19) as i8;

    let loop_count = obj.get("loop").and_then(|v| v.as_i64()).unwrap_or(-1);

    // Parse CPU affinity
    let allowed_cpus = if let Some(cpus_val) = obj.get("cpus") {
        parse_cpus(cpus_val)?
    } else {
        None
    };

    let taskgroup = parse_taskgroup(obj)?;
    if let Some(spec) = &taskgroup {
        record_taskgroup_bandwidth(cgroup_bandwidth, spec)?;
    }
    let cgroup_name = taskgroup.map(|spec| spec.name);

    // rt-app++ identity keys. Resolution happens here; the `thread_of` nesting
    // and cgroup-inheritance checks need every task and so run in load_rtapp.
    let thread_of = resolve_task_ref("thread_of", obj, name_to_pid)?;
    let parent_pid = resolve_task_ref("parent", obj, name_to_pid)?;
    let uid = Uid(parse_cred_id("uid", obj)?.unwrap_or(0));
    let gid = Gid(parse_cred_id("gid", obj)?.unwrap_or(0));
    let task_flags = match obj.get("kthread") {
        None | Some(Value::Bool(false)) => 0,
        Some(Value::Bool(true)) => PF_KTHREAD,
        Some(v) => {
            return Err(RtAppError::InvalidValue(format!(
                "kthread: expected true or false, got {v}"
            )))
        }
    };

    // Parse phases
    let all_phases = if let Some(phases_val) = obj.get("phases") {
        // Multi-phase task: each sub-object is a named phase
        let phases_obj = phases_val
            .as_object()
            .ok_or_else(|| RtAppError::InvalidValue("phases: expected object".into()))?;

        let mut all = Vec::new();
        for (_phase_name, phase_val) in phases_obj.iter() {
            let phase_obj = phase_val
                .as_object()
                .ok_or_else(|| RtAppError::InvalidValue("phase: expected object".into()))?;
            reject_task_level_keys_in_phase(phase_obj)?;

            let phase_loop = phase_obj.get("loop").and_then(|v| v.as_i64()).unwrap_or(1);

            let events = parse_events(phase_obj, name_to_pid, io_globals)?;

            if phase_loop <= 0 || phase_loop == 1 {
                all.extend(events);
            } else {
                for _ in 0..phase_loop {
                    all.extend(events.clone());
                }
            }
        }
        all
    } else {
        // Single-phase task: events are directly in the task object
        parse_events(obj, name_to_pid, io_globals)?
    };

    if all_phases.is_empty() {
        warn!(task = name, "task has no events, skipping");
        return Ok(Vec::new());
    }

    // Handle loop: -1 means repeat forever, N>1 uses RepeatMode::Count
    let (final_phases, repeat) = if loop_count < 0 {
        (all_phases, RepeatMode::Forever)
    } else if loop_count <= 1 {
        (all_phases, RepeatMode::Once)
    } else {
        (all_phases, RepeatMode::Count(loop_count as u32))
    };

    // Create task instances
    let mut defs = Vec::new();
    for i in 0..instance_count {
        let task_name = if instance_count == 1 {
            name.to_string()
        } else {
            format!("{name}-{i}")
        };
        let pid = Pid(*pid_start);
        *pid_start += 1;

        defs.push(TaskDef {
            name: task_name,
            pid,
            nice,
            behavior: TaskBehavior {
                phases: final_phases.clone(),
                repeat,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: allowed_cpus.clone(),
            parent_pid,
            cgroup_name: cgroup_name.clone(),
            task_flags,
            migration_disabled: 0,
            thread_group_leader: thread_of,
            uid,
            gid,
            fork_cpu: None,
        });
    }

    Ok(defs)
}

/// Extract run and sleep durations (in ns) from an irq_gen task definition.
///
/// Supports both flat (`"run": N, "sleep": M`) and phased layouts
/// (`"phases": { "irq_work": { "run": N }, "idle": { "sleep": M } }`).
fn extract_irq_gen_timing(obj: &Map<String, Value>) -> Result<(u64, u64), RtAppError> {
    // Try phased layout first (the v09 config uses this).
    if let Some(phases_val) = obj.get("phases") {
        if let Some(phases_obj) = phases_val.as_object() {
            let mut run_ns: Option<u64> = None;
            let mut sleep_ns: Option<u64> = None;

            for (_phase_name, phase_val) in phases_obj.iter() {
                if let Some(phase_obj) = phase_val.as_object() {
                    if let Some(r) = phase_obj
                        .get("run")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                    {
                        run_ns = Some(r * 1_000); // usec → ns
                    }
                    if let Some(s) = phase_obj
                        .get("sleep")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                    {
                        sleep_ns = Some(s * 1_000);
                    }
                }
            }

            if let (Some(r), Some(s)) = (run_ns, sleep_ns) {
                return Ok((r, s));
            }
        }
    }

    // Try flat layout.
    let run_us = obj
        .get("run")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)));
    let sleep_us = obj
        .get("sleep")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)));

    match (run_us, sleep_us) {
        (Some(r), Some(s)) => Ok((r * 1_000, s * 1_000)),
        _ => Err(RtAppError::InvalidValue(
            "irq_gen task missing run/sleep durations".into(),
        )),
    }
}

/// Resolve the rt-app++ cross-task relations once every task exists.
///
/// Three things `parse_task` cannot do on its own, because they need the whole
/// task set:
///
/// 1. **Check the reference resolves to a task.** A name can resolve through
///    `name_to_pid` and still name a pid no `TaskDef` carries: `irq_gen*`
///    entries occupy a pid slot and then become [`IrqEvent`]s rather than
///    tasks, so `"thread_of": "irq_gen0"` parses and would panic in the
///    engine. Caught here instead.
/// 2. **Refuse a `thread_of` chain.** The kernel's `group_leader` always
///    points at a leader; flattening a chain would put a plausible but wrong
///    string in front of `MATCH_PCOMM_PREFIX`.
/// 3. **Give a thread its process's cgroup.** Under cgroup v2's default domain
///    mode every thread of a process is in the process's cgroup. A thread that
///    declares no `taskgroup` inherits the leader's; one that declares a
///    *different* one is asking for threaded mode, which scxsim does not
///    model, and is refused rather than silently run as if it were the
///    ordinary case.
fn resolve_thread_groups(tasks: &mut [TaskDef]) -> Result<(), RtAppError> {
    // Snapshot what the checks below need, so the mutable pass does not have
    // to borrow the slice again.
    let by_pid: HashMap<Pid, (String, Option<Pid>, Option<String>)> = tasks
        .iter()
        .map(|t| {
            (
                t.pid,
                (
                    t.name.clone(),
                    t.thread_group_leader.filter(|l| *l != t.pid),
                    t.cgroup_name.clone(),
                ),
            )
        })
        .collect();
    let name_of = |pid: Pid| -> String {
        by_pid
            .get(&pid)
            .map_or_else(|| format!("{pid:?}"), |(name, _, _)| name.clone())
    };

    for def in tasks.iter_mut() {
        for (key, target) in [
            ("thread_of", def.thread_group_leader),
            ("parent", def.parent_pid),
        ] {
            let Some(target) = target else { continue };
            if !by_pid.contains_key(&target) {
                return Err(RtAppError::UnresolvedTaskRef {
                    key,
                    name: name_of(target),
                });
            }
        }

        let Some(leader) = def.thread_group_leader.filter(|l| *l != def.pid) else {
            continue;
        };
        let (_, leaders_leader, leader_cgroup) = &by_pid[&leader];
        if let Some(leaders_leader) = leaders_leader {
            return Err(RtAppError::NestedThreadGroup {
                task: def.name.clone(),
                leader: name_of(leader),
                leaders_leader: name_of(*leaders_leader),
            });
        }
        match (&def.cgroup_name, leader_cgroup) {
            (None, inherited) => def.cgroup_name = inherited.clone(),
            (Some(own), leaders) if Some(own) != leaders.as_ref() => {
                return Err(RtAppError::ThreadInForeignCgroup {
                    task: def.name.clone(),
                    own: own.clone(),
                    leader: name_of(leader),
                    leaders: leaders.clone(),
                })
            }
            _ => {}
        }
    }
    Ok(())
}

/// Check that every `resume` lowered to a wake of a task that exists.
///
/// `parse_events` resolves a `resume` target through `name_to_pid`, which is
/// built by a separate first pass over the same `tasks` object. If the two
/// passes ever disagree about how many pid slots an entry consumes, the name
/// resolves and the pid is wrong — and a `Phase::Wake` naming a pid no task
/// carries is delivered to nothing, with no error and no warning.
///
/// The disagreement that existed (`irq_gen*` entries with `instance > 1`) is
/// fixed at its source in `load_rtapp`. This is the check that would have
/// caught it, and catches the next one.
fn validate_wake_targets(tasks: &[TaskDef]) -> Result<(), RtAppError> {
    let live: BTreeSet<Pid> = tasks.iter().map(|t| t.pid).collect();
    for def in tasks {
        for phase in &def.behavior.phases {
            let Phase::Wake(target) = phase else { continue };
            if !live.contains(target) {
                return Err(RtAppError::UnresolvedWakeTarget {
                    waker: def.name.clone(),
                    target: *target,
                });
            }
        }
    }
    Ok(())
}

/// Load an rt-app JSON workload and convert it to a simulator [`Scenario`].
///
/// # Arguments
///
/// * `json_str` — Raw JSON string (may contain C-style `/* */` comments).
/// * `nr_cpus` — Number of simulated CPUs (rt-app doesn't specify this).
///
/// # Example
///
/// ```rust,no_run
/// use scx_simulator::load_rtapp;
///
/// let json = r#"{
///     "global": { "duration": 1 },
///     "tasks": {
///         "worker": {
///             "loop": -1,
///             "run": 5000,
///             "sleep": 5000
///         }
///     }
/// }"#;
///
/// let scenario = load_rtapp(json, 4).unwrap();
/// ```
pub fn load_rtapp(json_str: &str, nr_cpus: u32) -> Result<Scenario, RtAppError> {
    if nr_cpus == 0 {
        return Err(RtAppError::InvalidValue(
            "nr_cpus must be at least 1".into(),
        ));
    }
    let cleaned = strip_comments(json_str);
    let root: Value = serde_json::from_str(&cleaned)?;
    let root_obj = root
        .as_object()
        .ok_or(RtAppError::MissingField("root object"))?;

    // Parse global settings
    let duration_ns = if let Some(global) = root_obj.get("global") {
        let dur_secs = global
            .get("duration")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        if dur_secs > 0 {
            dur_secs as u64 * 1_000_000_000
        } else {
            // Default: 10 seconds if not specified or infinite
            10_000_000_000
        }
    } else {
        10_000_000_000
    };

    // The two globals the `iorun` profile requires. Both fall back to
    // rt-app's own defaults (`parse_global`), because that is what rt-app
    // itself would use — and `mem_buffer_size`'s default of 4 MiB is large
    // enough that a plausible-looking `iorun` becomes a handful of syscalls,
    // which is precisely why the profile refuses rather than guesses.
    let io_globals = root_obj
        .get("global")
        .and_then(|g| g.as_object())
        .map(|g| IoRunGlobals {
            io_device: g
                .get("io_device")
                .and_then(|v| v.as_str())
                .unwrap_or(rtapp_iorun::RTAPP_IORUN_DEFAULT_DEVICE)
                .to_string(),
            mem_buffer_size: g
                .get("mem_buffer_size")
                .and_then(|v| v.as_u64())
                .unwrap_or(rtapp_iorun::RTAPP_IORUN_DEFAULT_MEM_BUFFER_SIZE),
        })
        .unwrap_or_default();

    let tasks_obj = root_obj
        .get("tasks")
        .and_then(|v| v.as_object())
        .ok_or(RtAppError::MissingField("tasks"))?;

    // First pass: assign PIDs to build name→pid map
    let mut name_to_pid: HashMap<String, Pid> = HashMap::new();
    let mut pid_counter: i32 = 1;
    for (task_name, task_val) in tasks_obj.iter() {
        let instance_count = task_val
            .as_object()
            .and_then(|o| o.get("instance"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

        if instance_count == 1 {
            name_to_pid.insert(task_name.clone(), Pid(pid_counter));
            pid_counter += 1;
        } else {
            // Map the base name to the first instance
            name_to_pid.insert(task_name.clone(), Pid(pid_counter));
            for i in 0..instance_count {
                let inst_name = format!("{task_name}-{i}");
                name_to_pid.insert(inst_name, Pid(pid_counter));
                pid_counter += 1;
            }
        }
    }

    // Second pass: parse tasks with name→pid resolution.
    // Detect irq_gen_* tasks and convert them to IrqEvent entries instead
    // of regular tasks.  In production, IRQs fire outside the scheduler's
    // control; the irq_gen tasks in rt-app specs are only meaningful for
    // VM/pinned modes where they generate real softirqs.  In simulation we
    // must model them as simulator-level IRQ injection so that
    // bpf_in_serving_softirq(), rq->clock_task, etc. behave correctly.
    let mut all_tasks: Vec<TaskDef> = Vec::new();
    let mut irq_events: Vec<IrqEvent> = Vec::new();
    let mut cgroup_bandwidth: HashMap<String, CgroupBandwidth> = HashMap::new();
    let mut pid_counter: i32 = 1;
    for (task_name, task_val) in tasks_obj.iter() {
        let task_obj = task_val.as_object().ok_or_else(|| {
            RtAppError::InvalidValue(format!("task {task_name}: expected object"))
        })?;

        if task_name.starts_with("irq_gen") {
            // Convert to periodic IrqEvents instead of a scheduled task.
            //
            // Still consume PID slots to keep PID numbering stable — one per
            // INSTANCE, because that is what the first pass above consumed
            // when it built `name_to_pid`. Consuming exactly one (as this did
            // until the rt-app++ work) makes the two passes disagree the
            // moment an irq_gen entry declares `instance > 1`, and every task
            // parsed after it gets a pid lower than the one `name_to_pid`
            // published. Measured: with `"instance": 3` on an irq_gen entry, a
            // later `"resume": "later"` lowered to `Wake(Pid(4))` while the
            // task named `later` had been given `Pid(2)` — a wake delivered to
            // nothing, silently. `validate_wake_targets` is the belt to this
            // brace.
            pid_counter += task_obj
                .get("instance")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as i32;

            // Extract the pinned CPU from the affinity mask.
            let cpu = if let Some(cpus_val) = task_obj.get("cpus") {
                let cpus = parse_cpus(cpus_val)?;
                match cpus {
                    Some(ref c) if c.len() == 1 => {
                        if c[0].0 >= nr_cpus {
                            return Err(RtAppError::InvalidValue(format!(
                                "irq_gen task {task_name:?} targets CPU {}, but only \
                                 {nr_cpus} CPUs are configured (valid range: 0..{})",
                                c[0].0,
                                nr_cpus - 1,
                            )));
                        }
                        c[0]
                    }
                    _ => {
                        warn!(
                            task = task_name.as_str(),
                            "irq_gen task without single-CPU pinning; using CPU 0"
                        );
                        CpuId(0)
                    }
                }
            } else {
                warn!(
                    task = task_name.as_str(),
                    "irq_gen task without cpus field; using CPU 0"
                );
                CpuId(0)
            };

            // Extract run and sleep durations from phases (or top-level).
            let (run_ns, sleep_ns) = extract_irq_gen_timing(task_obj)?;
            let period_ns = run_ns + sleep_ns;

            // Generate periodic softirq events for the full duration.
            let events_before = irq_events.len();
            let mut t: u64 = 0;
            while t <= duration_ns {
                irq_events.push(IrqEvent {
                    cpu,
                    at_ns: t,
                    duration_ns: run_ns,
                    irq_type: IrqType::SoftIrq,
                    wake_pids: Vec::new(),
                });
                t += period_ns;
            }
            let events_added = irq_events.len() - events_before;

            info!(
                task = task_name.as_str(),
                cpu = cpu.0,
                run_us = run_ns / 1_000,
                period_us = period_ns / 1_000,
                events = events_added,
                "converted irq_gen task to periodic softirq events"
            );
            continue;
        }

        let defs = parse_task(
            task_name,
            task_obj,
            &mut pid_counter,
            &name_to_pid,
            &mut cgroup_bandwidth,
            &io_globals,
        )?;
        all_tasks.extend(defs);
    }

    if all_tasks.is_empty() {
        return Err(RtAppError::InvalidValue(
            "no tasks with events found".into(),
        ));
    }

    resolve_thread_groups(&mut all_tasks)?;
    validate_wake_targets(&all_tasks)?;

    // Validate CPU affinity entries against available CPUs.
    for def in &all_tasks {
        if let Some(ref cpus) = def.allowed_cpus {
            for cpu in cpus {
                if cpu.0 >= nr_cpus {
                    return Err(RtAppError::InvalidValue(format!(
                        "task {:?} has CPU affinity for CPU {}, but only {} CPUs \
                         are configured (valid range: 0..{})",
                        def.name,
                        cpu.0,
                        nr_cpus,
                        nr_cpus - 1,
                    )));
                }
            }
        }
    }

    let cgroups = cgroup_defs_for_tasks(&all_tasks, nr_cpus, &cgroup_bandwidth);

    Ok(Scenario {
        nr_cpus,
        required_scheduler_identity: None,
        smt_threads_per_core: 1,
        cpus_per_llc: 0,
        // rt-app files describe a workload, not a machine: one flat node,
        // one LLC, no SMT. `ForkPlacement::Auto` on one node is CPU 0, which
        // is what this path did before topology became explicit.
        topology: crate::topology::MachineTopology::uniform(nr_cpus, 0, 1, 1),
        fork_placement: crate::scenario::ForkPlacement::Auto,
        tasks: all_tasks,
        cgroups,
        duration_ns,
        noise: NoiseConfig::from_env(),
        overhead: OverheadConfig::from_env(),
        seed: seed_from_env(),
        fixed_priority: false,
        sched_overhead_rbc_ns: sched_overhead_rbc_ns_from_env(),
        // Explicit iff the operator actually set SCX_SIM_RBC_NS; the
        // unset case yields the Some(10) default, which must not hard-fail
        // on a PMU-less host.
        rbc_explicitly_requested: std::env::var_os("SCX_SIM_RBC_NS").is_some(),
        watchdog_timeout_ns: Some(DEFAULT_WATCHDOG_TIMEOUT_NS),
        ignore_bpf_errors: true,
        hotplug_events: Vec::new(),
        cpu_preempt_events: Vec::new(),
        cgroup_migrate_events: Vec::new(),
        task_rename_events: Vec::new(),
        cgroup_create_events: Vec::new(),
        cgroup_destroy_events: Vec::new(),
        cgroup_cpuset_change_events: Vec::new(),
        interleave: false,
        stochastic_timer_interleave: false,
        stochastic_timer_interleave_window_ns: 20_000_000,
        stochastic_timer_interleave_one_in: 4,
        targeted_cbw_yield_sites: false,
        targeted_cbw_yield_window_ns: 100_000_000,
        targeted_cbw_yield_limit: 1,
        preemptive: None,
        replay_trace: None,
        no_pmu_signal: false,
        max_cgroups: crate::cgroup::DEFAULT_MAX_CGROUPS,
        irq_events,
        futex_events: Vec::new(),
        native_concurrent: None,
        wait_debugger: false,
        warmup_ns: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_comments() {
        assert_eq!(strip_comments("hello /* world */ foo"), "hello  foo");
        assert_eq!(strip_comments("no comments"), "no comments");
        assert_eq!(strip_comments("a /* b /* nested */ c"), "a  c");
    }

    #[test]
    fn test_classify_event() {
        assert_eq!(classify_event("run"), Some("run"));
        assert_eq!(classify_event("run0"), Some("run"));
        assert_eq!(classify_event("runtime"), Some("runtime"));
        assert_eq!(classify_event("runtime1"), Some("runtime"));
        assert_eq!(classify_event("sleep"), Some("sleep"));
        assert_eq!(classify_event("sleep0"), Some("sleep"));
        assert_eq!(classify_event("timer"), Some("timer"));
        assert_eq!(classify_event("suspend"), Some("suspend"));
        assert_eq!(classify_event("resume"), Some("resume"));
        assert_eq!(classify_event("lock"), Some("lock"));
        assert_eq!(classify_event("loop"), None);
        assert_eq!(classify_event("cpus"), None);
        assert_eq!(classify_event("priority"), None);
    }

    #[test]
    fn test_simple_workload() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "worker": {
                    "loop": -1,
                    "run": 5000,
                    "sleep": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.nr_cpus, 2);
        assert_eq!(scenario.duration_ns, 1_000_000_000);
        assert_eq!(scenario.tasks.len(), 1);

        let task = &scenario.tasks[0];
        assert_eq!(task.name, "worker");
        assert_eq!(task.nice, 0);
        assert_eq!(task.behavior.repeat, RepeatMode::Forever);
        assert_eq!(task.behavior.phases.len(), 2);
        assert!(matches!(task.behavior.phases[0], Phase::Run(5_000_000)));
        assert!(matches!(task.behavior.phases[1], Phase::Sleep(5_000_000)));
    }

    #[test]
    fn test_suspend_resume() {
        let json = r#"{
            "tasks": {
                "producer": {
                    "loop": -1,
                    "run": 10000,
                    "resume": "consumer",
                    "sleep": 20000
                },
                "consumer": {
                    "loop": -1,
                    "suspend": "consumer",
                    "run": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.tasks.len(), 2);

        let producer = &scenario.tasks[0];
        assert_eq!(producer.name, "producer");
        assert_eq!(producer.behavior.phases.len(), 3);
        assert!(matches!(
            producer.behavior.phases[0],
            Phase::Run(10_000_000)
        ));
        assert!(matches!(producer.behavior.phases[1], Phase::Wake(Pid(2)))); // consumer pid
        assert!(matches!(
            producer.behavior.phases[2],
            Phase::Sleep(20_000_000)
        ));

        let consumer = &scenario.tasks[1];
        assert_eq!(consumer.name, "consumer");
        assert_eq!(consumer.behavior.phases.len(), 2);
        assert!(matches!(
            consumer.behavior.phases[0],
            Phase::Sleep(u64::MAX)
        )); // suspend
        assert!(matches!(consumer.behavior.phases[1], Phase::Run(5_000_000)));
    }

    #[test]
    fn test_multi_phase() {
        let json = r#"{
            "tasks": {
                "task1": {
                    "loop": -1,
                    "phases": {
                        "active": {
                            "loop": 2,
                            "run": 1000
                        },
                        "idle": {
                            "sleep": 5000
                        }
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        let task = &scenario.tasks[0];
        // phase "active" with loop=2 should expand to 2 runs, then 1 sleep
        assert_eq!(task.behavior.phases.len(), 3);
        assert!(matches!(task.behavior.phases[0], Phase::Run(1_000_000)));
        assert!(matches!(task.behavior.phases[1], Phase::Run(1_000_000)));
        assert!(matches!(task.behavior.phases[2], Phase::Sleep(5_000_000)));
    }

    #[test]
    fn test_instances() {
        let json = r#"{
            "tasks": {
                "worker": {
                    "instance": 3,
                    "loop": -1,
                    "run": 10000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();
        assert_eq!(scenario.tasks.len(), 3);
        assert_eq!(scenario.tasks[0].name, "worker-0");
        assert_eq!(scenario.tasks[1].name, "worker-1");
        assert_eq!(scenario.tasks[2].name, "worker-2");
        // Each should have unique PIDs
        assert_ne!(scenario.tasks[0].pid, scenario.tasks[1].pid);
    }

    #[test]
    fn test_nice_priority() {
        let json = r#"{
            "tasks": {
                "high": { "priority": -19, "loop": -1, "run": 1000 },
                "low":  { "priority": 10, "loop": -1, "run": 1000 }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        assert_eq!(scenario.tasks[0].nice, -19);
        assert_eq!(scenario.tasks[1].nice, 10);
    }

    #[test]
    fn test_comments_and_timer() {
        let json = r#"{
            /* This is a comment */
            "tasks": {
                "periodic": {
                    "loop": -1,
                    "run": 2000,
                    "timer": { "ref": "tick", "period": 16667 }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        let task = &scenario.tasks[0];
        assert_eq!(task.behavior.phases.len(), 2);
        assert!(matches!(task.behavior.phases[0], Phase::Run(2_000_000)));
        // timer period 16667 usec = 16667000 ns
        assert!(matches!(task.behavior.phases[1], Phase::Sleep(16_667_000)));
    }

    // ---- iorun: the second calibrated I/O profile ------------------------
    //
    // Evidence: experiments/io_model_rtapp_iorun_20260814/. The profile scored
    // 70/70 blind at ARE <= 20%; v1's coefficients applied to the same bytes
    // scored 0/70.

    fn iorun_json(declared: u64, mem_buffer_size: u64, io_device: &str) -> String {
        serde_json::json!({
            "tasks": { "io0": { "instance": 1, "loop": 1, "iorun": declared } },
            "global": {
                "duration": 1,
                "io_device": io_device,
                "mem_buffer_size": mem_buffer_size
            }
        })
        .to_string()
    }

    #[test]
    fn iorun_lowers_to_one_system_cpu_phase_and_nothing_else() {
        let s = load_rtapp(&iorun_json(33_554_432, 16_384, "/dev/null"), 4).unwrap();
        assert_eq!(s.tasks.len(), 1);
        // 2048 write() calls at the frozen 218625/2048 ns each, and exactly
        // one phase: no NonRunning, no Park, no alternation structure.
        assert_eq!(s.tasks[0].behavior.phases.len(), 1);
        assert!(matches!(
            s.tasks[0].behavior.phases[0],
            Phase::SystemCpu(218_625)
        ));
    }

    /// The claim the whole profile exists for. Same declared bytes, different
    /// `mem_buffer_size`, eight times the cost. A byte-count model is flat here.
    #[test]
    fn iorun_cost_tracks_calls_not_declared_bytes() {
        let bytes = 1_073_741_824;
        let coarse = load_rtapp(&iorun_json(bytes, 262_144, "/dev/null"), 4).unwrap();
        let fine = load_rtapp(&iorun_json(bytes, 32_768, "/dev/null"), 4).unwrap();
        let ns = |s: &Scenario| match s.tasks[0].behavior.phases[0] {
            Phase::SystemCpu(ns) => ns,
            ref other => panic!("expected SystemCpu, got {other:?}"),
        };
        assert_eq!(ns(&coarse), 437_250);
        assert_eq!(ns(&fine), 3_498_000);
        assert_eq!(ns(&fine), ns(&coarse) * 8);
    }

    /// `iorun` used to be dropped with a warning. A dropped event fails
    /// visibly; a wrongly-modelled one does not. Out of domain is now an error.
    #[test]
    fn out_of_domain_iorun_is_an_error_not_a_silent_drop() {
        // A real device reintroduces blocking the profile never measured.
        let err = load_rtapp(&iorun_json(33_554_432, 16_384, "/dev/vda"), 4).unwrap_err();
        assert!(matches!(err, RtAppError::UnmodelledIoRun(_)), "{err}");
        assert!(err.to_string().contains("/dev/vda"), "{err}");

        // Above rt-app's own 32-bit saturation point.
        let err = load_rtapp(&iorun_json(4_294_967_296, 65_536, "/dev/null"), 4).unwrap_err();
        assert!(matches!(err, RtAppError::UnmodelledIoRun(_)), "{err}");

        // Too few calls to have been calibrated.
        let err = load_rtapp(&iorun_json(4_194_304, 1_048_576, "/dev/null"), 4).unwrap_err();
        assert!(matches!(err, RtAppError::UnmodelledIoRun(_)), "{err}");
    }

    /// rt-app defaults `io_device` to `/dev/null` and `mem_buffer_size` to
    /// 4 MiB, so a config that omits both must be resolved against *those*
    /// values rather than against anything convenient.
    #[test]
    fn omitted_globals_fall_back_to_rtapp_defaults() {
        let json = serde_json::json!({
            "tasks": { "io0": { "instance": 1, "loop": 1, "iorun": 33_554_432u64 } },
            "global": { "duration": 1 }
        })
        .to_string();
        // 33554432 / 4 MiB = 8 calls, far below the calibrated floor: refused.
        let err = load_rtapp(&json, 4).unwrap_err();
        assert!(matches!(err, RtAppError::UnmodelledIoRun(_)), "{err}");
        assert!(err.to_string().contains("mem_buffer_size"), "{err}");
    }

    /// `iorun` composes with the events that were already supported, in order.
    #[test]
    fn iorun_composes_with_other_events_in_declaration_order() {
        let json = serde_json::json!({
            "tasks": { "io0": { "instance": 1, "loop": 1,
                "run": 500, "iorun": 33_554_432u64, "sleep": 250 } },
            "global": { "duration": 1, "io_device": "/dev/null",
                        "mem_buffer_size": 16_384 }
        })
        .to_string();
        let s = load_rtapp(&json, 4).unwrap();
        let phases = &s.tasks[0].behavior.phases;
        assert_eq!(phases.len(), 3);
        assert!(matches!(phases[0], Phase::Run(500_000)));
        assert!(matches!(phases[1], Phase::SystemCpu(218_625)));
        assert!(matches!(phases[2], Phase::Sleep(250_000)));
    }

    #[test]
    fn test_parse_cpus_string_single() {
        let v = serde_json::json!("0");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0)]));
    }

    #[test]
    fn test_parse_cpus_string_list() {
        let v = serde_json::json!("0,2,4");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0), CpuId(2), CpuId(4)]));
    }

    #[test]
    fn test_parse_cpus_string_range() {
        let v = serde_json::json!("1-3");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(1), CpuId(2), CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_string_mixed() {
        let v = serde_json::json!("0,2-4,6");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(
            cpus,
            Some(vec![CpuId(0), CpuId(2), CpuId(3), CpuId(4), CpuId(6)])
        );
    }

    #[test]
    fn test_parse_cpus_number() {
        let v = serde_json::json!(3);
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_array() {
        let v = serde_json::json!([0, 1, 3]);
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0), CpuId(1), CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_in_workload() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "pinned": {
                    "loop": -1,
                    "cpus": "0,1",
                    "run": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();
        let task = &scenario.tasks[0];
        assert_eq!(task.allowed_cpus, Some(vec![CpuId(0), CpuId(1)]));
    }

    #[test]
    fn test_cpu_affinity_oob_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "pinned": {
                    "loop": -1,
                    "cpus": [99, 100],
                    "run": 5000,
                    "sleep": 5000
                }
            }
        }"#;

        let err = load_rtapp(json, 4).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CPU 99") && msg.contains("4 CPUs"),
            "expected clear OOB error, got: {msg}"
        );
    }

    #[test]
    fn test_zero_cpus_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "t1": { "loop": -1, "run": 5000 }
            }
        }"#;

        let err = load_rtapp(json, 0).unwrap_err();
        assert!(
            err.to_string().contains("nr_cpus must be at least 1"),
            "expected nr_cpus validation error, got: {err}"
        );
    }

    // -- taskgroup → implicit cgroup synthesis (Diff 2/5) --

    #[test]
    fn test_normalize_taskgroup_name_simple() {
        assert_eq!(normalize_taskgroup_name("/tg1").as_deref(), Some("/tg1"));
        assert_eq!(normalize_taskgroup_name("tg1").as_deref(), Some("/tg1"));
        assert_eq!(
            normalize_taskgroup_name("/a/b/c").as_deref(),
            Some("/a/b/c")
        );
        assert_eq!(
            normalize_taskgroup_name("//a//b//").as_deref(),
            Some("/a/b")
        );
        assert_eq!(normalize_taskgroup_name("/"), None);
        assert_eq!(normalize_taskgroup_name(""), None);
    }

    #[test]
    fn test_taskgroup_string_form_synthesizes_cgroup() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "sleep": 80000,
                    "taskgroup": "/tg_string"
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();

        // Implicit cgroup synthesized from the taskgroup field.
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/tg_string");
        assert_eq!(scenario.cgroups[0].parent_name, None);
        // No cpu.max in string form -> no bandwidth.
        assert!(scenario.cgroups[0].bandwidth.is_none());
        // All-CPUs cpuset.
        assert_eq!(
            scenario.cgroups[0].cpuset,
            Some(vec![CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        );
        // Task assignment.
        assert_eq!(scenario.tasks[0].cgroup_name.as_deref(), Some("/tg_string"));
    }

    #[test]
    fn test_taskgroup_object_form_with_cpu_max() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/tg_v2",
                        "cpu.weight": 250,
                        "cpu.max": "200000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/tg_v2");
        assert_eq!(scenario.tasks[0].cgroup_name.as_deref(), Some("/tg_v2"));

        let bw = scenario.cgroups[0]
            .bandwidth
            .as_ref()
            .expect("expected cpu.max bandwidth");
        assert_eq!(bw.period_us, 100_000);
        assert_eq!(bw.quota_us, 200_000);
        assert_eq!(bw.burst_us, 0);
    }

    #[test]
    fn test_taskgroup_object_form_cpu_max_max_is_unlimited() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/unlimited",
                        "cpu.max": "max 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/unlimited");
        assert!(scenario.cgroups[0].bandwidth.is_none());
    }

    #[test]
    fn test_taskgroup_nested_path_synthesizes_ancestors() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "leaf": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/a/b/c",
                        "cpu.max": "10000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        // /a, /a/b, /a/b/c — three implicit cgroups (BTreeSet -> sorted).
        let names: Vec<_> = scenario.cgroups.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/a", "/a/b", "/a/b/c"]);

        // Hierarchy: each level parents the next.
        assert_eq!(scenario.cgroups[0].parent_name, None);
        assert_eq!(scenario.cgroups[1].parent_name.as_deref(), Some("/a"));
        assert_eq!(scenario.cgroups[2].parent_name.as_deref(), Some("/a/b"));

        // Bandwidth attached only to the leaf.
        assert!(scenario.cgroups[0].bandwidth.is_none());
        assert!(scenario.cgroups[1].bandwidth.is_none());
        let leaf_bw = scenario.cgroups[2]
            .bandwidth
            .as_ref()
            .expect("leaf cgroup should carry the cpu.max bandwidth");
        assert_eq!(leaf_bw.quota_us, 10_000);
        assert_eq!(leaf_bw.period_us, 100_000);
    }

    #[test]
    fn test_taskgroup_conflicting_cpu_max_rejected() {
        // Two tasks reference the same taskgroup path but with different
        // cpu.max values — must be rejected, not silently merged.
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "a": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/shared", "cpu.max": "10000 100000" }
                },
                "b": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/shared", "cpu.max": "20000 100000" }
                }
            }
        }"#;

        let err = load_rtapp(json, 2).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting cpu.max"),
            "expected conflicting-cpu.max error, got: {msg}"
        );
    }

    #[test]
    fn test_taskgroup_invalid_cpu_max_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "t": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/x", "cpu.max": "10000" }
                }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(err.to_string().contains("expected 'max PERIOD'"));
    }

    #[test]
    fn test_taskgroup_only_spec_synthesizes_cgroup_with_bandwidth() {
        // End-to-end parser test: a taskgroup-only rt-app spec (no top-level
        // `cgroup` block) is parsed into Scenario.cgroups with the per-cgroup
        // bandwidth fields populated from the inline `cpu.max` field.
        //
        // (Pre-shrink this test also exercised the engine-side
        // BandwidthManager bridge -- removed in `tg shrink-rust-
        // bandwidthmanager-518-to-30-lines-no-fake-approximation`. The
        // library's `scx_cgroup_bw_init` now configures per-cgroup
        // bandwidth state via the `cgroup_set_bandwidth` callback at
        // scenario load; the engine no longer maintains a parallel mirror.)

        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "stop_worker": {
                    "instance": 4,
                    "loop": -1,
                    "run": 100000,
                    "taskgroup": {
                        "path": "/test_bw_stop",
                        "cpu.max": "10000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();

        // Synthesized cgroup with bandwidth.
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/test_bw_stop");
        let bw = scenario.cgroups[0]
            .bandwidth
            .as_ref()
            .expect("expected synthesized bandwidth");
        assert_eq!(bw.quota_us, 10_000);
        assert_eq!(bw.period_us, 100_000);

        // All four task instances assigned.
        assert_eq!(scenario.tasks.len(), 4);
        for t in &scenario.tasks {
            assert_eq!(t.cgroup_name.as_deref(), Some("/test_bw_stop"));
        }
    }

    // -- rt-app++ extensions: thread groups, creds, parent, kthread --

    /// `thread_of` names the FIRST instance of the target, which is the
    /// thread-group leader, and every instance of the declaring task joins it.
    #[test]
    fn thread_of_points_every_instance_at_the_leaders_first_instance() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "frontendd":    { "run": 1000, "loop": 1 },
                "iothreadpool": { "instance": 3, "run": 1000, "loop": 1,
                                  "thread_of": "frontendd" }
            }
        }"#;
        let s = load_rtapp(json, 4).unwrap();
        let leader = s.tasks.iter().find(|t| t.name == "frontendd").unwrap();
        assert_eq!(leader.thread_group_leader, None, "a process leads itself");
        for i in 0..3 {
            let th = s
                .tasks
                .iter()
                .find(|t| t.name == format!("iothreadpool-{i}"))
                .unwrap();
            assert_eq!(th.thread_group_leader, Some(leader.pid));
        }
    }

    /// Naming your own task means "my instances are one process, led by
    /// instance 0" — the compact spelling of a same-named worker pool. It is
    /// the same rule as the cross-task form, not a special case: the leader is
    /// the first instance of the named task, which here is instance 0 itself.
    #[test]
    fn thread_of_self_makes_the_instances_one_process() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "workerpool": { "instance": 3, "run": 1000, "loop": 1,
                                "thread_of": "workerpool" }
            }
        }"#;
        let s = load_rtapp(json, 4).unwrap();
        let first = s.tasks[0].pid;
        assert_eq!(
            s.tasks[0].thread_group_leader,
            Some(first),
            "instance 0 names itself, which the engine reads as `is its own leader`"
        );
        for t in &s.tasks[1..] {
            assert_eq!(t.thread_group_leader, Some(first));
        }
    }

    #[test]
    fn thread_of_an_unknown_task_is_an_error() {
        let json = r#"{
            "tasks": { "w": { "run": 1000, "loop": 1, "thread_of": "nosuchtask" } }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(matches!(err, RtAppError::UnresolvedTaskRef { .. }), "{err}");
        assert!(err.to_string().contains("nosuchtask"), "{err}");
    }

    /// `irq_gen*` entries consume a pid slot and then become `IrqEvent`s, not
    /// tasks. Resolving through `name_to_pid` therefore succeeds while the pid
    /// names nothing the engine will build — which used to be a panic deep in
    /// `Engine::new` rather than a parse error here.
    #[test]
    fn thread_of_an_irq_gen_task_is_an_error_not_an_engine_panic() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "irq_gen0": { "cpus": "0", "run": 100, "sleep": 900, "loop": -1 },
                "w": { "run": 1000, "loop": 1, "thread_of": "irq_gen0" }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(matches!(err, RtAppError::UnresolvedTaskRef { .. }), "{err}");
    }

    #[test]
    fn nested_thread_groups_are_rejected() {
        let json = r#"{
            "tasks": {
                "a": { "run": 1000, "loop": 1 },
                "b": { "run": 1000, "loop": 1, "thread_of": "a" },
                "c": { "run": 1000, "loop": 1, "thread_of": "b" }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(matches!(err, RtAppError::NestedThreadGroup { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("do not nest"), "{msg}");
        assert!(
            msg.contains("\"a\""),
            "the error must name the real leader: {msg}"
        );
    }

    /// A thread with no `taskgroup` of its own joins its process's cgroup,
    /// which is cgroup v2's domain-mode behaviour and what makes a single
    /// `taskgroup` declaration cover a whole worker pool.
    #[test]
    fn a_thread_inherits_its_process_cgroup() {
        let json = r#"{
            "tasks": {
                "svc": { "run": 1000, "loop": 1, "taskgroup": "/prod/svc" },
                "pool": { "instance": 2, "run": 1000, "loop": 1, "thread_of": "svc" }
            }
        }"#;
        let s = load_rtapp(json, 2).unwrap();
        for t in &s.tasks {
            assert_eq!(t.cgroup_name.as_deref(), Some("/prod/svc"), "{}", t.name);
        }
    }

    /// A thread declaring a DIFFERENT cgroup from its process is cgroup-v2
    /// threaded mode. scxsim does not model it, so it is refused by name
    /// rather than run as if it were the ordinary case.
    #[test]
    fn a_thread_in_a_foreign_cgroup_is_rejected() {
        let json = r#"{
            "tasks": {
                "svc":  { "run": 1000, "loop": 1, "taskgroup": "/prod/svc" },
                "pool": { "run": 1000, "loop": 1, "thread_of": "svc",
                          "taskgroup": "/background/batch" }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(
            matches!(err, RtAppError::ThreadInForeignCgroup { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("THREADED"), "{err}");
    }

    /// Same cgroup spelled out on both is fine — it says nothing new.
    #[test]
    fn a_thread_may_restate_its_process_cgroup() {
        let json = r#"{
            "tasks": {
                "svc":  { "run": 1000, "loop": 1, "taskgroup": "/prod/svc" },
                "pool": { "run": 1000, "loop": 1, "thread_of": "svc",
                          "taskgroup": "/prod/svc" }
            }
        }"#;
        let s = load_rtapp(json, 2).unwrap();
        assert_eq!(s.tasks.len(), 2);
    }

    #[test]
    fn uid_gid_parent_and_kthread_reach_the_task_def() {
        let json = r#"{
            "tasks": {
                "boss":   { "run": 1000, "loop": 1 },
                "worker": { "run": 1000, "loop": 1, "uid": 4711, "gid": 1000,
                            "parent": "boss", "kthread": true }
            }
        }"#;
        let s = load_rtapp(json, 2).unwrap();
        let boss = s.tasks.iter().find(|t| t.name == "boss").unwrap();
        let w = s.tasks.iter().find(|t| t.name == "worker").unwrap();
        assert_eq!(w.uid, Uid(4711));
        assert_eq!(w.gid, Gid(1000));
        assert_eq!(w.parent_pid, Some(boss.pid));
        assert_eq!(w.task_flags, PF_KTHREAD);
        // Defaults, so a spec that says nothing gets the ordinary user task.
        assert_eq!(boss.uid, Uid(0));
        assert_eq!(boss.gid, Gid(0));
        assert_eq!(boss.parent_pid, None);
        assert_eq!(boss.task_flags, 0);
    }

    #[test]
    fn malformed_rtapp_pp_values_are_rejected() {
        for (json, needle) in [
            (r#"{"tasks":{"w":{"run":1,"loop":1,"uid":-1}}}"#, "uid"),
            (r#"{"tasks":{"w":{"run":1,"loop":1,"gid":"nope"}}}"#, "gid"),
            (
                r#"{"tasks":{"w":{"run":1,"loop":1,"kthread":1}}}"#,
                "kthread",
            ),
            (
                r#"{"tasks":{"w":{"run":1,"loop":1,"thread_of":7}}}"#,
                "thread_of",
            ),
        ] {
            let err = load_rtapp(json, 2).unwrap_err();
            assert!(
                matches!(err, RtAppError::InvalidValue(_)) && err.to_string().contains(needle),
                "expected an InvalidValue naming {needle}, got: {err}"
            );
        }
    }

    /// Identity belongs to a task for its whole life. Silently skipping a
    /// task-level key found inside `phases` — which is what the generic
    /// unknown-key path would do — would leave the author believing a
    /// per-phase identity change happened.
    #[test]
    fn rtapp_pp_keys_inside_phases_are_rejected() {
        let json = r#"{
            "tasks": {
                "w": {
                    "loop": 1,
                    "phases": {
                        "a": { "run": 1000, "uid": 4711 },
                        "b": { "sleep": 1000 }
                    }
                }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(
            matches!(err, RtAppError::TaskLevelKeyInPhase("uid")),
            "{err}"
        );
    }

    /// KNOWN GAP: this parser's instance naming is not what rt-app produces,
    /// so the same spec yields different `comm` strings in simulation and on
    /// hardware.
    ///
    /// WHY EXPECTED: rt-app's `thread_data_set_unique_name()` is
    /// `snprintf("%s-%d", name, tdata->ind)` with `tdata->ind` a GLOBAL thread
    /// index (`data->ind = index` in `rt-app_parse_config.c`), applied to
    /// every task including single-instance ones. Measured on `~/bin/rt-app`
    /// at `checkouts/rt-app@9eedd75` with a two-task / three-thread spec, read
    /// from `/proc/<pid>/task/*/comm`:
    ///
    /// ```text
    ///   frontendd-0        (declared "instance": 1 — still suffixed)
    ///   iothreadpool-1     (first instance of the second task — index 1)
    ///   iothreadpool-2
    /// ```
    ///
    /// This parser instead uses the bare name at `instance == 1` and a
    /// per-task index otherwise. Prefix rules — which is what the deployed
    /// corpus overwhelmingly writes — are unaffected. An exact rule, or a
    /// prefix reaching past the base name, diverges.
    ///
    /// It is pinned rather than fixed because changing it touches pid
    /// assignment, `resume`-target resolution and the export path together;
    /// that is its own change with its own risk, and nothing here is blocked
    /// on it.
    ///
    /// WHEN THIS GOES RED: the naming was aligned with rt-app. Invert it —
    /// assert the rt-app spelling — and re-check `name_to_pid`, which maps
    /// both the bare name and the suffixed one.
    ///
    /// DO NOT: delete it, and do not "fix" it by renaming only the
    /// `instance == 1` case; the index is global, so the multi-instance case
    /// is wrong too.
    #[test]
    fn known_gap_instance_naming_diverges_from_stock_rtapp() {
        let json = r#"{
            "tasks": {
                "frontendd":    { "run": 1000, "loop": 1 },
                "iothreadpool": { "instance": 2, "run": 1000, "loop": 1 }
            }
        }"#;
        let s = load_rtapp(json, 2).unwrap();
        let names: Vec<&str> = s.tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["frontendd", "iothreadpool-0", "iothreadpool-1"],
            "rt-app itself would produce [frontendd-0, iothreadpool-1, iothreadpool-2]. \
             KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
             assert the property now holds. Do not delete it, and do not loosen the bound."
        );
    }

    /// An `irq_gen*` entry with `instance > 1` used to desynchronise the two
    /// passes over `tasks`, because the first consumed one pid per instance
    /// and the second consumed exactly one. Every task parsed after it then
    /// got a pid lower than the one `name_to_pid` had published, so a `resume`
    /// naming it lowered to a wake of a pid no task carried — delivered to
    /// nothing, with no error.
    ///
    /// Measured before the fix: `later` was given `Pid(2)` while `waker`
    /// emitted `Wake(Pid(4))`.
    #[test]
    fn irq_gen_instances_do_not_desynchronise_pid_numbering() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "irq_gen0": { "instance": 3, "cpus": "0", "run": 100,
                              "sleep": 900, "loop": -1 },
                "later":    { "run": 1000, "loop": 1 },
                "waker":    { "run": 1000, "loop": 1, "resume": "later" }
            }
        }"#;
        let s = load_rtapp(json, 2).unwrap();
        let later = s.tasks.iter().find(|t| t.name == "later").unwrap().pid;
        let waker = s.tasks.iter().find(|t| t.name == "waker").unwrap();
        assert!(
            waker
                .behavior
                .phases
                .iter()
                .any(|p| matches!(p, Phase::Wake(pid) if *pid == later)),
            "waker must resume the pid `later` actually got ({later:?}), got {:?}",
            waker.behavior.phases
        );
    }

    /// The belt to that brace: a wake naming a pid no task carries is an
    /// error, not a wake delivered to nothing. Constructed directly rather
    /// than through a spec, because the source that produced it is fixed.
    #[test]
    fn a_wake_of_a_nonexistent_pid_is_rejected() {
        let mut tasks = vec![TaskDef {
            name: "waker".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Wake(Pid(99))],
                repeat: RepeatMode::Once,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        }];
        let err = validate_wake_targets(&tasks).unwrap_err();
        assert!(
            matches!(err, RtAppError::UnresolvedWakeTarget { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("delivered to nothing"), "{err}");

        // And the control: pointing it at a real task is fine.
        tasks[0].behavior.phases = vec![Phase::Wake(Pid(1))];
        validate_wake_targets(&tasks).unwrap();
    }
}
