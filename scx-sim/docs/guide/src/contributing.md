# Contributing

## Building the guide locally

```bash
cd scx-sim/docs/guide
make            # auto-installs mdbook via cargo if missing, then builds ./book/
make serve      # builds + serves on http://localhost:3000 with auto-reload
```

`make check` is the CI-friendly invocation: it fails on any mdbook
ERROR or WARN line (typically a broken intra-book link or a chapter
referenced from `SUMMARY.md` that doesn't yet exist).

## File layout

```text
scx-sim/docs/guide/
├── book.toml         # mdbook config (theme, edit-url-template, search)
├── Makefile          # build / serve / check / clean
└── src/              # all chapters live here
    ├── SUMMARY.md    # table of contents (drives the sidebar)
    ├── introduction.md, overview.md
    ├── getting-started.md / getting-started/*.md
    ├── concepts.md / concepts/*.md
    ├── running-simulations.md / running-simulations/*.md
    ├── recipes.md / recipes/*.md
    ├── reference/*.md
    └── architecture.md / architecture/*.md
```

## Editing existing chapters

The initial guide content is in place; ongoing changes should preserve
the same style:

1. **Generate where possible.** A future improvement generates the
   CLI reference from `--help` output at build time, and generates
   the exit-code table from the `ExitKind` enum in `safe/types.rs`.
   Hand-writing those tables is intentionally a stop-gap; if you
   touch them, prefer adding the generator over re-hand-editing.
2. **Cite source.** Every concrete claim in a chapter should link
   either to a source file (pinned to `simulator.v6`) or to a
   worked-example fixture under `tests/fixtures/` or `examples/`.
3. **Keep examples runnable.** If you change a CLI flag default or
   rename a workload, update every chapter that quotes it. The
   `make test-examples` gate (see below) catches workload breakage but
   not stale flag text.

## Style

- Diátaxis split: tutorials in **Getting Started**, how-tos in
  **Running Simulations** and **Recipes**, conceptual content in
  **Concepts** and **Architecture**, look-it-up content in
  **Reference**. Don't mix modes within a page.
- Code samples should run as-is against the bundled `examples/`. If a
  sample requires a non-default fixture, name it inline.
- Prefer absolute GitHub URLs to scx-sim source files (pinned to
  `simulator.v6`) until in-tree cross-linking is wired up; this keeps
  the published HTML usable as a standalone document.

## Keeping examples runnable (CI)

Every `*.json` workload under `scx-sim/examples/` is exercised on
every CI run by the
[`scxsim guide examples`](https://github.com/rrnewton/sched-test/actions/workflows/scxsim-examples.yml)
GitHub Actions workflow, which invokes:

```bash
cd scx-sim
make test-examples
```

`make test-examples` is a thin wrapper around
[`docs/guide/tests/test_examples.sh`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/docs/guide/tests/test_examples.sh).
The script enumerates `examples/*.json`, runs each through
`scxsim run --duration 100ms --watchdog 5s --cpus 4`, and asserts
exit code 0. Failures print the captured stderr/stdout so the
breakage is diagnosable from the CI log alone.

Why this is a separate workflow from `simulator.yml` (the full
validate.sh + cargo-nextest run): the guide's examples are a
**reader-facing contract** — if `examples/hello.json` no longer
parses or no longer exits 0, every reader who copy-pastes from
*Getting Started* hits the broken state on their first attempt. The
narrow workflow gives that contract a fast, dedicated red-light
signal that doesn't depend on the full test suite being green.

### Adding a new example

1. Drop a new `*.json` into `scx-sim/examples/`. The script picks it
   up automatically — no allow-list to update.
2. Add a one-line row to the table at the top of
   `scx-sim/examples/README.md` so users browsing the directory know
   what it demonstrates.
3. Run `make test-examples` locally to confirm exit 0. If the example
   only makes sense under a specific scheduler, widen the matrix:

   ```bash
   make test-examples SCXSIM_TEST_SCHEDULERS="simple lavd"
   ```

   …and add that scheduler to `SCXSIM_TEST_SCHEDULERS` in
   `.github/workflows/scxsim-examples.yml` so CI exercises the same
   combination you tested locally.
4. Reference the example from whichever guide page motivates it
   (e.g. add it to the *Running Simulations* recipe that demonstrates
   the relevant feature).

### Tunables

`docs/guide/tests/test_examples.sh` reads several `SCXSIM_TEST_*`
environment variables for ad-hoc tuning (duration, watchdog, CPU
count, scheduler matrix, wall-clock timeout, alternate examples
directory). The script header documents them in full.
