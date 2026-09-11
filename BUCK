load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

oncall("sched_ext")

# Single-crate approach: matches Cargo's structure exactly
rust_library(
    name = "schtest_lib",
    srcs = ["schtest/rust/src/lib.rs"]
    + glob([
        "schtest/rust/src/util/**/*.rs",
        "schtest/rust/src/workloads/**/*.rs",
        "schtest/rust/src/cases/**/*.rs",
    ]),
    crate = "schtest",
    crate_root = "schtest/rust/src/lib.rs",
    edition = "2021",
    features = ["cargo_build"],
    named_deps = {
        "cgroups_rs": "fbsource//third-party/rust:cgroups-rs-05-fs",
    },
    test_deps = ["fbsource//third-party/rust:more-asserts"],
    deps = [
        "fbsource//third-party/rust:anyhow",
        "fbsource//third-party/rust:criterion",
        "fbsource//third-party/rust:inventory",
        "fbsource//third-party/rust:libc",
        "fbsource//third-party/rust:nix",
        "fbsource//third-party/rust:num_cpus",
        "fbsource//third-party/rust:procfs",
        "fbsource//third-party/rust:quickcheck",
        "fbsource//third-party/rust:rand_09",
        "fbsource//third-party/rust:regex",
        "fbsource//third-party/rust:serde",
        "fbsource//third-party/rust:tdigest",
        "fbsource//third-party/rust:term_size",
    ],
)

rust_binary(
    name = "schtest",
    srcs = ["schtest/rust/src/main.rs"],
    crate = "schtest",
    crate_root = "schtest/rust/src/main.rs",
    edition = "2021",
    features = ["cargo_build"],
    deps = [
        "fbsource//third-party/rust:anyhow",
        "fbsource//third-party/rust:clap",
        "fbsource//third-party/rust:inventory",
        "fbsource//third-party/rust:libtest-with",
        "fbsource//third-party/rust:once_cell",
        ":schtest_lib",
    ],
)

rust_binary(
    name = "benchmark_cpu",
    srcs = ["schtest/rust/src/bin/benchmark_cpu.rs"],
    crate = "benchmark_cpu",
    crate_root = "schtest/rust/src/bin/benchmark_cpu.rs",
    edition = "2021",
    features = ["cargo_build"],
    deps = [
        "fbsource//third-party/rust:clap",
        "fbsource//third-party/rust:libc",
        "fbsource//third-party/rust:nix",
        "fbsource//third-party/rust:rand_09",
        "fbsource//third-party/rust:serde_json",
        ":schtest_lib",
    ],
)
