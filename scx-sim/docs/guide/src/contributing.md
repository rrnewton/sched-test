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

## Filling in stubs

Every stub page is marked with a `> **Status — stub.**` block. The
plan is to fill them in incrementally:

1. **High-value, low-volatility first.** Exit codes, output formats,
   and the rt-app workload schema are stable; the CLI flag list and
   recipe details change more often.
2. **Generate where possible.** A future improvement generates the
   CLI reference from `--help` output at build time, and generates
   the exit-code table from the `ExitKind` enum in `safe/types.rs`.
   Hand-writing those tables is intentionally a stop-gap.
3. **Cite source.** Every concrete claim in a chapter should link
   either to a source file or to a worked-example fixture under
   `tests/fixtures/` or `examples/`.

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
