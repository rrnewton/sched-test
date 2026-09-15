# Per-scheduler config for layered
# Strip const qualifiers: BPF "const volatile" globals need to be writable
EXTRA_CFLAGS_layered := -Dconst=
# scx_layered BPF source (for #include "intf.h", "timer.bpf.c", "util.bpf.c",
# "main.bpf.c")
LAYERED_BPF_DIR := $(ROOT_DIR)/scheds/rust/scx_layered/src/bpf
EXTRA_INCLUDES_layered := -I$(LAYERED_BPF_DIR) -Ilayered
