# Makefile — builds scheduler .so files for scx_simulator
#
# Auto-discovers schedulers: any subdirectory containing wrapper.c becomes
# a scheduler. Each is built into $(BUILD_DIR)/libscx_<name>.so.
#
# Invoked by build.rs:
#   make -C schedulers/ BUILD_DIR=<out_dir>/schedulers \
#        SIMULATOR_DIR=.. ROOT_DIR=../../ \
#        BPF_INCLUDE=<DEP_BPF_INCLUDE path> CC=clang
#
# Or standalone (defaults assume relative paths from this directory):
#   make BUILD_DIR=build

# --- Paths (overridable by build.rs) ---
SIMULATOR_DIR ?= ..
ROOT_DIR      ?= $(SIMULATOR_DIR)/..
# C substrate vendored into the scx_simulator crate (build.rs overrides these).
CSRC_DIR      ?= $(SIMULATOR_DIR)/crates/scx_simulator/csrc
SCXTEST_DIR   ?= $(SIMULATOR_DIR)/crates/scx_simulator/scxtest
BUILD_DIR     ?= build

# CC defaults to 'cc' (gcc) in Make, but this code requires clang.
# Override the default unless the user explicitly set CC.
ifeq ($(origin CC),default)
CC := clang
endif

# BPF_INCLUDE must point to the libbpf header directory.
# When invoked from build.rs, this is set to DEP_BPF_INCLUDE.
# For standalone use: try pkg-config, then fall back to Cargo build cache.
BPF_INCLUDE   ?= $(or \
    $(shell pkg-config --cflags-only-I libbpf 2>/dev/null | sed 's/^-I//'),\
    $(firstword $(wildcard $(SIMULATOR_DIR)/target/debug/build/libbpf-sys-*/out/include) \
                $(wildcard $(SIMULATOR_DIR)/target/release/build/libbpf-sys-*/out/include)))

# --- Auto-discover schedulers ---
SCHEDS := $(patsubst %/wrapper.c,%,$(wildcard */wrapper.c))
SO_TARGETS := $(foreach s,$(SCHEDS),$(BUILD_DIR)/libscx_$(s).so)

# --- Common flags ---
# -Wno-implicit-function-declaration: scheduler wrappers call Rust-provided
# kfuncs (e.g. sim_bpf_get_smp_processor_id) without C declarations.
# These symbols are resolved at dlopen time via -rdynamic.
# Clang 21+ treats implicit function declarations as hard errors.
CFLAGS_BASE := -fPIC -DSCX_BPF_UNITTEST -g -O2 \
               -Wno-unused-parameter -Wno-unknown-attributes \
               -Wno-implicit-function-declaration \
               -Wconditional-uninitialized

# --- UB probe support (opt-in via SCX_SIM_UB_PROBE=1) ---
#
# Turns the UB classes where BPF and C DISAGREE into loud, deterministic
# aborts at the exact faulting site, instead of silent wrong answers. This is
# an exaggerated-diagnostics mode under Twin Design Principle 2: opt-in,
# never the default, because the default must match production and the
# production kernel does not abort on any of these.
#
#   integer-divide-by-zero  BPF defines x/0 -> 0 and x%0 -> x; C says UB.
#                           csrc/sim_sigfpe.c recovers the VALUE, but cannot
#                           undo optimisations clang already made on the
#                           assumption that the divisor is nonzero, so
#                           trapping is the only sound detection.
#   shift-exponent          BPF masks variable shifts (DST OP (SRC & 63) /
#                           ((u32)SRC & 31), kernel/bpf/core.c); C says UB.
#   signed-integer-overflow BPF wraps two's-complement; C says UB.
#
# -ftrivial-auto-var-init=pattern makes any surviving uninitialised read
# produce an obviously-bogus 0xAA.. value rather than a plausible one.
# DELIBERATELY NOT =zero: zero substitutes one plausible-looking value for
# the kernel's unconstrained garbage, so a divergence looks like a correct
# answer. Zero HIDES this bug class; pattern EXPOSES it.
#
# ORTHOGONAL TO SCX_SIM_UBSAN below, which landed separately (bdcd34d) and is
# the general -fsanitize=undefined abort-on-first-hit mode. This one is
# narrow and TRAPS: it names the three classes where BPF and C actually
# disagree, so a hit is a fidelity finding rather than any UB at all. Both
# are opt-in, gated on different variables, and may be enabled together.
ifeq ($(SCX_SIM_UB_PROBE),1)
CFLAGS_UB := -ftrivial-auto-var-init=pattern \
             -fsanitize=integer-divide-by-zero,shift-exponent,signed-integer-overflow \
             -fsanitize-trap=integer-divide-by-zero,shift-exponent,signed-integer-overflow
