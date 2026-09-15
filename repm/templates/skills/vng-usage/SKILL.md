---
name: vng-usage
description: Run experiments inside virtme-ng VMs with custom kernels
---

# virtme-ng (vng) Usage

## Prerequisites

- `vng` installed and on PATH
- QEMU/KVM available (`/dev/kvm` exists)
- Kernel bzImage at known path (or use host kernel)

## Running a VM

```bash
# Boot VM with host kernel
vng --run -- /bin/bash

# Boot VM with custom kernel
vng --run --kernel /path/to/bzImage -- /bin/bash

# Boot with specific CPU count
vng --run --cpus 12 -- /bin/bash
```

## Running experiments in VM

```bash
# Inside the VM:
cd /path/to/workspace
repm run --mode rtapp-vm --reps 3
```

## Key constraints

- VM mode supports ALL three schedulers (EEVDF, LAVD-PreIRQ, LAVD-PostIRQ)
- CPU pinning inside VM maps to virtual CPUs, not host CPUs
- Performance overhead from virtualization is consistent across schedulers
  (valid for relative comparisons, not absolute numbers)
- sched_ext module must be loaded in the VM kernel for LAVD schedulers

## Troubleshooting

- **"KVM not available"**: Check `/dev/kvm` permissions, ensure
  `kvm-intel` or `kvm-amd` module is loaded
- **"sched_ext not found"**: Kernel must be built with `CONFIG_SCHED_CLASS_EXT=y`
- **VM hangs on boot**: Try `vng --run --verbose` for boot logs
