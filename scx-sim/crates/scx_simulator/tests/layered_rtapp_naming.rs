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

/// `PcommPrefix` distinguishes a worker thread from its process — the gap
/// sim-ttaa0 recorded, now closed.
///
/// This was `known_gap_pcomm_prefix_cannot_distinguish_a_thread_from_its_leader`,
/// which asserted the worker landed in the catch-all because scxsim had no
/// thread groups and `pcomm` therefore always equalled `comm`. It is inverted
/// here per the known-gap convention: the property it stood in for is now
/// assertable for the first time.
///
/// The discriminator is `iothreadpool`'s own comm sharing no prefix with
/// `frontendd`, so the only way it can reach layer 0 is through
/// `p->group_leader->comm`. The third task is the control: same comm shape,
/// no thread group, must fall through. Sabotage check — make
/// `ScenarioBuilder::add_thread_of` leave `thread_group_leader` at `None`, or
/// make the engine skip `task_set_group_leader`, and the worker assertion goes
/// red while the control stays green.
#[test]
fn pcomm_prefix_distinguishes_a_thread_from_its_leader() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("by_leader", LayerKind::Open)
            .with_match(LayerMatch::PcommPrefix("frontendd".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let mut builder =
        Scenario::builder()
            .cpus(2)
            .detect_bpf_errors()
            .add_task("frontendd", 0, blip());
    let leader = Pid(1);
    builder = builder
        .add_thread_of("iothreadpool0", 0, blip(), leader)
        // Control: identical comm shape, its own process.
        .add_task("iothreadpool9", 0, blip());
    let scenario = builder.duration_ms(100).build();
    assert_eq!(pid_of(&scenario, "frontendd"), leader, "pid assumption");
    let worker = pid_of(&scenario, "iothreadpool0");
    let loner = pid_of(&scenario, "iothreadpool9");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(leader),
        0,
        "the leader matches on its own comm"
    );
    assert_eq!(
        probes.task_layer(worker),
        0,
        "a thread of frontendd must match PcommPrefix(\"frontendd\") on its LEADER's comm; \
         its own comm shares no prefix with it"
    );
    assert_eq!(
        probes.task_layer(loner),
        1,
        "an identically-named task that is its own process must NOT match — this is what \
         proves the match came from the thread group and not from the comm"
    );
}

/// The core of the workload-language extension: `thread_of` in an rt-app spec
/// makes a real layered `pcomm` rule fire on a real parent process name.
///
/// Stock rt-app has no thread-group syntax at all, so this is an rt-app++
/// extension rather than a mapping. Shaped like the production case it exists
/// for: one service process, a pool of worker threads whose own names carry no
/// service identity, and a rule that catches the pool by the process it
/// belongs to. `instance: 3` on the pool is load-bearing — production matches
/// pools, not single threads.
///
/// Note what is NOT asserted: nothing here claims stock rt-app would reproduce
/// it. It would not — measured, every rt-app thread reports the same `Tgid`
/// and a leader comm of `rt-app`. That is why `scenario_to_rtapp_json` refuses
/// to export a scenario carrying a thread group.
#[test]
fn rtapp_thread_of_drives_pcomm_prefix_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("edge", LayerKind::Open)
            .with_match(LayerMatch::PcommPrefix("frontendd".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "frontendd": { "run": 4000, "sleep": 4000, "loop": 3 },
            "iothreadpool": {
                "instance": 3, "run": 4000, "sleep": 4000, "loop": 3,
                "thread_of": "frontendd"
            },
            "batchd":  { "run": 4000, "sleep": 4000, "loop": 3 },
            "iohelper": { "run": 4000, "sleep": 4000, "loop": 3, "thread_of": "batchd" }
        }
    }"#;
    let scenario = strict_rtapp(spec, 4);
    let leader = pid_of(&scenario, "frontendd");
    let pool: Vec<Pid> = (0..3)
        .map(|i| pid_of(&scenario, &format!("iothreadpool-{i}")))
        .collect();
    let other_leader = pid_of(&scenario, "batchd");
    let other_thread = pid_of(&scenario, "iohelper");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(leader), 0, "the process itself matches");
    for (i, pid) in pool.iter().enumerate() {
        assert_eq!(
            probes.task_layer(*pid),
            0,
            "iothreadpool-{i} is a thread of frontendd and must match \
             PcommPrefix(\"frontendd\") on the process name"
        );
    }
    assert_eq!(
        probes.task_layer(other_thread),
        1,
        "a thread of a DIFFERENT process must not match — otherwise `thread_of` would be \
         matching on nothing more than 'is a thread'"
    );
    assert_eq!(probes.task_layer(other_leader), 1, "control process");
}

