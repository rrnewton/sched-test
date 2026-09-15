//! Loading a real `scx_layered` JSON layer config.
//!
//! Two things are under test and they are not the same thing:
//!
//! 1. **The config parses into the layers it describes.** Cheap, and not
//!    enough on its own — `tests/layered.rs` already proves a hand-written
//!    Rust `LayerSpec` list reaches the scheduler.
//! 2. **The rules a config file asked for are the rules the scheduler
//!    evaluates.** That is the claim this file exists for, and it is checked
//!    against the scheduler's own `match_one()` through the match probes, not
//!    against a re-derivation. `config_rules_fire_against_comm_and_cgroup`
//!    is the one that matters: it shows WHICH term of WHICH OR group put each
//!    task where, including a term that held while its AND-sibling did not.
//!
//! Everything scxsim cannot honour is refused BY NAME, so the refusal tests
//! assert on the name and the disposition, not merely that an error occurred.

use scx_simulator::*;

#[macro_use]
mod common;

/// Options sized for the topology these tests build.
fn opts(nr_cpus: u32) -> LayerConfigOptions {
    LayerConfigOptions::new(nr_cpus)
}

/// A task that runs once for `run_ns` and exits.
fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

/// The names a load refused, so assertions read as a set of field names.
fn refused_names(err: &LayerConfigError) -> Vec<&'static str> {
    match err {
        LayerConfigError::Unsupported(rs) => rs.iter().map(|r| r.item.json_name()).collect(),
        other => panic!("expected an Unsupported refusal, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// The claim that matters: rules from a FILE, evaluated against real strings
// ---------------------------------------------------------------------------

/// A layer config loaded from a file drives the scheduler's own matcher
/// against task `comm` and cgroup path, and the probes name the winning term.
///
/// The load-bearing task is `bg_other`: it shares `background/` with
/// `bg_worker`, so the AND group's cgroup term HOLDS for it and the comm term
/// does not. A run where nothing was compared cannot produce that asymmetry —
/// which is exactly the state `scxsim run -s layered` was in before a config
/// could be loaded at all, with one catch-all layer and no rule ever
/// evaluated against any string.
#[test]
fn config_rules_fire_against_comm_and_cgroup() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "iface",
        "matches": [[{ "CgroupPrefix": "interactive/" }]],
        "kind": { "Grouped": { "util_range": [0.1, 0.8], "preempt": true } } },
      { "name": "bg_worker_only",
        "matches": [[{ "CgroupPrefix": "background/" },
                     { "CommPrefix": "bg_worker" }]],
        "kind": { "Confined": { "util_range": [0.2, 0.9], "slice_us": 4000 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(4)).expect("config should load");
    assert_eq!(loaded.specs.len(), 3);

    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&loaded.specs);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let all = [CpuId(0), CpuId(1), CpuId(2), CpuId(3)];
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .cgroup("interactive", &all)
        .cgroup("background", &all)
        .add_task_in_cgroup("iface_a", 0, run_once(10_000_000), "interactive")
        .add_task_in_cgroup("bg_worker", 0, run_once(10_000_000), "background")
        .add_task_in_cgroup("bg_other", 0, run_once(10_000_000), "background")
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let layer_of = |pid: u32| monitor.probes().task_layer(Pid(pid as i32));
    assert_eq!(layer_of(1), 0, "iface_a should match the cgroup-only layer");
    assert_eq!(layer_of(2), 1, "bg_worker should match the AND layer");
    assert_eq!(
        layer_of(3),
        2,
        "bg_other should fall through to the catch-all"
    );

    // WHICH rule, not just which layer. bg_other's AND group must have gotten
    // past the cgroup term and failed on the comm term: term index 1.
    let trace = monitor
        .first_match_trace(Pid(3))
        .expect("bg_other should have a match trace");
    assert_eq!(
        trace.cgrp_path.as_deref(),
        Some("background/"),
        "the scheduler's own format_cgrp_path() output is what the rule sees"
    );
    assert_eq!(trace.comm.as_deref(), Some("bg_other"));
    assert_eq!(
        trace.layers[1].groups[0],
        OrGroupVerdict::FailedAt(1),
        "the cgroup term HELD for bg_other and the comm term did not; \
         failing at term 0 instead would mean no cgroup string was compared"
    );
    assert!(
        trace.layers[2].matched(),
        "the catch-all must accept whatever falls through"
    );

    // And the winner for bg_worker is the AND group, both terms held.
    let bg = monitor.first_match_trace(Pid(2)).expect("trace");
    assert_eq!(bg.layers[1].groups[0], OrGroupVerdict::Matches);
    assert_eq!(
        monitor.probes().describe_term(1, 0, 1),
        "CommPrefix(\"bg_worker\")",
        "the term the scheduler holds is the one the config named"
    );
}

/// Upstream spells negation as a separate `*Exclude` match variant; scxsim
/// spells it `Not`. Both lower to the same BPF kind with `exclude` set, so a
/// config using the upstream spelling must produce a rule that fires on
/// everything BUT the prefix.
#[test]
fn comm_prefix_exclude_from_a_config_inverts_the_rule() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "not_bench",
        "matches": [[{ "CommPrefixExclude": "bench" }]],
        "kind": { "Open": {} } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("config should load");
    assert_eq!(
        loaded.specs[0].matches[0][0],
        LayerMatch::Not(Box::new(LayerMatch::CommPrefix("bench".into())))
    );

    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&loaded.specs);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("bench_a", 0, run_once(10_000_000))
        .add_task("other_b", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    assert_eq!(
        sim.run_monitored(scenario, &mut NoMonitor)
            .trace
            .exit_kind(),
        &ExitKind::Normal
    );
    assert_eq!(probes.task_layer(Pid(1)), 1, "bench_a is excluded");
    assert_eq!(probes.task_layer(Pid(2)), 0, "other_b is not");
    assert_eq!(probes.match_exclude(0, 0, 0), Some(true));
}