$(info scx-sim/schedulers: UB PROBE MODE ON — traps on BPF/C UB divergence, NOT production-fidelity)
else
CFLAGS_UB :=
endif

# --- Coverage support (opt-in via SCX_SIM_COVERAGE=1) ---
ifeq ($(SCX_SIM_COVERAGE),1)
CFLAGS_COV  := -fprofile-instr-generate -fcoverage-mapping
LDFLAGS_COV := -fprofile-instr-generate
else
CFLAGS_COV  :=
LDFLAGS_COV :=
endif

# --- UndefinedBehaviorSanitizer (opt-in via SCX_SIM_UBSAN=1) ---
#
# Why: these .so files are BPF scheduler source compiled as ordinary
# userspace C. BPF and C do not agree on everything, and where the C
# standard says "undefined" the compiler is free to miscompile silently.
# UBSan turns that silence into a diagnostic with a file:line.
#
# -fno-sanitize-recover=all IS LOAD-BEARING, DO NOT DROP IT. By default
# UBSan prints "runtime error: ..." and CONTINUES, and the process still
# exits 0. A gate built that way reports failures and passes anyway --
# measured, not assumed: an injected shift-out-of-bounds printed its
# diagnostic and the run still exited 0. With no-recover the first hit
# aborts and the exit status is non-zero, so callers actually fail.
#
# Deliberately added to CFLAGS and NOT to CFLAGS_BASE. That is what keeps
# sim_sigfpe.c, sim_rbc_trampoline.c and sim_deterministic_mem.c
# uninstrumented, for exactly the reasons they are already excluded from
# coverage: sim_sigfpe.c decodes x86 div at RIP and breaks if instruction
# layout shifts, and the other two are branchless/hot-path by design.
#
# Linking: the default build is -nostdlib, which cannot host a sanitizer
# runtime, so UBSan swaps it for the shared runtime. -shared-libsan is
# required because the Rust host binary that dlopens these is NOT
# sanitized, so the .so must carry its own runtime reference. The rpath
# is computed from the compiler at build time rather than written down,
# so no machine-specific path enters this file.
#
# CAVEAT -- do not combine with RBC replay. UBSan inserts conditional
# branches, and this project counts retired conditional branches for
# deterministic replay. A trace recorded from a UBSan build will not
# replay against a non-UBSan build. Use UBSan to find UB, not to record.
ifeq ($(SCX_SIM_UBSAN),1)
CFLAGS_UBSAN  := -fsanitize=undefined -fno-sanitize-recover=all
LDFLAGS_UBSAN := -fsanitize=undefined -shared-libsan \
                 -Wl,-rpath,$(shell $(CC) -print-runtime-dir)
else
CFLAGS_UBSAN  :=
LDFLAGS_UBSAN :=
endif

# -nostdlib is only correct when nothing needs a runtime. Coverage and UBSan
# each bring one, and they compose, so decide from both rather than letting
# whichever block is written last silently win.
ifeq ($(strip $(LDFLAGS_COV)$(LDFLAGS_UBSAN)),)
LDFLAGS_SO := -shared -nostdlib
else
LDFLAGS_SO := -shared $(LDFLAGS_COV) $(LDFLAGS_UBSAN)
endif

CFLAGS := $(CFLAGS_BASE) $(CFLAGS_COV) $(CFLAGS_UBSAN) $(CFLAGS_UB)