/// rt-app `uid` / `gid` reach `real_cred`, so the id rules fire.
///
/// Before this, `sim_task_alloc()` left `p->real_cred` NULL and layered's arms
/// are `cred = p->real_cred; if (cred) result = cred->euid.val == ...`. Both
/// kinds therefore returned false for every task — exposed in `LayerMatch`,
/// present in deployed configurations, and silently dead. The `Not` layer is
/// what makes this more than "some layer matched": task `svc` satisfies the
/// uid rule and fails the gid one, so each id is shown to be read separately.
#[test]
fn rtapp_uid_and_gid_drive_the_credential_matches() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("both", LayerKind::Open).with_or(vec![
            LayerMatch::UserIdEquals(4711),
            LayerMatch::GroupIdEquals(1000),
        ]),
        LayerSpec::new("uid_only", LayerKind::Open).with_match(LayerMatch::UserIdEquals(4711)),
        LayerSpec::new("gid_only", LayerKind::Open).with_match(LayerMatch::GroupIdEquals(1000)),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "both":     { "run": 4000, "sleep": 4000, "loop": 3, "uid": 4711, "gid": 1000 },
            "svc":      { "run": 4000, "sleep": 4000, "loop": 3, "uid": 4711 },
            "grouponly":{ "run": 4000, "sleep": 4000, "loop": 3, "gid": 1000 },
            "rootish":  { "run": 4000, "sleep": 4000, "loop": 3 }
        }
    }"#;
    let scenario = strict_rtapp(spec, 4);
    let (both, svc, grouponly, rootish) = (
        pid_of(&scenario, "both"),
        pid_of(&scenario, "svc"),
        pid_of(&scenario, "grouponly"),
        pid_of(&scenario, "rootish"),
    );

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(both), 0, "uid AND gid");
    assert_eq!(
        probes.task_layer(svc),
        1,
        "uid 4711 with the default gid must satisfy only the uid rule"
    );
    assert_eq!(
        probes.task_layer(grouponly),
        2,
        "gid 1000 with the default uid must satisfy only the gid rule"
    );
    assert_eq!(
        probes.task_layer(rootish),
        3,
        "a task that declared neither keeps uid/gid 0 and matches neither rule"
    );
}

/// rt-app `parent` reaches `real_parent`, so `PpidEquals` fires.
///
/// `MATCH_PPID_EQUALS` is `p->real_parent->pid == match->ppid`. The relation
/// was already reachable from `TaskDef::parent_pid`; what was missing was any
/// way to say it in a workload spec.
#[test]
fn rtapp_parent_drives_ppid_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "supervisor": { "run": 4000, "sleep": 4000, "loop": 3 },
            "child":      { "run": 4000, "sleep": 4000, "loop": 3, "parent": "supervisor" },
            "orphan":     { "run": 4000, "sleep": 4000, "loop": 3 }
        }
    }"#;
    let scenario = strict_rtapp(spec, 2);
    let supervisor = pid_of(&scenario, "supervisor");
    let child = pid_of(&scenario, "child");
    let orphan = pid_of(&scenario, "orphan");

    sched.layered_layers(&[
        LayerSpec::new("supervised", LayerKind::Open)
            .with_match(LayerMatch::PpidEquals(supervisor.0 as u32)),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(child),
        0,
        "rt-app `parent` must reach p->real_parent->pid"
    );
    assert_eq!(
        probes.task_layer(orphan),
        1,
        "a task with no `parent` is its own real_parent (sim_task_alloc's self-reference), \
         so its ppid is its own pid and it must not match the supervisor's"
    );
    // The supervisor's own membership is a separate matter and is asserted by
    // known_gap_ppid_equals_also_matches_the_named_parent_itself below.
}