/// A monitor that records nothing, for runs that only need the outcome.
struct NoMonitor;
impl Monitor for NoMonitor {
    fn sample(&mut self, _ctx: &ProbeContext) {}
}

// ---------------------------------------------------------------------------
// Per-layer policy fields reach `struct layer`
// ---------------------------------------------------------------------------

/// Every scalar policy field a config can set is readable back out of the
/// scheduler's `struct layer` with the value the config asked for.
///
/// Publication is not otherwise observable: `layered_set_layer_field()`
/// returning 0 is a claim about the setter, not about scheduler state. The
/// values below are all distinguishable from the wrapper's defaults, so a
/// setter that silently did nothing fails here.
#[test]
fn layer_field_publication_reaches_struct_layer() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "policy",
        "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Grouped": {
            "util_range": [0.1, 0.9],
            "slice_us": 10000,
            "fifo": true,
            "yield_ignore": 0.25,
            "disallow_open_after_us": 7,
            "disallow_preempt_after_us": 9,
            "xllc_mig_min_us": 11,
            "skip_remote_node": true,
            "prev_over_idle_core": true,
            "idle_confined": true,
            "placement": "Sticky",
            "member_expire_ms": 1234
        } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("config should load");
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&loaded.specs);
    let probes = LayeredProbes::new(&sched);

    let expect = |field: LayerField, want: u64| {
        assert_eq!(
            probes.layer_field(0, field),
            Some(want),
            "{field:?} did not reach struct layer"
        );
    };
    expect(LayerField::Fifo, 1);
    // yield_ignore 0.25 of a 10ms slice leaves 75% of it as the yield step.
    expect(LayerField::YieldStepNs, 7_500_000);
    expect(LayerField::DisallowOpenAfterNs, 7_000);
    expect(LayerField::DisallowPreemptAfterNs, 9_000);
    expect(LayerField::XllcMigMinNs, 11_000);
    expect(LayerField::SkipRemoteNode, 1);
    expect(LayerField::PrevOverIdleCore, 1);
    expect(LayerField::IdleConfined, 1);
    expect(LayerField::TaskPlace, LayerPlacement::Sticky as u64);
    expect(LayerField::MemberExpireMs, 1234);
    expect(LayerField::Perf, 0);

    // The catch-all left everything alone, so it must read back as the
    // defaults — otherwise the publication above is leaking across layers.
    assert_eq!(probes.layer_field(1, LayerField::Fifo), Some(0));
    assert_eq!(probes.layer_field(1, LayerField::SkipRemoteNode), Some(0));
    assert_eq!(probes.layer_field(1, LayerField::MemberExpireMs), Some(0));
    assert_eq!(probes.layer_field(2, LayerField::Fifo), None, "no layer 2");
}

