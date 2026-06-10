load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

oncall("sched_ext")

# Single-crate approach: matches Cargo's structure exactly
rust_library(
    name = "schtest_lib",
    srcs = ["src/lib.rs"]
    + glob([
        "src/util/**/*.rs",
        "src/workloads/**/*.rs",
        "src/cases/**/*.rs",
    ]),
    crate = "schtest",
    crate_root = "src/lib.rs",
    features = ["cargo_build"],
    test_deps = ["fbsource//third-party/rust:more-asserts"],
    deps = [
        "fbsource//third-party/rust:anyhow",
        "fbsource//third-party/rust:criterion",
        "fbsource//third-party/rust:inventory",
        "fbsource//third-party/rust:libc",
        "fbsource//third-party/rust:nix",
        "fbsource//third-party/rust:procfs",
        "fbsource//third-party/rust:rand_09",
        "fbsource//third-party/rust:tdigest",
        "fbsource//third-party/rust:term_size",
    ],
)

rust_binary(
    name = "schtest",
    srcs = ["src/main.rs"],
    crate = "schtest",
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