/// KNOWN GAP: `PpidEquals(X)` also matches task X itself, because a task with
/// no declared parent is its own `real_parent`.
///
/// WHY EXPECTED: `sim_task_alloc()` sets `p->real_parent = p` — "self
/// referencing; simulates init as parent". For pid 1 that is exactly what the
/// kernel does; for every other task it is a simplification. layered's arm is
/// `p->real_parent->pid == match->ppid`, so a parentless task satisfies a rule
/// naming its own pid. A config saying "everything under the supervisor" then
/// silently picks up the supervisor too.
///
/// The simplification is load-bearing elsewhere and is not casually removable:
/// `engine.rs` defers `task_pid_to_raw` registration specifically so that a
/// self-referencing `real_parent` makes `bpf_task_from_pid()` return NULL
/// during `init_task`, which is what drives LAVD down its correct
/// initialisation path. Closing this means giving parentless tasks a real
/// synthetic init parent, which is engine work with its own blast radius.
///
/// WHEN THIS GOES RED: parentless tasks got a real parent. Invert it — assert
/// the supervisor lands in the catch-all — and check `init_task`'s
/// `bpf_task_from_pid()` path still behaves.
///
/// DO NOT: delete it, and do not work around it in the rt-app front end by
/// synthesising a `parent` for every task; that would hide the artifact rather
/// than fix it, and it would make every task's ppid a number no spec named.
#[test]
fn known_gap_ppid_equals_also_matches_the_named_parent_itself() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "supervisor": { "run": 4000, "sleep": 4000, "loop": 3 },
            "child":      { "run": 4000, "sleep": 4000, "loop": 3, "parent": "supervisor" }
        }
    }"#;
    let scenario = strict_rtapp(spec, 2);
    let supervisor = pid_of(&scenario, "supervisor");

    sched.layered_layers(&[
        LayerSpec::new("supervised", LayerKind::Open)
            .with_match(LayerMatch::PpidEquals(supervisor.0 as u32)),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(supervisor),
        0,
        "the supervisor matches a rule naming ITS OWN pid as the parent, because \
         sim_task_alloc() leaves real_parent self-referencing. \
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
         assert the property now holds. Do not delete it, and do not loosen the bound."
    );
}

/// rt-app `kthread` reaches `PF_KTHREAD`, so `IsKthread` fires.
///
/// The bool this rule carries is asserted here in only one direction on
/// purpose. Upstream's arm is `return p->flags & PF_KTHREAD;` — it never reads
/// `match->is_kthread`, so `IsKthread(false)` behaves identically to
/// `IsKthread(true)`. Pinning both directions would pin an upstream bug as if
/// it were our contract; see `known_gap_is_kthread_ignores_its_own_argument`.
#[test]
fn rtapp_kthread_drives_is_kthread_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("kthreads", LayerKind::Open).with_match(LayerMatch::IsKthread(true)),
        LayerSpec::catch_all("user"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "kworker": { "run": 4000, "sleep": 4000, "loop": 3, "kthread": true },
            "plainthr": { "run": 4000, "sleep": 4000, "loop": 3, "kthread": false },
            "usertask": { "run": 4000, "sleep": 4000, "loop": 3 }
        }
    }"#;
    let scenario = strict_rtapp(spec, 2);
    let kworker = pid_of(&scenario, "kworker");
    let plain = pid_of(&scenario, "plainthr");
    let user = pid_of(&scenario, "usertask");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(kworker), 0, "PF_KTHREAD must be set");
    assert_eq!(probes.task_layer(plain), 1, "\"kthread\": false is a no-op");
    assert_eq!(probes.task_layer(user), 1, "an absent key is a user task");
}

/// KNOWN GAP (upstream, not ours): `MATCH_IS_KTHREAD` ignores the boolean the
/// rule carries, so `IsKthread(false)` matches kernel threads too.
///
/// WHY EXPECTED: layered's arm is `return p->flags & PF_KTHREAD;`
/// (`main.bpf.c`), with no reference to `match->is_kthread` — unlike
/// `MATCH_IS_GROUP_LEADER` right above it, which does compare against its
/// argument. Our `LayerMatch::IsKthread(bool)` mirrors the upstream schema and
/// therefore advertises a knob the BPF does not read. Negation still works via
/// `LayerMatch::Not`, which sets the separate `exclude` flag.
///
/// WHEN THIS GOES RED: upstream started honouring the argument. Invert it —
/// assert `IsKthread(false)` matches only non-kthreads — and drop the caveat
/// from `rtapp_kthread_drives_is_kthread_match`.
///
/// DO NOT: delete it, and do not "fix" it on our side by rewriting
/// `to_ffi()` to emit `Not` for `IsKthread(false)`. That would make our API
/// mean something the scheduler does not, which is the failure this whole
/// file exists to catch.
#[test]
fn known_gap_is_kthread_ignores_its_own_argument() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("not_kthreads", LayerKind::Open).with_match(LayerMatch::IsKthread(false)),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "kworker":  { "run": 4000, "sleep": 4000, "loop": 3, "kthread": true },
            "usertask": { "run": 4000, "sleep": 4000, "loop": 3 }
        }
    }"#;
    let scenario = strict_rtapp(spec, 2);
    let kworker = pid_of(&scenario, "kworker");
    let user = pid_of(&scenario, "usertask");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(kworker),
        0,
        "a kernel thread lands in the IsKthread(FALSE) layer, because the arm returns the \
         flag rather than comparing it against the argument. \
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
         assert the property now holds. Do not delete it, and do not loosen the bound."
    );
    assert_eq!(
        probes.task_layer(user),
        1,
        "a user task correctly does not match, which is why the bug is easy to miss: the \
         rule is only wrong on the tasks it was written to exclude. \
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this assertion to \
         assert the property now holds. Do not delete it, and do not loosen the bound."
    );
}