/// A preempting layer has both `disallow_*_after_us` forced to "never",
/// whatever the config said — scx_layered's config-load loop overrides them
/// with a warning rather than erroring, and so must we.
///
/// The other half of this test is the one that was WRONG on first writing and
/// is the reason it is spelled out: `DFL_DISALLOW_OPEN_AFTER_US` and
/// `DFL_DISALLOW_PREEMPT_AFTER_US` are `2 *` and `4 * SCX_SLICE_DFL / 1000` —
/// a FIXED pair, applied to every non-preempting layer that leaves them
/// unset, **regardless of that layer's own `slice_us`**. Deriving them from
/// the layer slice gives 2 ms / 4 ms for the 1 ms layer below where production
/// gives 40 ms / 80 ms, and that is invisible without asserting the number.
#[test]
fn preempt_forces_the_disallow_cutoffs_to_never() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "pre",
        "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Open": { "preempt": true,
                            "disallow_open_after_us": 5,
                            "disallow_preempt_after_us": 6 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": { "slice_us": 1000 } } }
    ]"#;
    let o = opts(2);
    let loaded = parse_layer_config(json, &o).expect("config should load");
    assert_eq!(loaded.specs[0].disallow_open_after_ns, DISALLOW_AFTER_NEVER);
    assert_eq!(
        loaded.specs[0].disallow_preempt_after_ns,
        DISALLOW_AFTER_NEVER
    );
    assert_eq!(loaded.specs[1].slice_ns, 1_000_000, "the layer's own slice");
    assert_eq!(
        loaded.specs[1].disallow_open_after_ns,
        2 * o.scx_slice_dfl_ns,
        "must be 2x SCX_SLICE_DFL, NOT 2x this layer's 1ms slice"
    );
    assert_eq!(
        loaded.specs[1].disallow_preempt_after_ns,
        4 * o.scx_slice_dfl_ns,
        "must be 4x SCX_SLICE_DFL, NOT 4x this layer's 1ms slice"
    );
}

/// The `disallow_*` defaults do not move with the layer's slice.
///
/// A discriminator for exactly the defect above: three layers with slices an
/// order of magnitude apart must all publish the same pair.
#[test]
fn the_disallow_defaults_are_the_same_whatever_the_layer_slice_is() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "a", "matches": [[{ "CommPrefix": "a" }]],
        "kind": { "Open": { "slice_us": 1000 } } },
      { "name": "b", "matches": [[{ "CommPrefix": "b" }]],
        "kind": { "Open": { "slice_us": 100000 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let o = opts(2);
    let loaded = parse_layer_config(json, &o).expect("config should load");
    let want = (2 * o.scx_slice_dfl_ns, 4 * o.scx_slice_dfl_ns);
    for spec in &loaded.specs {
        assert_eq!(
            (spec.disallow_open_after_ns, spec.disallow_preempt_after_ns),
            want,
            "layer {:?} (slice {}ns) must take the fixed defaults",
            spec.name,
            spec.slice_ns
        );
    }
}

/// `cpus_range_frac` resolves against the machine size exactly as
/// `main.rs::resolve_cpus_pct_range()` does, including its floor of one CPU.
#[test]
fn cpus_range_frac_resolves_against_the_cpu_count() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "quarter",
        "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Grouped": { "util_range": [0.1, 0.9],
                               "cpus_range_frac": [0.25, 0.5] } } },
      { "name": "tiny",
        "matches": [[{ "CommPrefix": "y" }]],
        "kind": { "Grouped": { "util_range": [0.1, 0.9],
                               "cpus_range_frac": [0.0, 0.01] } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(16)).expect("config should load");
    assert_eq!(loaded.specs[0].cpus_range, Some((4, 8)));
    assert_eq!(
        loaded.specs[1].cpus_range,
        Some((1, 1)),
        "both ends floor at one CPU, so a layer can never be unservable"
    );
}

// ---------------------------------------------------------------------------
// Refusals, by name
// ---------------------------------------------------------------------------

