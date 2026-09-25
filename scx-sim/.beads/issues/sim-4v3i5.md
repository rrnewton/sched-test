---
title: 'scx-sim coverage tooling: coverage.sh aborts on the first failing test and searches the wrong target dir; merged per-scheduler profiles overstate coverage'
status: open
priority: 2
issue_type: bug
labels:
- coverage
depends_on:
  sim-hyr11: related
created_at: 2026-09-25T05:18:55.284703133+00:00
updated_at: 2026-09-25T05:18:55.284703133+00:00
---

# Description

DEFECT

The in-tree coverage scripts cannot produce a correct per-scheduler report at this pin. The 2026-09-24 re-measure had to replace them. Each defect below was reproduced at sched-test 24d864c6.

`coverage.sh`

1. The script runs with `set -euo pipefail`, and the test step is `cargo test --all -- --test-threads=1 2>&1 | tee ...` without `--no-fail-fast`.
   - The two failing layered tests (sim-hyr11) stop cargo at the first failing target, and pipefail then aborts the script.
   - So no profile is ever merged.
2. `PROJ_ROOT="$(cd .. && pwd)"` is the sched-test root, not scx-sim.
   - `find "$PROJ_ROOT/target" ... schedulers_cov` therefore searches a directory that does not exist. The build output is in `scx-sim/target`.
   - The same variable drives `find "$PROJ_ROOT" -name '*.profraw' -delete`, which deletes raw profiles anywhere under sched-test.
3. Scheduler .so discovery runs `find` under `$(dirname <test binary>)`.
   - It works only because `target/debug/scxsim` happens to be listed in `test-binaries.txt`.
   - It also picks up six non-coverage `out/schedulers/libscx_simple.so` builds.
4. `FIRST_BIN=$(head -1 test-binaries.txt)` depends on the order of that file.

`scripts/gen_scheduler_fn_coverage.sh`

5. It covers only `SCHEDS=(simple lavd cosmos)`, missing layered, mitosis and tickless.
6. `find_so()` takes `find ... | head -1`, in filesystem order, so which .so it picks is not deterministic.

Merged profiles

7. Every scheduler wrapper is compiled from a file named `wrapper.c`. So static functions from different schedulers get the same profile name, `wrapper.c:<name>`, and a profile merged across the whole test suite attributes their counters to each other.
   - Measured on the after tree, merged versus per-module profile. The table is in the harness capture, `tables/after/llvm_cov_profiles.tsv`.
   - Covered functions, merged vs per-module: cosmos 113 vs 87, lavd 366 vs 359, layered 250 vs 231, mitosis 120 vs 103, simple 41 vs 34, tickless 72 vs 51.
   - Cosmos lines covered: 898 merged vs 700 per-module.
   - The before tree shows the same overstatement.
   - Per-scheduler coverage must be computed from each scheduler's own profiles only, or the wrappers need distinct file names.

FIX DIRECTION

- Add `--no-fail-fast`, and record the failing targets instead of aborting.
- Derive paths from the script's own directory.
- Find scheduler libraries under `target/.../schedulers_cov` explicitly.
- Cover all six schedulers.
- Pick libraries deterministically.
- Merge profiles per scheduler library.

The harness pipeline in coverage/scx_pin_bump_20260924 (`cov_driver.sh`, `sim_cov.py`) shows one working arrangement.

ACCEPTANCE

- `coverage.sh` completes while sim-hyr11 still fails.
- It deletes nothing outside scx-sim.
- It reports all six schedulers with per-module numbers.
- Two runs produce identical reports.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). The capture is coverage/scx_pin_bump_20260924 in the dev harness (rrnewton/dev-sched-test).