/// A thread inherits its process's cgroup, so a cgroup rule catches the whole
/// process — threads included — from one `taskgroup` declaration.
///
/// Under cgroup v2's default domain mode every thread of a process is in the
/// process's cgroup. Without this, a `thread_of` worker would sit in the root
/// cgroup while its process sat in `/prod/frontend`, and the single most
/// common match kind in the deployed corpus would miss the pool.
#[test]
fn rtapp_thread_inherits_the_process_cgroup() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("edge", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("prod/frontend".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let spec = r#"{
        "global": { "duration": 1 },
        "tasks": {
            "frontendd": {
                "run": 4000, "sleep": 4000, "loop": 3, "taskgroup": "/prod/frontend"
            },
            "iothreadpool": {
                "instance": 2, "run": 4000, "sleep": 4000, "loop": 3,
                "thread_of": "frontendd"
            },
            "elsewhere": { "run": 4000, "sleep": 4000, "loop": 3 }
        }
    }"#;
    let scenario = strict_rtapp(spec, 4);
    for name in ["iothreadpool-0", "iothreadpool-1"] {
        let def = scenario.tasks.iter().find(|t| t.name == name).unwrap();
        assert_eq!(
            def.cgroup_name.as_deref(),
            Some("/prod/frontend"),
            "{name} declared no taskgroup and must inherit its process's"
        );
    }
    let pool: Vec<Pid> = ["iothreadpool-0", "iothreadpool-1"]
        .iter()
        .map(|n| pid_of(&scenario, n))
        .collect();
    let elsewhere = pid_of(&scenario, "elsewhere");

    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    for pid in pool {
        assert_eq!(
            probes.task_layer(pid),
            0,
            "a thread of a process in /prod/frontend is in that cgroup too"
        );
    }
    assert_eq!(probes.task_layer(elsewhere), 1, "control");
}

/// `add_thread_of` gives a leader a *fresh* address space, not one derived
/// from its pid.
///
/// Deriving it from the pid was the first spelling of this and it collides:
/// the leader below is `Pid(1)`, so `MmId(leader.pid)` is `MmId(1)` — which
/// `unrelated` already holds. The two would then silently share an address
/// space and look wake-affine to each other.
///
/// The ordering here is load-bearing and is the reason this test exists in the
/// shape it does: an earlier version added `unrelated` first, which made the
/// leader `Pid(2)` and the collision impossible to reach. It passed under
/// sabotage — the fix reverted — and therefore proved nothing.
#[test]
fn add_thread_of_does_not_collide_with_an_explicit_mm_id() {
    let scenario = Scenario::builder()
        .cpus(2)
        .add_task("leader", 0, blip())
        .add_task_with_mm("unrelated", 0, blip(), MmId(1))
        .add_thread_of("worker", 0, blip(), Pid(1))
        .duration_ms(10)
        .build();
    assert_eq!(
        pid_of(&scenario, "leader"),
        Pid(1),
        "the collision this guards against needs the leader at Pid(1)"
    );

    let mm = |name: &str| {
        scenario
            .tasks
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .mm_id
            .expect("every task in this scenario has an address space")
    };
    assert_eq!(
        mm("leader"),
        mm("worker"),
        "a thread and its process share an address space"
    );
    assert_ne!(
        mm("unrelated"),
        mm("leader"),
        "the thread group must not land on the MmId a caller already chose"
    );
}