/// Every match kind scxsim has no substrate for is refused by its upstream
/// name, and none of them is waivable.
#[test]
fn unsupported_match_kinds_are_refused_by_name() {
    let _lock = common::setup_test();
    for (variant, name) in [
        (r#"{ "CgroupRegex": "a.*b" }"#, "CgroupRegex"),
        (r#"{ "NSPIDEquals": [1, 2] }"#, "NSPIDEquals"),
        (r#"{ "NSEquals": 3 }"#, "NSEquals"),
        (r#"{ "CmdJoin": "j" }"#, "CmdJoin"),
        (r#"{ "UsedGpuTid": true }"#, "UsedGpuTid"),
        (r#"{ "UsedGpuPid": true }"#, "UsedGpuPid"),
        (r#"{ "HintEquals": 7 }"#, "HintEquals"),
        (r#"{ "SystemCpuUtilBelow": 0.5 }"#, "SystemCpuUtilBelow"),
        (r#"{ "DsqInsertBelow": 0.5 }"#, "DsqInsertBelow"),
    ] {
        let json = format!(
            r#"[ {{ "name": "l", "matches": [[{variant}]], "kind": {{ "Open": {{}} }} }},
                 {{ "name": "rest", "matches": [[]], "kind": {{ "Open": {{}} }} }} ]"#
        );
        let err = parse_layer_config(&json, &opts(2)).expect_err("{name} must be refused");
        assert_eq!(refused_names(&err), vec![name]);
        assert_eq!(
            Unsupported::from_json_name(name).unwrap().disposition(),
            Disposition::Fatal,
            "{name} must not be waivable: dropping a term widens the AND group"
        );
    }
}

/// A match kind cannot be waived even when named, because dropping a term
/// makes the group match strictly more tasks rather than losing fidelity.
#[test]
fn match_kinds_cannot_be_waived() {
    let mut o = opts(2);
    let err = o.waive(&["UsedGpuPid"]).expect_err("must not be waivable");
    assert!(err[0].contains("not waivable"), "{err:?}");
    assert!(o.waived.is_empty());
}

/// Layer fields scxsim cannot apply are refused by name, and a waiver lets
/// the config load with the loss reported rather than hidden.
#[test]
fn unsupported_fields_are_refused_and_then_waivable() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "hungry",
        "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Grouped": { "util_range": [0.1, 0.9],
                               "membw_gb": 12.5,
                               "idle_resume_us": 40,
                               "util_peak_half_life_ms": 100 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let err = parse_layer_config(json, &opts(2)).expect_err("must be refused");
    let mut got = refused_names(&err);
    got.sort_unstable();
    assert_eq!(
        got,
        vec!["idle_resume_us", "membw_gb", "util_peak_half_life_ms"],
        "all of them at once — reporting the first would turn a three-field \
         config into three edit-and-retry cycles"
    );

    let mut o = opts(2);
    o.waive(&got).expect("all three are waivable");
    let loaded = parse_layer_config(json, &o).expect("waived config should load");
    let waived: Vec<&str> = loaded.waived.iter().map(|r| r.item.json_name()).collect();
    assert_eq!(waived.len(), 3, "every waived field is reported back");
    assert!(
        loaded.caveats().count() >= 3,
        "and each one appears in the run banner"
    );
}

/// `perf` is HONOURED, not refused, and reaches `struct layer`.
///
/// It was refused on first writing, on the stated grounds that
/// `scx_bpf_cpuperf_set` is a weak-undefined symbol in `libscx_layered.so` and
/// a non-zero `perf` would jump to NULL. That is false: the scxsim host
/// binary exports a working definition (`kfuncs.rs`), and the dynamic linker
/// binds the two — `nm -D target/release/scxsim | grep cpuperf` shows `T`.
/// Publishing it runs the scheduler's own `layer->perf > 0` branch and its
/// kfunc call, which is more of the real logic executing.
///
/// What scxsim does NOT have is a DVFS model, so the level is recorded on the
/// CPU and does not change how fast simulated work completes. That is a
/// caveat the CLI prints, not a reason to skip a real code path.
#[test]
fn perf_is_published_into_struct_layer() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "fast", "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Open": { "perf": 1024 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("perf must not be refused");
    assert_eq!(loaded.specs[0].perf, 1024);

    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&loaded.specs);
    let probes = LayeredProbes::new(&sched);
    assert_eq!(probes.layer_field(0, LayerField::Perf), Some(1024));
    assert_eq!(probes.layer_field(1, LayerField::Perf), Some(0));

    // And the run does not crash on the kfunc the refusal claimed was NULL.
    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("x_a", 0, run_once(10_000_000))
        .add_task("other", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    assert_eq!(
        sim.run_monitored(scenario, &mut NoMonitor)
            .trace
            .exit_kind(),
        &ExitKind::Normal
    );
}

/// Config shapes `main.rs::resolve_cpus_pct_range()` bails on must not load.
/// Accepting them would silently run a configuration the real scheduler
/// refuses to start on.
#[test]
fn cpus_range_forms_upstream_rejects_are_rejected() {
    let _lock = common::setup_test();
    for (json, want) in [
        (
            r#"[ { "name": "a", "matches": [[{ "CommPrefix": "x" }]],
                   "kind": { "Grouped": { "util_range": [0.1, 0.9],
                                          "cpus_range": [1, 2],
                                          "cpus_range_frac": [0.1, 0.2] } } },
                 { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#,
            "cannot be used with",
        ),
        (
            r#"[ { "name": "a", "matches": [[{ "CommPrefix": "x" }]],
                   "kind": { "Grouped": { "util_range": [0.1, 0.9],
                                          "cpus_range_frac": [1.5, 2.0] } } },
                 { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#,
            "between 0.0 and 1.0",
        ),
    ] {
        match parse_layer_config(json, &opts(8)) {
            Err(LayerConfigError::Invalid(msg)) => assert!(msg.contains(want), "{msg}"),
            other => panic!("expected Invalid containing {want:?}, got {other:?}"),
        }
    }
}

/// A rule the wrapper would silently truncate or that could only ever answer
/// "no match" is rejected, not published.
///
/// Both are cases upstream's `verify_layer_specs()` / `init_layers()` bail on,
/// and both are the "silent never-matching or silently-widened rule" the
/// wrapper's own `default:` arm exists to prevent.
#[test]
fn rules_that_could_not_mean_what_they_say_are_rejected() {
    let _lock = common::setup_test();
    let long_comm = "c".repeat(300);
    for (json, want) in [
        (
            format!(
                r#"[ {{ "name": "a", "matches": [[{{ "CommPrefix": "{long_comm}" }}]],
                       "kind": {{ "Open": {{}} }} }},
                     {{ "name": "rest", "matches": [[]], "kind": {{ "Open": {{}} }} }} ]"#
            ),
            "truncate",
        ),
        (
            r#"[ { "name": "a", "matches": [[{ "NumaNode": 99 }]],
                   "kind": { "Open": {} } },
                 { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#
                .to_string(),
            "NumaNode(99)",
        ),
        (
            // \u0000 is legal JSON and produces a real NUL in the needle,
            // which cannot cross the C boundary into struct layer_match.
            r#"[ { "name": "a", "matches": [[{ "CommPrefix": "a\u0000b" }]],
                   "kind": { "Open": {} } },
                 { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#
                .to_string(),
            "NUL byte",
        ),
    ] {
        match parse_layer_config(&json, &opts(2)) {
            Err(LayerConfigError::Invalid(msg)) => assert!(msg.contains(want), "{msg}"),
            other => panic!("expected Invalid containing {want:?}, got {other:?}"),
        }
    }
}

/// The growth algorithms `LayerGrowthAlgo`'s doc says are rejected rather than
/// approximated are in fact rejected on the config path too.
///
/// The rejection used to live only in `LayeredControl`, which
/// `--layer-config` never engages, so a config naming one loaded silently and
/// published a `growth_algo` the BPF's own Big/Little idle selection does not
/// recognise. A documented refusal that does not happen is worse than no
/// refusal, because a reader stops looking.
#[test]
fn unmodellable_growth_algos_are_refused() {
    let _lock = common::setup_test();
    for (algo, want) in [
        ("CpuSetSpread", "growth_algo:CpuSetSpread*"),
        ("CpuSetSpreadReverse", "growth_algo:CpuSetSpread*"),
        ("CpuSetSpreadRandom", "growth_algo:CpuSetSpread*"),
        ("StickyDynamic", "growth_algo:StickyDynamic"),
    ] {
        let json = format!(
            r#"[ {{ "name": "a", "matches": [[{{ "CommPrefix": "x" }}]],
                   "kind": {{ "Open": {{ "growth_algo": "{algo}" }} }} }},
                 {{ "name": "rest", "matches": [[]], "kind": {{ "Open": {{}} }} }} ]"#
        );
        let err = parse_layer_config(&json, &opts(2))
            .err()
            .unwrap_or_else(|| panic!("{algo} must be refused, but the config loaded"));
        assert_eq!(refused_names(&err), vec![want], "for growth_algo {algo}");
        assert_eq!(
            Unsupported::from_json_name(want).unwrap().disposition(),
            Disposition::Fatal,
            "{want} must not be waivable: the CPU order it asks for is not \
             something scxsim can approximate"
        );
    }
}

/// A config round-tripped through scx_layered's OWN serializer must load, and
/// `xnuma_threshold` must reach the spec.
///
/// `xnuma_threshold` / `xnuma_threshold_delta` carry `#[serde(default = ..)]`
/// but no `skip_serializing` upstream, so `scx_layered --example <f>` and
/// `--print-and-exit` always emit them — refusing on presence would have
/// rejected the most canonical way to obtain a config file. They are also
/// MODELLED as of the arbitrary-topology work: `LayerSpec` carries them and
/// `layered_control::refresh_xnuma()` consumes them, so there is nothing to
/// refuse. On the CLI path, which does not run the control loop, they are
/// inert and the run banner says so.
#[test]
fn a_config_carrying_upstreams_own_defaults_loads() {
    let _lock = common::setup_test();
    let dumped = r#"[
      { "name": "a", "comment": null, "template": null,
        "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Open": { "xnuma_threshold": [0.6, 0.7],
                            "xnuma_threshold_delta": [0.2, 0.3] } } },
      { "name": "rest", "comment": null, "template": null,
        "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(dumped, &opts(2)).expect("upstream's own dump must load");
    assert_eq!(loaded.specs[0].xnuma_threshold, DEFAULT_XNUMA_THRESHOLD);

    let asked_for = dumped.replace("[0.6, 0.7]", "[0.1, 0.2]");
    let tuned = parse_layer_config(&asked_for, &opts(2)).expect("a real setting is honoured");
    assert_eq!(tuned.specs[0].xnuma_threshold, (0.1, 0.2));
    assert_eq!(
        tuned.specs[0].xnuma_threshold_delta, DEFAULT_XNUMA_THRESHOLD_DELTA,
        "an unset companion keeps upstream's default"
    );
}

/// A field whose value is inert asks for nothing, so it is not refused.
/// `perf: 0` is the config saying "no cpufreq hint", which scxsim delivers
/// exactly by doing nothing.
#[test]
fn an_inert_value_of_a_refusable_field_is_not_refused() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "l", "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Open": { "perf": 0, "idle_resume_us": 0 } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("inert values should load");
    assert!(loaded.waived.is_empty());
}

/// scx_layered deprecates `allow_node_aligned` and `idle_smt` and ignores
/// them with a warning. Mirroring that is fidelity; refusing would be the
/// divergence, and would stop a real config loading over a flag the real
/// scheduler also does nothing with.
#[test]
fn upstream_deprecated_fields_are_reported_not_refused() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "l", "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Open": { "allow_node_aligned": true, "idle_smt": false } } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("deprecated flags must not refuse");
    let mut names: Vec<&str> = loaded
        .deprecated
        .iter()
        .map(|r| r.item.json_name())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["allow_node_aligned", "idle_smt"]);
    for item in [
        Unsupported::FieldAllowNodeAligned,
        Unsupported::FieldIdleSmt,
    ] {
        assert_eq!(item.disposition(), Disposition::DeprecatedUpstream);
    }
}

