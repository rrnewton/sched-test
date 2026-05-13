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
               -Wno-implicit-function-declaration

# --- Coverage support (opt-in via SCX_SIM_COVERAGE=1) ---
ifeq ($(SCX_SIM_COVERAGE),1)
CFLAGS_COV := -fprofile-instr-generate -fcoverage-mapping
LDFLAGS_SO := -shared -fprofile-instr-generate
else
CFLAGS_COV :=
LDFLAGS_SO := -shared -nostdlib
endif

CFLAGS := $(CFLAGS_BASE) $(CFLAGS_COV)

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
# build-time gate that selects between the pre-Phase-2 weak
# `scx_cgroup_bw_*` shims (delegating to the engine `BandwidthManager`)
# and the real `scx/lib/cgroup_bw.bpf.c` library compiled into the .so.
#
# DEFAULT IS NOW ON. Phase 2 has shipped (Stages A-B-C-D landed; Stage
# E EXIT CRITERION met -- canonical cpu-bw-stall-bug reproduces
# `ExitKind::ErrorStall` deterministically through the production
# library at runnable_for_ns=80002523). The gate stays as a build-time
# off-switch ONLY for emergency rollback during integration; set
# `SCXSIM_PHASE2_REAL_CGROUP_BW=0` to revert to the engine-side
# BandwidthManager mirror path.
SCXSIM_PHASE2_REAL_CGROUP_BW ?= 1
ifeq ($(SCXSIM_PHASE2_REAL_CGROUP_BW),1)
CFLAGS += -DSCXSIM_PHASE2_REAL_CGROUP_BW=1
$(info scx-sim/schedulers: SCXSIM_PHASE2_REAL_CGROUP_BW=1 -- compiling cgroup_bw.bpf.c into LAVD .so)
else
$(info scx-sim/schedulers: SCXSIM_PHASE2_REAL_CGROUP_BW=0 -- ROLLBACK MODE: pre-Phase-2 weak shims)
endif

INCLUDES := -I$(SIMULATOR_DIR)/csrc \
            -I$(ROOT_DIR)/lib/scxtest \
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
SHARED_SRCS := $(SIMULATOR_DIR)/csrc/sim_bpf_stubs.c \
               $(SIMULATOR_DIR)/csrc/sim_sigfpe.c \
               $(ROOT_DIR)/lib/scxtest/overrides.c

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

.PHONY: all clean e9 e9-rip-trampoline $(SCHEDS)

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
$(BUILD_DIR)/$(1)_sim_bpf_stubs.o: $(SIMULATOR_DIR)/csrc/sim_bpf_stubs.c | $(BUILD_DIR)
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
$(BUILD_DIR)/$(1)_sim_sigfpe.o: $(SIMULATOR_DIR)/csrc/sim_sigfpe.c | $(BUILD_DIR)
	$(CC) $(CFLAGS_BASE) \
		-MMD -MF $(BUILD_DIR)/$(1)_sim_sigfpe.d \
		-c -o $$@ $$<
endef

# Per-scheduler overrides.c -> <name>_overrides.o
define OVERRIDES_RULE
$(BUILD_DIR)/$(1)_overrides.o: $(ROOT_DIR)/lib/scxtest/overrides.c | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(INCLUDES) $(EXTRA_INCLUDES_$(1)) $(EXTRA_CFLAGS_$(1)) \
		-MMD -MF $(BUILD_DIR)/$(1)_overrides.d \
		-c -o $$@ $$<
endef

# Per-scheduler sim_rbc_trampoline.c -> <name>_sim_rbc_trampoline.o
# NOTE: compiled WITHOUT $(INCLUDES) to avoid vmlinux.h conflicts.
# NOTE: compiled WITHOUT coverage flags — the trampoline is called on every
# instrumented Jcc and must have minimal overhead.
define RBC_TRAMPOLINE_RULE
$(BUILD_DIR)/$(1)_sim_rbc_trampoline.o: $(SIMULATOR_DIR)/csrc/sim_rbc_trampoline.c | $(BUILD_DIR)
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
$(BUILD_DIR)/$(1)_sim_deterministic_mem.o: $(SIMULATOR_DIR)/csrc/sim_deterministic_mem.c | $(BUILD_DIR)
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
E9_TRAMPOLINE_SRC := $(SIMULATOR_DIR)/csrc/e9_rbc_trampoline.c
E9_TRAMPOLINE_BIN := $(BUILD_DIR)/e9_rbc_trampoline
E9_RIP_TRAMPOLINE_SRC := $(SIMULATOR_DIR)/csrc/e9_rip_trampoline.c
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

# Include generated dependency files for incremental rebuilds
-include $(wildcard $(BUILD_DIR)/*.d)

# Catch-all: stale .d files may reference headers that have moved or been
# deleted (e.g. after upstream sync). Without this rule, Make errors out with
# "No rule to make target". The catch-all silently ignores the stale dep;
# the next real compilation regenerates the .d file with correct paths.
%.h:
	@:
