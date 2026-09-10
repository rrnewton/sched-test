//! Can our workload language drive the names scx_layered matches on?
//!
//! scx_layered classifies tasks almost entirely by *name*. A survey of real
//! deployed layer configurations found that three match kinds — `CgroupPrefix`,
//! `PcommPrefix` and `CommPrefix` — account for the large majority of every
//! rule written, in that order. A simulator that cannot control those three
//! strings cannot run a real layered config, however faithfully the BPF itself
//! executes. Task names below are deliberately generic; the configurations
//! they stand in for are not public.
//!
//! `tests/layered.rs` covers `CommPrefix` and the `Cgroup*` kinds driven from
//! `Scenario::builder()`. This file covers the two things it does not:
//!
//! 1. the **rt-app** front end, which is how the workloads we actually want to
//!    replay arrive — and which reaches `p->comm` and `cgrp->kn->name` by a
//!    different route than the builder does;
//! 2. **`PcommPrefix`**, which was exposed through `LayerMatch` with no
//!    behavioural test at all and dereferenced a NULL `p->group_leader`.
//!
//! The chains under test:
//!
//! - rt-app task-object key -> `TaskDef::name` -> `sim_task_set_comm()` ->
//!   `p->comm`, read directly by `MATCH_COMM_PREFIX`;
//! - rt-app `taskgroup` -> `TaskDef::cgroup_name` -> synthesized `CgroupDef`
//!   chain -> `cgrp->kn->name`, walked by `format_cgrp_path()` to build the
//!   string `MATCH_CGROUP_PREFIX` compares against;
//! - `p->group_leader->comm`, read by `MATCH_PCOMM_PREFIX`.

use scx_simulator::*;

#[macro_use]
mod common;

/// Load an rt-app spec and turn BPF errors into test failures.
///
/// `load_rtapp` defaults to `ignore_bpf_errors: true`, which would let
/// layered's own "didn't match any layer" `scx_bpf_error` pass silently — the
/// exact failure these tests exist to catch.
fn strict_rtapp(json: &str, nr_cpus: u32) -> Scenario {
    let mut scenario = load_rtapp(json, nr_cpus).expect("rt-app spec should parse");
    scenario.ignore_bpf_errors = false;
    scenario
}

/// The pid rt-app assigned to the task it named `name`.
fn pid_of(scenario: &Scenario, name: &str) -> Pid {
    scenario
        .tasks
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no task named {name}"))
        .pid
}

/// A short run/sleep cycle; long enough to be classified, short enough to be
/// cheap.
fn blip() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
        repeat: RepeatMode::Count(3),
    }
}

/// An rt-app spec whose task names are the only thing distinguishing tasks.
///
/// Shaped like a real edge config: a service thread prefix, a cache thread
/// prefix, and everything else.
const COMM_SPEC: &str = r#"{
    "global": { "duration": 1 },
    "tasks": {
        "frontendd":   { "run": 4000, "sleep": 4000, "loop": 3 },
        "cachesvc_io": { "run": 4000, "sleep": 4000, "loop": 3 },
        "unrelated":   { "run": 4000, "sleep": 4000, "loop": 3 }
    }
}"#;

/// The same three tasks, distinguished instead by nested `taskgroup` paths.
const CGROUP_SPEC: &str = r#"{
    "global": { "duration": 1 },
    "tasks": {
        "w0": { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/prod/frontend" },
        "w1": { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/prod/cachesvc" },
        "w2": { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/background/batch" }
    }
}"#;

/// rt-app task names reach `p->comm`, so `CommPrefix` rules fire.
///
/// The positive answer for the comm axis: the layer a task lands in is decided
/// by the key it was given in the rt-app `tasks` object and nothing else — all
/// three tasks here are otherwise identical.
#[test]
fn rtapp_task_name_drives_comm_prefix_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("edge", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("frontendd".into())),
        LayerSpec::new("cache", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("cachesvc".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = strict_rtapp(COMM_SPEC, 4);
    let edge = pid_of(&scenario, "frontendd");
    let cache = pid_of(&scenario, "cachesvc_io");
    let rest = pid_of(&scenario, "unrelated");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(edge),
        0,
        "rt-app task \"frontendd\" should reach p->comm and match CommPrefix(\"frontendd\")"
    );
    assert_eq!(
        probes.task_layer(cache),
        1,
        "rt-app task \"cachesvc_io\" should match CommPrefix(\"cachesvc\")"
    );
    assert_eq!(
        probes.task_layer(rest),
        2,
        "rt-app task \"unrelated\" matches neither prefix and must hit the catch-all"
    );
}

/// rt-app `taskgroup` reaches `cgrp->kn->name`, so `CgroupPrefix` rules fire.
///
/// The positive answer for the cgroup axis. All three tasks are named
/// `w0`/`w1`/`w2`, which carries no information, so only the `taskgroup` path
/// can be deciding the layer. The prefixes asserted here are written the way a
/// production config writes them — `prod/frontend`, with no leading slash —
/// because that is what the kernel's `format_cgrp_path()` produces.
#[test]
fn rtapp_taskgroup_drives_cgroup_prefix_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("edge", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("prod/frontend".into())),
        LayerSpec::new("cache", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("prod/cachesvc".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = strict_rtapp(CGROUP_SPEC, 4);
    // format_cgrp_path() walks cgrp->ancestors[], so the intermediate level
    // must have been materialized for the leaf path to render at all.
    let names: Vec<&str> = scenario.cgroups.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"/prod") && names.contains(&"/prod/frontend"),
        "rt-app should synthesize the implicit ancestor cgroup, got {names:?}"
    );
    let edge = pid_of(&scenario, "w0");
    let cache = pid_of(&scenario, "w1");
    let rest = pid_of(&scenario, "w2");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(edge),
        0,
        "taskgroup /prod/frontend should match CgroupPrefix(\"prod/frontend\")"
    );
    assert_eq!(
        probes.task_layer(cache),
        1,
        "taskgroup /prod/cachesvc should match CgroupPrefix(\"prod/cachesvc\")"
    );
    assert_eq!(
        probes.task_layer(rest),
        2,
        "taskgroup /background/batch matches neither prefix and must hit the catch-all"
    );
}

/// A nested rt-app `taskgroup` renders as the path the *kernel* would render.
///
/// The test above would pass on several wrong renderings, because a prefix
/// rule only pins a prefix. This one pins the exact string by offering the
/// scheduler a menu: the correct rendering plus the two ways it has actually
/// been wrong, each as its own layer, so the layer id reports which one won.
///
/// It regresses a real bug. `CgroupDef::name` is both the key tasks reference
/// a cgroup by and the value published into `cgrp->kn->name`. The rt-app
/// bridge must use full paths as keys (a bare `frontend` is not unique across
/// the tree), so before the fix the parent's whole path was re-embedded in the
/// child's directory entry and `/prod/frontend` rendered as
/// `/prod//prod/frontend/` — no production rule would have matched it, and
/// nothing would have said so.
#[test]
fn rtapp_taskgroup_renders_the_kernel_cgroup_path() {
    let _lock = common::setup_test();
    const CANDIDATES: [&str; 3] = [
        "prod/frontend/",        // what the kernel renders
        "/prod//prod/frontend/", // full path in every kn->name
        "/prod/frontend/",       // leading slash left on the leaf entry
    ];
    let sched = DynamicScheduler::layered(4);
    let mut specs: Vec<LayerSpec> = CANDIDATES
        .iter()
        .enumerate()
        .map(|(i, candidate)| {
            LayerSpec::new(format!("cand{i}"), LayerKind::Open)
                .with_match(LayerMatch::CgroupPrefix((*candidate).into()))
        })
        .collect();
    specs.push(LayerSpec::catch_all("no_candidate_matched"));
    sched.layered_layers(&specs);
    let probes = LayeredProbes::new(&sched);

    let scenario = strict_rtapp(CGROUP_SPEC, 4);
    let edge = pid_of(&scenario, "w0");
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let got = probes.task_layer(edge);
    assert_eq!(
        got,
        0,
        "rt-app taskgroup /prod/frontend must render as {:?}; it rendered as {}",
        CANDIDATES[0],
        CANDIDATES
            .get(got as usize)
            .copied()
            .unwrap_or("something none of the candidates describe")
    );
}