/// A key that is in upstream's schema but on the wrong layer kind is dropped
/// by scx_layered's own serde without a word. Report it; do not refuse a file
/// the real scheduler loads. Upstream's `examples/cpuset.json` is exactly
/// this shape — `util_range` under an `Open` layer.
#[test]
fn a_field_written_under_the_wrong_kind_is_reported_not_refused() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "l", "matches": [[{ "CommPrefix": "x" }]],
        "kind": { "Grouped": { "util_range": [0.1, 0.9] } } },
      { "name": "rest", "matches": [[]],
        "kind": { "Open": { "util_range": [0.8, 0.9] } } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("must load, as scx_layered does");
    assert_eq!(loaded.notes.len(), 1, "{:?}", loaded.notes);
    assert!(
        loaded.notes[0].contains("util_range") && loaded.notes[0].contains("Open"),
        "the note must name the field AND the kind that has no such field: {:?}",
        loaded.notes[0]
    );
}

/// A key that is in no layer kind at all is a typo, and nothing would have
/// applied it. That one is refused.
#[test]
fn a_key_in_no_layer_kind_is_refused_as_unknown() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "rest", "matches": [[]],
        "kind": { "Open": { "slcie_us": 1000 } } }
    ]"#;
    match parse_layer_config(json, &opts(2)) {
        Err(LayerConfigError::UnknownKeys(keys)) => {
            assert_eq!(keys.len(), 1);
            assert!(keys[0].contains("slcie_us"), "{keys:?}");
        }
        other => panic!("expected UnknownKeys, got {other:?}"),
    }
}