# --- scx cgroup_bw API version detection ---
#
# Between scx commits a08c9e272b (Apr 23 2026, OLD API) and 6f4921a6c6
# (Apr 30 2026, NEW API), the lib/cgroup_bw.bpf.c API switched some
# function signatures from `(struct cgroup *cgrp, ...)` to
# `(u64 cgrp_id, ...)`. Specifically:
#
#   - scx_cgroup_bw_throttled
#   - scx_cgroup_bw_consume
#   - scx_cgroup_bw_put_aside
#
# A new public symbol (`struct scx_task_cgroup_bw`) was added to
# scheds/include/lib/cgroup.h at the same flag-day. We probe for that
# symbol to pick the right wrapper signature for this scheduler at
# build time. See scx-sim/schedulers/lavd/wrapper.c for the matching
# `#ifdef SCX_CGROUP_BW_NEW_API` branches and tg task
# `fix-wrapper-c-old-new-cgroup-bw-api-conditional`.
#
# Detection is idempotent and runs at every Make invocation; the result
# is then injected as `-DSCX_CGROUP_BW_NEW_API=1` (NEW) or absent (OLD)
# via $(CFLAGS_BPF_API).
SCX_CGROUP_BW_API := $(shell \
    if test -f $(ROOT_DIR)/scheds/include/lib/cgroup.h && \
       grep -q '^struct scx_task_cgroup_bw' \
            $(ROOT_DIR)/scheds/include/lib/cgroup.h ; then \
        echo NEW ; \
    else \
        echo OLD ; \
    fi)
ifeq ($(SCX_CGROUP_BW_API),NEW)
CFLAGS_BPF_API := -DSCX_CGROUP_BW_NEW_API=1
else
CFLAGS_BPF_API :=
endif
$(info scx-sim/schedulers: detected cgroup_bw API = $(SCX_CGROUP_BW_API))
CFLAGS += $(CFLAGS_BPF_API)

# Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
# Phase 2 has shipped (Stages A-B-C-D landed; Stage E EXIT CRITERION
# met -- canonical cpu-bw-stall-bug reproduces ExitKind::ErrorStall
# deterministically through the production library at
# runnable_for_ns=80002523).
#
# The build-time gate that USED to select between the pre-Phase-2 weak
# `scx_cgroup_bw_*` shims and the real `scx/lib/cgroup_bw.bpf.c`
# library is RETIRED. Stage D deleted the rollback code path entirely
# (cgroup_bw_ffi.rs, the wrapper.c #ifndef weak forwarders) so there
# is no other mode to select. The define is unconditionally emitted to
# keep the wrapper.c #ifdef SCXSIM_PHASE2_REAL_CGROUP_BW conditionals
# always-true; cleanup of the leftover #ifdefs is a follow-up but they
# do not change behavior.
CFLAGS += -DSCXSIM_PHASE2_REAL_CGROUP_BW=1

# Per-scheduler wrapper.c files include scx library sources via relative paths
# like "../../scx/lib/ravg.bpf.c" that anchor on the schedulers/ directory (two
# real levels above the scx submodule: schedulers -> scx-sim -> repo-root/scx).
# -I$(SIMULATOR_DIR)/schedulers makes that resolve. (Previously this anchored on
# scx-sim/csrc, before csrc moved into the scx_simulator crate; scheds/include
# cannot substitute because it is a symlink into the submodule, so `../../`
# traverses the target.) A future generic wrapper generation replaces these
# fragile relative includes with a stable -I.
INCLUDES := -I$(CSRC_DIR) \
            -I$(SCXTEST_DIR) \
            -I$(SIMULATOR_DIR)/schedulers \
            -I$(ROOT_DIR)/scheds/include \
            -I$(ROOT_DIR)/scheds/include/lib \
            -I$(ROOT_DIR)/scheds/vmlinux \
            -I$(ROOT_DIR)/scheds/vmlinux/arch/x86 \
            -I$(ROOT_DIR)/scheds/include/bpf-compat \
            $(if $(BPF_INCLUDE),-I$(BPF_INCLUDE))

# Shared source files compiled into every .so
# NOTE: sim_sdt_stubs.c is NOT included here — it's compiled into the main
# binary and exported via -rdynamic. This ensures there's only one copy of
# the SDT hash table state, allowing sim_sdt_reset() to work correctly.
# overrides.c is included to provide weak stubs that the linker can use
# if the main binary's strong definitions aren't found (they should be).
SHARED_SRCS := $(CSRC_DIR)/sim_bpf_stubs.c \
               $(CSRC_DIR)/sim_sigfpe.c \
               $(SCXTEST_DIR)/overrides.c

# --- Per-scheduler config ---
# Each scheduler can define EXTRA_CFLAGS_<name> and EXTRA_INCLUDES_<name>
# Set default goal BEFORE including config files, so rules in config.mk
# don't accidentally become the default target.
.DEFAULT_GOAL := all
$(foreach s,$(SCHEDS),$(eval -include $(s)/config.mk))

