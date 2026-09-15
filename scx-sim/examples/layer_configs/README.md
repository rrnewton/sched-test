scx_layered layer configs
=========================

These are **layer configurations**, not workloads. They go to
`scxsim run --layer-config <file>`; the rt-app workloads one directory up go
to the same command as the positional argument.

They live in a subdirectory on purpose: `examples/*.json` is auto-discovered
as rt-app workloads by `tests/examples_matrix.rs` and
`docs/guide/tests/test_examples.sh`, and a layer config is not one.

Format
------

The format is **scx_layered's own**, unchanged — the one
`scx/scheds/rust/scx_layered/src/config.rs` defines and production passes as a
**positional** argument, `scx_layered f:<path>`. There is no `--spec` flag;
upstream's own help reads `$ scx_layered f:example.json`. A file that works
against the real scheduler is meant to work here, and most of
`scx/scheds/rust/scx_layered/examples/*.json` load directly.

Running one
-----------

Every command below assumes you are in `scx-sim/` and that `scxsim` resolves.
It is not installed by default — either put `target/release/` on `$PATH` or
spell the path, as `examples/README.md` also notes:

    cargo build --release -p scx_simulator --bin scxsim
    SCXSIM=./target/release/scxsim

    $SCXSIM run -s layered --cpus 4 \
        --layer-config examples/layer_configs/edge_like.json \
        --layer-report \
        examples/cgroup_hierarchy.json

`edge_like.json` is written against `../cgroup_hierarchy.json`, whose tasks are
`background_0`, `background_1`, `interactive_0` and `interactive_1` in the
cgroups `/background` and `/interactive`.

`--layer-report` is the part worth having. A layered run that exits 0 is *not*
evidence that any rule fired: with no `--layer-config` the scheduler runs one
catch-all layer and never compares a rule against any string. The report prints
the scheduler's own per-OR-group verdict, so you can see which term matched and
which one rejected:

    task "background_1" (pid 2) comm="background_1" cgroup="background/"
      -> layer 2 "everything_else"
         rejected layer 0 "interactive" OR 0: failed at term 0 = CgroupPrefix("interactive/")
         rejected layer 1 "bg_worker_0" OR 0: failed at term 1 = CommPrefix("background_0")
         MATCHED  layer 2 "everything_else" OR 0: <catch-all, no terms>

Note the second rejection: term 0 held (`background_1` *is* in
`background/`) and term 1 did not. That asymmetry is only producible by
actually comparing both strings.

What is refused, and why
------------------------

Fields and match kinds scxsim cannot honour are refused **by name**, with the
reason, rather than ignored — a config that loads with fields quietly dropped
looks like the production configuration and is not one.

Match kinds are never waivable, because dropping a term from an AND group
makes it match strictly more tasks. Layer fields can be waived with
`--layer-config-drop <name>,...`, and every waived field is echoed before the
run. **To see the whole waivable set, name one that does not exist** — the
error lists them:

    $ $SCXSIM run -s layered --layer-config <any> --layer-config-drop ? <workload>
    error: --layer-config-drop: ?: not a refusable scx_layered config field
    Waivable fields are: idle_resume_us, membw_gb, xnuma_threshold, ...

The authority is `crates/scx_simulator/src/safe/layered_config.rs`
(`Unsupported`), not this file.

Caveats the CLI prints for you
------------------------------

`scxsim run` publishes the wrapper's **static weight-proportional CPU split**,
not the output of scx_layered's userspace allocator. Everything that is only
an input to that allocator — `util_range`, `cpus_range` / `cpus_range_frac`,
`util_includes_open_cputime`, `nodes`, `llcs`, `xnuma_threshold` /
`xnuma_threshold_delta`, and `growth_algo`'s allocation role — therefore does
not shape the CPU sets here. The CLI names the ones your config actually uses,
on every `--layer-config` run. All of them ARE honoured when the Rust API
enables the control loop (`layered_enable_control_loop`).

`perf` is published, and the scheduler's own `scx_bpf_cpuperf_set` call runs —
but the engine models no DVFS, so the level is recorded on the CPU and does not
change how fast simulated work completes. The CLI says so when a config sets it.

Topology is the other limit, and it is not a caveat but a refusal: `scxsim run`
gives layered a flat 1-LLC / 1-node machine, so a config with `NumaNode(1)` is
rejected naming the node count it got. That is upstream's behaviour too, and
the multi-node case is reachable from the Rust API today
(`DynamicScheduler::layered_with_topology`).