/// A top-level `LayerSpec` key that upstream does not have is refused by
/// serde itself, because that struct has no flattened field to hide behind.
#[test]
fn an_unknown_top_level_key_is_a_parse_error() {
    let _lock = common::setup_test();
    let json = r#"[ { "name": "rest", "nmae": "typo", "matches": [[]],
                      "kind": { "Open": {} } } ]"#;
    assert!(matches!(
        parse_layer_config(json, &opts(2)),
        Err(LayerConfigError::Json(_))
    ));
}

// ---------------------------------------------------------------------------
// Structural rules, mirroring upstream's verify_layer_specs()
// ---------------------------------------------------------------------------

/// scx_layered treats a task that matches no layer as a fatal error, so the
/// last layer must be the catch-all: `matches` exactly `[[]]`.
#[test]
fn the_terminal_layer_must_be_the_catch_all() {
    let _lock = common::setup_test();
    for json in [
        // No catch-all at all.
        r#"[ { "name": "only", "matches": [[{ "CommPrefix": "x" }]],
               "kind": { "Open": {} } } ]"#,
        // A terminal layer with real terms is not a catch-all either.
        r#"[ { "name": "a", "matches": [[{ "CommPrefix": "x" }]], "kind": { "Open": {} } },
             { "name": "b", "matches": [[{ "CommPrefix": "y" }]], "kind": { "Open": {} } } ]"#,
        // A non-terminal layer with NO OR groups at all — upstream's
        // "NULL matches" bail.
        r#"[ { "name": "null", "matches": [], "kind": { "Open": {} } },
             { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#,
    ] {
        assert!(
            matches!(
                parse_layer_config(json, &opts(2)),
                Err(LayerConfigError::Invalid(_))
            ),
            "should be rejected: {json}"
        );
    }

    let ok = r#"[
      { "name": "a", "matches": [[{ "CommPrefix": "x" }]], "kind": { "Open": {} } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#;
    assert!(parse_layer_config(ok, &opts(2)).is_ok());
}