# --- e9patch tool path ---
# Look for e9tool in third_party/ first, then fall back to PATH.
E9TOOL ?= $(or $(wildcard $(SIMULATOR_DIR)/third_party/e9patch/e9tool),$(shell which e9tool 2>/dev/null))
E9PATCH ?= $(or $(wildcard $(SIMULATOR_DIR)/third_party/e9patch/e9patch),$(shell which e9patch 2>/dev/null))

# --- Build rules ---

.PHONY: all clean ubsan e9 e9-rip-trampoline $(SCHEDS)

ifeq ($(BPF_INCLUDE),)
all:
	$(error BPF_INCLUDE is empty — install libbpf-devel or run 'cargo build' first to populate target/debug/build/libbpf-sys-*/out/include)
endif

all: $(SO_TARGETS)
	@echo "Built $(words $(SO_TARGETS)) scheduler(s): $(SCHEDS)"

# Individual scheduler targets (e.g., make simple)
$(foreach s,$(SCHEDS),$(eval $(s): $(BUILD_DIR)/libscx_$(s).so))

clean:
	rm -rf $(BUILD_DIR)

# Build every scheduler with UBSan into a SEPARATE build dir, so the
# instrumented .so files never get mistaken for the normal ones (they are not
# interchangeable: see the RBC-replay caveat above).
#
# Run them with:
#     scxsim run ... --scheduler-file <dir>/build-ubsan/libscx_<name>.so
#
# A hit aborts the run with a non-zero exit and a file:line diagnostic. If you
# see "runtime error:" but a zero exit, -fno-sanitize-recover=all has been
# dropped and the gate is no longer a gate.
ubsan:
	@$(MAKE) --no-print-directory SCX_SIM_UBSAN=1 BUILD_DIR=$(BUILD_DIR)-ubsan all
	@echo "UBSan schedulers in $(BUILD_DIR)-ubsan (not interchangeable with $(BUILD_DIR))"

# --- Object compilation ---

# Per-scheduler wrapper.c -> <name>_wrapper.o
define WRAPPER_RULE
$(BUILD_DIR)/$(1)_wrapper.o: $(1)/wrapper.c | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(INCLUDES) $(EXTRA_INCLUDES_$(1)) $(EXTRA_CFLAGS_$(1)) \
		-MMD -MF $(BUILD_DIR)/$(1)_wrapper.d \
		-c -o $$@ $$<
endef

# Per-scheduler sim_bpf_stubs.c -> <name>_sim_bpf_stubs.o
define STUBS_RULE
$(BUILD_DIR)/$(1)_sim_bpf_stubs.o: $(CSRC_DIR)/sim_bpf_stubs.c | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(INCLUDES) $(EXTRA_INCLUDES_$(1)) $(EXTRA_CFLAGS_$(1)) \
		-MMD -MF $(BUILD_DIR)/$(1)_sim_bpf_stubs.d \
		-c -o $$@ $$<
endef

# Per-scheduler sim_sigfpe.c -> <name>_sim_sigfpe.o
# NOTE: compiled WITHOUT $(INCLUDES) to avoid vmlinux.h / signal.h conflicts
# NOTE: compiled WITHOUT coverage flags — the signal handler manually decodes
# x86 div/idiv instructions at RIP; coverage instrumentation changes the
# instruction layout around div, causing the handler to miscalculate lengths
# and jump to invalid addresses.
define SIGFPE_RULE
$(BUILD_DIR)/$(1)_sim_sigfpe.o: $(CSRC_DIR)/sim_sigfpe.c | $(BUILD_DIR)
	$(CC) $(CFLAGS_BASE) \
		-MMD -MF $(BUILD_DIR)/$(1)_sim_sigfpe.d \
		-c -o $$@ $$<
endef

# Per-scheduler overrides.c -> <name>_overrides.o
define OVERRIDES_RULE
$(BUILD_DIR)/$(1)_overrides.o: $(SCXTEST_DIR)/overrides.c | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(INCLUDES) $(EXTRA_INCLUDES_$(1)) $(EXTRA_CFLAGS_$(1)) \
		-MMD -MF $(BUILD_DIR)/$(1)_overrides.d \
		-c -o $$@ $$<
endef

