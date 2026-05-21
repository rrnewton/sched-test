# Makefile for the scxsim mdbook guide.
#
# Targets:
#   make / make build       Build the HTML book under ./book/
#                           (auto-installs mdbook via cargo if missing).
#   make serve              Build + serve on http://localhost:$(PORT)
#                           with auto-reload (default PORT=3000).
#   make install-mdbook     Force `cargo install mdbook`.
#   make ensure-mdbook      No-op if mdbook is in PATH, otherwise install.
#   make check              Build and fail on any mdbook ERROR/WARN output
#                           (CI-friendly).
#   make clean              Remove the ./book/ output directory.
#
# The book/ output directory is ignored at the workspace root
# (see /.gitignore "scx-sim/docs/guide/book/").
#
# Variables:
#   PORT      Port for `make serve`. Default: 3000.
#   MDBOOK    Path to mdbook binary. Default: first one on PATH.

PORT   ?= 3000
MDBOOK ?= mdbook

.PHONY: all build serve install-mdbook ensure-mdbook check clean

all: build

# ----------------------------------------------------------------------
# Build / serve
# ----------------------------------------------------------------------

build: ensure-mdbook
	$(MDBOOK) build

serve: ensure-mdbook
	$(MDBOOK) serve --port $(PORT) --hostname 0.0.0.0 --open

# ----------------------------------------------------------------------
# mdbook bootstrap
# ----------------------------------------------------------------------

# Install mdbook unconditionally.
install-mdbook:
	@echo ">> Installing mdbook via cargo (this may take a few minutes)..."
	cargo install mdbook

# Idempotent check: only install if mdbook isn't already on PATH.
ensure-mdbook:
	@if ! command -v $(MDBOOK) >/dev/null 2>&1; then \
	    echo ">> $(MDBOOK) not found in PATH; installing via cargo..."; \
	    cargo install mdbook; \
	else \
	    printf ">> %s: %s (%s)\n" "$(MDBOOK)" "$$(command -v $(MDBOOK))" "$$($(MDBOOK) --version)"; \
	fi

# ----------------------------------------------------------------------
# CI helpers
# ----------------------------------------------------------------------

# CI-friendly build: fail on any ERROR or WARN line emitted by mdbook.
# Useful as a guard against broken intra-book links or missing chapter
# files referenced from SUMMARY.md.
check: ensure-mdbook
	@LOG=$$(mktemp); \
	    $(MDBOOK) build 2>&1 | tee $$LOG; \
	    rc=$$?; \
	    if [ $$rc -ne 0 ]; then \
	        echo ">> mdbook build exited with $$rc"; rm -f $$LOG; exit $$rc; \
	    fi; \
	    if grep -E '^(\[ERROR\]|\[WARN\])' $$LOG; then \
	        echo ">> mdbook build produced errors or warnings (treated as failure)"; \
	        rm -f $$LOG; exit 1; \
	    fi; \
	    rm -f $$LOG

# ----------------------------------------------------------------------
# Cleanup
# ----------------------------------------------------------------------

clean:
	rm -rf book/