/// A catch-all group BEFORE the last layer is legal — upstream only rejects a
/// non-terminal layer with no OR groups at all, and `[[]]` has one — but it
/// shadows every layer after it, so it is reported.
///
/// Refusing it would diverge from the real scheduler, which runs such a
/// config. Staying silent would hide a config whose later layers are dead.
#[test]
fn an_early_catch_all_loads_and_is_reported_as_shadowing() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "greedy", "matches": [[]], "kind": { "Open": {} } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } } ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("upstream accepts this");
    assert!(
        loaded
            .notes
            .iter()
            .any(|n| n.contains("greedy") && n.contains("no layer after it")),
        "the shadowing must be reported: {:?}",
        loaded.notes
    );
}

/// An empty config, and one with more layers than the scheduler was built
/// for, are both rejected before anything reaches the FFI.
#[test]
fn layer_count_bounds_are_checked_against_the_scheduler() {
    let _lock = common::setup_test();
    assert!(matches!(
        parse_layer_config("[]", &opts(2)),
        Err(LayerConfigError::Invalid(_))
    ));

    let sched = DynamicScheduler::layered(2);
    let max = LayeredProbes::new(&sched).enum_value(LayeredEnumProbe::MaxLayers) as usize;
    let mut o = opts(2);
    o.max_layers = max;
    let mut layers: Vec<String> = (0..max)
        .map(|i| {
            format!(
                r#"{{ "name": "l{i}", "matches": [[{{ "CommPrefix": "p{i}" }}]],
                      "kind": {{ "Open": {{}} }} }}"#
            )
        })
        .collect();
    layers.push(r#"{ "name": "rest", "matches": [[]], "kind": { "Open": {} } }"#.into());
    let json = format!("[{}]", layers.join(","));
    match parse_layer_config(&json, &o) {
        Err(LayerConfigError::Invalid(msg)) => {
            assert!(msg.contains("MAX_LAYERS"), "{msg}");
        }
        other => panic!("expected an Invalid over MAX_LAYERS, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ABI: the Rust-side selectors match the scheduler's
// ---------------------------------------------------------------------------

/// Each [`LayerField`] selector names the `struct layer` member the Rust side
/// thinks it does.
///
/// WHY THIS IS AN OFFSET CHECK AND NOT A READ-BACK. Writing a value with
/// selector *n* and reading it back with selector *n* cannot detect a
/// reordered `enum layered_layer_field`: both sides move together and the
/// value round-trips into whichever member the C enum currently maps *n* to.
/// An adversarial reviewer proved that by swapping two members of the C enum
/// declaration and getting 73/73 green. Byte offsets do not move with the
/// enum, so comparing them is asymmetric and a reorder fails here.
///
/// The expected offsets are not hardcoded — they come from the same C side —
/// so what this actually pins is that Rust's `LayerField::ALL` ordering, the
/// C enum, and the two switch statements all agree, and that no selector is
/// missing from any of them.
#[test]
fn layer_field_selectors_name_the_same_struct_members() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    let probes = LayeredProbes::new(&sched);

    assert_eq!(
        probes.enum_value(LayeredEnumProbe::LayerFieldCount) as usize,
        LayerField::ALL.len(),
        "the scheduler knows a different number of field selectors than \
         LayerField::ALL lists — one side gained a field and the other did not"
    );

    // Distinct offsets, ordered as the members are declared in struct layer,
    // and each width consistent with the member's type. A reorder of either
    // enum breaks distinctness or the member/width pairing.
    let mut seen: Vec<(u64, u32, &'static str)> = Vec::new();
    for field in LayerField::ALL {
        let (off, width) = probes
            .layer_field_offset(field)
            .unwrap_or_else(|| panic!("{field:?} has no offset — the C switch is missing it"));
        assert!(
            width == 1 || width == 4 || width == 8,
            "{field:?} names a member of implausible width {width}"
        );
        assert!(
            !seen.iter().any(|(o, _, _)| *o == off),
            "{field:?} shares byte offset {off} with {:?} — two selectors name \
             the same struct layer member, so one of them is publishing into \
             the wrong field",
            seen.iter().find(|(o, _, _)| *o == off).map(|(_, _, n)| n)
        );
        seen.push((off, width, field.struct_layer_member()));
    }

    // The bool-typed selectors must land on 1-byte members and the ns-typed
    // ones on 8-byte members. This is what catches a swap between two
    // selectors of DIFFERENT type, the shape the reviewer demonstrated.
    for (field, want_width) in [
        (LayerField::Fifo, 1u32),
        (LayerField::SkipRemoteNode, 1),
        (LayerField::PrevOverIdleCore, 1),
        (LayerField::IdleConfined, 1),
        (LayerField::YieldStepNs, 8),
        (LayerField::DisallowOpenAfterNs, 8),
        (LayerField::DisallowPreemptAfterNs, 8),
        (LayerField::XllcMigMinNs, 8),
        (LayerField::MemberExpireMs, 8),
        (LayerField::Perf, 4),
    ] {
        let (_, width) = probes.layer_field_offset(field).expect("offset");
        assert_eq!(
            width, want_width,
            "{field:?} landed on a {width}-byte struct layer member, expected \
             {want_width} — the selector names the wrong field"
        );
    }
}

/// The new capacity probes report the values `intf.h` is built with, so the
/// loader's bounds come from the scheduler rather than from constants copied
/// into Rust that could go stale across an scx bump.
///
/// `LayerConfigOptions::new()` carries such a copy for the pure-parser tests;
/// this is what tells us when it has drifted.
#[test]
fn capacity_probes_agree_with_the_defaults_baked_into_the_loader() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    let probes = LayeredProbes::new(&sched);
    let dfl = LayerConfigOptions::new(2);
    for (which, got, want, name) in [
        (
            LayeredEnumProbe::MaxLayers,
            dfl.max_layers,
            probes.enum_value(LayeredEnumProbe::MaxLayers),
            "MAX_LAYERS",
        ),
        (
            LayeredEnumProbe::MaxLayerMatchOrs,
            dfl.max_match_ors,
            probes.enum_value(LayeredEnumProbe::MaxLayerMatchOrs),
            "MAX_LAYER_MATCH_ORS",
        ),
        (
            LayeredEnumProbe::NrLayerMatchKinds,
            dfl.max_match_ands,
            probes.enum_value(LayeredEnumProbe::NrLayerMatchKinds),
            "NR_LAYER_MATCH_KINDS",
        ),
        (
            LayeredEnumProbe::MinLayerWeight,
            dfl.weight_range.0 as usize,
            probes.enum_value(LayeredEnumProbe::MinLayerWeight),
            "MIN_LAYER_WEIGHT",
        ),
        (
            LayeredEnumProbe::MaxLayerWeight,
            dfl.weight_range.1 as usize,
            probes.enum_value(LayeredEnumProbe::MaxLayerWeight),
            "MAX_LAYER_WEIGHT",
        ),
        (
            LayeredEnumProbe::DefaultLayerWeight,
            dfl.default_weight as usize,
            probes.enum_value(LayeredEnumProbe::DefaultLayerWeight),
            "DEFAULT_LAYER_WEIGHT",
        ),
        (
            LayeredEnumProbe::MaxComm,
            dfl.max_comm,
            probes.enum_value(LayeredEnumProbe::MaxComm),
            "MAX_COMM",
        ),
        (
            LayeredEnumProbe::MaxPath,
            dfl.max_path,
            probes.enum_value(LayeredEnumProbe::MaxPath),
            "MAX_PATH",
        ),
        (
            LayeredEnumProbe::DefaultSliceNs,
            dfl.default_slice_ns as usize,
            probes.enum_value(LayeredEnumProbe::DefaultSliceNs),
            "the default slice",
        ),
        (
            LayeredEnumProbe::ScxSliceDfl,
            dfl.scx_slice_dfl_ns as usize,
            probes.enum_value(LayeredEnumProbe::ScxSliceDfl),
            "SCX_SLICE_DFL",
        ),
    ] {
        assert_eq!(
            got as i32, want,
            "{name} ({which:?}): LayerConfigOptions::new() is stale against the \
             scheduler. Update the constant; do not change the probe."
        );
    }
    assert_eq!(probes.enum_value(LayeredEnumProbe::MaxLayerName), 128);
}

/// `AvgRuntime` is a real, honoured match kind, not one of the refused ones:
/// `task_ctx.runtime_avg` is maintained by the scheduler's own
/// `layered_stopping()`, so the comparison runs entirely BPF-side.
///
/// Upstream's `examples/avgruntime.json` is a config nobody could load before
/// this; the assertion below is that the rule reaches the scheduler with the
/// bounds the config gave and the kind upstream assigns it.
#[test]
fn avg_runtime_from_a_config_reaches_the_scheduler() {
    let _lock = common::setup_test();
    let json = r#"[
      { "name": "shortlived",
        "matches": [[{ "AvgRuntime": [0, 5000] }]],
        "kind": { "Open": {} } },
      { "name": "rest", "matches": [[]], "kind": { "Open": {} } }
    ]"#;
    let loaded = parse_layer_config(json, &opts(2)).expect("AvgRuntime should load");
    assert_eq!(
        loaded.specs[0].matches[0][0],
        LayerMatch::AvgRuntime(0, 5000)
    );

    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&loaded.specs);
    let probes = LayeredProbes::new(&sched);
    assert_eq!(
        probes.match_kind(0, 0, 0),
        Some(LayeredMatchKind::AvgRuntime),
        "the config's rule must arrive as MATCH_AVG_RUNTIME, not be dropped"
    );
}