# Per-scheduler sim_rbc_trampoline.c -> <name>_sim_rbc_trampoline.o
# NOTE: compiled WITHOUT $(INCLUDES) to avoid vmlinux.h conflicts.
# NOTE: compiled WITHOUT coverage flags — the trampoline is called on every
# instrumented Jcc and must have minimal overhead.
define RBC_TRAMPOLINE_RULE
$(BUILD_DIR)/$(1)_sim_rbc_trampoline.o: $(CSRC_DIR)/sim_rbc_trampoline.c | $(BUILD_DIR)
	$(CC) $(CFLAGS_BASE) \
		-MMD -MF $(BUILD_DIR)/$(1)_sim_rbc_trampoline.d \
		-c -o $$@ $$<
endef

# Per-scheduler sim_deterministic_mem.c -> <name>_sim_deterministic_mem.o
# Provides branchless memset/memcpy that override glibc's SIMD-optimized
# versions (which have nondeterministic alignment-dependent branches).
# NOTE: compiled WITHOUT $(INCLUDES) to avoid vmlinux.h conflicts.
# NOTE: compiled WITHOUT coverage flags — these are hot-path memory ops.
define DETERMINISTIC_MEM_RULE
$(BUILD_DIR)/$(1)_sim_deterministic_mem.o: $(CSRC_DIR)/sim_deterministic_mem.c | $(BUILD_DIR)
	$(CC) $(CFLAGS_BASE) \
		-MMD -MF $(BUILD_DIR)/$(1)_sim_deterministic_mem.d \
		-c -o $$@ $$<
endef

# Link .o files into .so
# NOTE: sim_sdt_stubs.o is NOT linked here — SDT functions resolve from the
# main binary via -rdynamic. This ensures there's only one copy of the SDT
# hash table state, allowing sim_sdt_reset() to work correctly.
# NOTE: -Wl,--init=e9_so_init creates a DT_INIT entry so e9tool can inject
# its loader. Without it, -nostdlib .so files have no init section.
define SO_RULE
$(BUILD_DIR)/libscx_$(1).so: $(BUILD_DIR)/$(1)_wrapper.o $(BUILD_DIR)/$(1)_sim_bpf_stubs.o $(BUILD_DIR)/$(1)_sim_sigfpe.o $(BUILD_DIR)/$(1)_overrides.o $(BUILD_DIR)/$(1)_sim_rbc_trampoline.o $(BUILD_DIR)/$(1)_sim_deterministic_mem.o
	$(CC) $(LDFLAGS_SO) -Wl,--init=e9_so_init -o $$@ $$^
endef

# Instantiate rules for each scheduler
$(foreach s,$(SCHEDS),$(eval $(call WRAPPER_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call STUBS_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call SIGFPE_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call OVERRIDES_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call RBC_TRAMPOLINE_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call DETERMINISTIC_MEM_RULE,$(s))))
$(foreach s,$(SCHEDS),$(eval $(call SO_RULE,$(s))))

# --- e9patch post-processing ---
# Instrument all conditional branches with a software RBC counter trampoline.
#
# The e9tool 'jcc' matcher covers all instructions that increment the PMU's
# BR_INST_RETIRED.COND counter (Zydis COND_BR category):
#   - Jcc (JE, JNE, JG, JL, JA, JB, etc.) — the common case
#   - LOOP, LOOPE/LOOPZ, LOOPNE/LOOPNZ — legacy loop instructions
#   - JCXZ, JECXZ, JRCXZ — counter-register conditional jumps
# Modern compilers never emit LOOP/JRCXZ (confirmed: 0 instances across
# all scheduler .so files), but the matcher handles them for correctness.
#
# The trampoline binary is compiled with e9compile.sh from e9patch. It uses
# dlsym to resolve E9_SHARED_RBC (shared state struct in the main binary) and
# e9_preempt_yield (yield function in the main binary). The fast path
# (counter > 0) is a single decrement + branch with no external calls.
#
# The .so's e9_arm/e9_disarm/e9_worker_setup (from sim_rbc_trampoline.c)
# write to the same E9_SHARED_RBC struct, ensuring consistent state.
#
# Usage: make e9  (requires e9tool; builds _e9.so for all schedulers)

E9COMPILE ?= $(SIMULATOR_DIR)/third_party/e9patch/e9compile.sh
E9_TRAMPOLINE_SRC := $(CSRC_DIR)/e9_rbc_trampoline.c
E9_TRAMPOLINE_BIN := $(BUILD_DIR)/e9_rbc_trampoline
E9_RIP_TRAMPOLINE_SRC := $(CSRC_DIR)/e9_rip_trampoline.c
E9_RIP_TRAMPOLINE_BIN := $(BUILD_DIR)/e9_rip_trampoline