/// Both axes at once, ANDed — the shape a real layered config actually uses.
///
/// Deployed configs rarely match on one attribute; the common form is "this
/// cgroup AND this thread-name prefix". The two off-diagonal tasks are what
/// make this more than a repeat of the two tests above: each satisfies exactly
/// one half of the AND, so each proves the *other* half is live.
#[test]
fn rtapp_comm_and_cgroup_compose_in_one_and_group() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("edge_hot", LayerKind::Open).with_or(vec![
            LayerMatch::CgroupPrefix("prod/frontend".into()),
            LayerMatch::CommPrefix("frontendd".into()),
        ]),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "frontendd":   { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/prod/frontend" },
            "frontendd_elsewhere": { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/background/batch" },
            "helper":      { "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/prod/frontend" }
        }
    }"#;
    let scenario = strict_rtapp(spec, 4);
    let both = pid_of(&scenario, "frontendd");
    let comm_only = pid_of(&scenario, "frontendd_elsewhere");
    let cgroup_only = pid_of(&scenario, "helper");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(both),
        0,
        "task satisfying both ANDed rules must land in edge_hot"
    );
    assert_eq!(
        probes.task_layer(comm_only),
        1,
        "right comm, wrong cgroup: the AND must fail, proving the cgroup rule is live"
    );
    assert_eq!(
        probes.task_layer(cgroup_only),
        1,
        "right cgroup, wrong comm: the AND must fail, proving the comm rule is live"
    );
}

/// `PcommPrefix` reads the group leader's comm without faulting.
///
/// `MATCH_PCOMM_PREFIX` does `__builtin_memcpy(pcomm, p->group_leader->comm,
/// MAX_COMM)`. `sim_task_alloc()` used to leave `group_leader` NULL, so this
/// exact configuration — which `LayerMatch::PcommPrefix` has always allowed
/// callers to build — killed the process with SIGSEGV. It is the most-used
/// match kind in real edge configurations, and it had no behavioural test:
/// the only other mention of `PcommPrefix` in the suite is the enum-ABI guard,
/// which never runs a match.
#[test]
fn pcomm_prefix_matches_the_group_leader_comm() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("edge", LayerKind::Open)
            .with_match(LayerMatch::PcommPrefix("frontendd".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("frontendd", 0, blip())
        .add_task("something_else", 0, blip())
        .duration_ms(100)
        .build();
    let (edge, rest) = (
        pid_of(&scenario, "frontendd"),
        pid_of(&scenario, "something_else"),
    );

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(edge),
        0,
        "a single-threaded task is its own group leader, so its own comm must satisfy PcommPrefix"
    );
    assert_eq!(
        probes.task_layer(rest),
        1,
        "non-matching task must fall through"
    );
}

/// KNOWN GAP: `IsGroupLeader` answers backwards for every task, because
/// `p->tgid` is never set.
///
/// WHY EXPECTED: `MATCH_IS_GROUP_LEADER` is `(p->tgid == p->pid) == want`.
/// `sim_task_set_pid()` leaves `tgid` at 0 while task pids start at 1, so
/// `IsGroupLeader(true)` can never fire and `IsGroupLeader(false)` always
/// does. Every simulated task IS a single-threaded process and therefore IS a
/// group leader, so both answers are exactly inverted — silently, since a
/// wrong-but-plausible layer assignment raises no error. Real AI-training
/// layered configurations carry such a rule.
///
/// The one-line fix (`p->tgid = pid`) is correct and is deliberately NOT
/// applied, because it unmasks sim-zwypg — a task left alone on a layer DSQ
/// while every CPU is idle is never dispatched — which takes
/// `layered.rs::default_config_loads_and_runs` from 3/3 tasks completing to
/// 2/3. Landing red is worse than a documented gap. Tracked as sim-6mheb.
///
/// WHEN THIS GOES RED: `tgid` was wired up. Invert it — assert every task
/// lands in the `leaders` layer (id 0), which is the true answer — and check
/// that sim-zwypg went with it.
///
/// DO NOT: delete it, and do not "fix" it by removing the `IsGroupLeader`
/// rules; the inversion is the finding.
#[test]
fn known_gap_is_group_leader_answers_backwards_for_every_task() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("leaders", LayerKind::Open).with_match(LayerMatch::IsGroupLeader(true)),
        LayerSpec::new("non_leaders", LayerKind::Open).with_match(LayerMatch::IsGroupLeader(false)),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("a", 0, blip())
        .add_task("b", 0, blip())
        .duration_ms(100)
        .build();
    let pids: Vec<Pid> = scenario.tasks.iter().map(|t| t.pid).collect();

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    for pid in pids {
        assert_eq!(
            probes.task_layer(pid),
            1,
            "{pid:?} is a single-threaded process and IS a group leader, but tgid=0 \
             puts it in the non_leaders layer. \
             KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
             assert the property now holds. Do not delete it, and do not loosen the bound."
        );
    }
}

/// KNOWN GAP: `PcommPrefix` cannot distinguish a worker thread from its
/// process, because scxsim has no thread groups — every task is its own
/// leader, so `pcomm` is always just `comm`.
///
/// WHY EXPECTED: `sim_task_alloc()` sets `p->group_leader = p`, which is the
/// correct kernel state for a single-threaded process and is all the simulator
/// can express: `TaskDef` has a `parent_pid` but no thread-group relation, and
/// rt-app has no syntax for one either. Production uses `PcommPrefix` for
/// exactly the case this cannot reach — catching a worker-pool thread by its
/// *process* name rather than its own — and for several real edge
/// configurations it is the only match kind used at all. Tracked as
/// sim-ttaa0.
///
/// WHEN THIS GOES RED: thread groups were modelled. Invert it — assert that
/// the child DOES match `PcommPrefix` on the leader's name — and add the
/// rt-app surface that expresses the grouping.
///
/// DO NOT: delete it (it is the only record that the dominant production
/// match kind is not yet faithfully reproducible), and do not weaken it to
/// "PcommPrefix is unsupported" — it is supported, just not distinguishing.
#[test]
fn known_gap_pcomm_prefix_cannot_distinguish_a_thread_from_its_leader() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("by_leader", LayerKind::Open)
            .with_match(LayerMatch::PcommPrefix("frontendd".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let mut scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("frontendd", 0, blip())
        .add_task("iothreadpool0", 0, blip())
        .duration_ms(100)
        .build();
    let leader = pid_of(&scenario, "frontendd");
    let worker = pid_of(&scenario, "iothreadpool0");
    // State the intent as loudly as the language allows: the worker belongs to
    // the leader's process. `parent_pid` is the closest relation `TaskDef`
    // has, and it is deliberately set here so that wiring `group_leader` from
    // it would flip this test red.
    scenario
        .tasks
        .iter_mut()
        .find(|t| t.pid == worker)
        .expect("worker task")
        .parent_pid = Some(leader);

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(leader),
        0,
        "the leader matches on its own comm, which is not the gap"
    );
    assert_eq!(
        probes.task_layer(worker),
        1,
        "a worker declared as a child of frontendd still does not match \
         PcommPrefix(\"frontendd\"), because there are no thread groups. \
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
         assert the property now holds. Do not delete it, and do not loosen the bound."
    );
}