# Build the trampoline binary using e9compile.sh (must run from e9patch dir
# so that #include "stdlib.c" resolves to e9patch/examples/stdlib.c).
$(E9_TRAMPOLINE_BIN): $(E9_TRAMPOLINE_SRC) | $(BUILD_DIR)
	@if [ ! -f "$(E9COMPILE)" ]; then \
		echo "ERROR: e9compile.sh not found. Build e9patch: cd third_party/e9patch && ./build.sh"; \
		exit 1; \
	fi
	cp $(E9_TRAMPOLINE_SRC) $(SIMULATOR_DIR)/third_party/e9patch/e9_rbc_trampoline.c
	cd $(SIMULATOR_DIR)/third_party/e9patch && ./e9compile.sh e9_rbc_trampoline.c
	mv $(SIMULATOR_DIR)/third_party/e9patch/e9_rbc_trampoline $(E9_TRAMPOLINE_BIN)
	rm -f $(SIMULATOR_DIR)/third_party/e9patch/e9_rbc_trampoline.c \
	      $(SIMULATOR_DIR)/third_party/e9patch/e9_rbc_trampoline.o

# Build the RIP trampoline binary for arbitrary-address replay.
$(E9_RIP_TRAMPOLINE_BIN): $(E9_RIP_TRAMPOLINE_SRC) | $(BUILD_DIR)
	@if [ ! -f "$(E9COMPILE)" ]; then \
		echo "ERROR: e9compile.sh not found. Build e9patch: cd third_party/e9patch && ./build.sh"; \
		exit 1; \
	fi
	cp $(E9_RIP_TRAMPOLINE_SRC) $(SIMULATOR_DIR)/third_party/e9patch/e9_rip_trampoline.c
	cd $(SIMULATOR_DIR)/third_party/e9patch && ./e9compile.sh e9_rip_trampoline.c
	mv $(SIMULATOR_DIR)/third_party/e9patch/e9_rip_trampoline $(E9_RIP_TRAMPOLINE_BIN)
	rm -f $(SIMULATOR_DIR)/third_party/e9patch/e9_rip_trampoline.c \
	      $(SIMULATOR_DIR)/third_party/e9patch/e9_rip_trampoline.o

define E9_RULE
$(BUILD_DIR)/libscx_$(1)_e9.so: $(BUILD_DIR)/libscx_$(1).so $(E9_TRAMPOLINE_BIN)
	@if [ -z "$(E9TOOL)" ]; then \
		echo "ERROR: e9tool not found. Build it: cd third_party/e9patch && ./build.sh"; \
		exit 1; \
	fi
	E9PATCH=$(E9PATCH) $(E9TOOL) \
		-M 'jcc' \
		-P 'rbc_trampoline()@$(E9_TRAMPOLINE_BIN)' \
		-o $$@ $$<
endef
$(foreach s,$(SCHEDS),$(eval $(call E9_RULE,$(s))))

E9_TARGETS := $(foreach s,$(SCHEDS),$(BUILD_DIR)/libscx_$(s)_e9.so)
e9: $(E9_TARGETS) $(E9_RIP_TRAMPOLINE_BIN)
	@echo "Built $(words $(E9_TARGETS)) e9patch scheduler(s): $(SCHEDS)"
	@echo "Built RIP trampoline: $(E9_RIP_TRAMPOLINE_BIN)"

# Standalone target for the RIP trampoline (used by e9patch RIP replay).
.PHONY: e9-rip-trampoline
e9-rip-trampoline: $(E9_RIP_TRAMPOLINE_BIN)
	@echo "Built RIP trampoline: $(E9_RIP_TRAMPOLINE_BIN)"

# Create build directory
$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

# --- Variable introspection ---
# `make print-CFLAGS_BASE` prints one variable's expanded value. Used by
# scripts/check_ub_fidelity.sh to assert the UB-policy flags are really in the
# compile line rather than merely mentioned in a comment.
print-%:
	@echo '$($*)'

# Include generated dependency files for incremental rebuilds
-include $(wildcard $(BUILD_DIR)/*.d)

# Catch-all: stale .d files may reference headers that have moved or been
# deleted (e.g. after upstream sync). Without this rule, Make errors out with
# "No rule to make target". The catch-all silently ignores the stale dep;
# the next real compilation regenerates the .d file with correct paths.
%.h:
	@:
