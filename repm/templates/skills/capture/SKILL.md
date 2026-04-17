---
name: capture
description: Capture scheduling traces locally or from remote hosts
---

# Trace Capture

## Local capture

```bash
# Capture with a specific scheduler
repm capture --scheduler lavd_irq --duration 30
```

### Prerequisites (local)

- Root access (sudo) for scheduler management and tracing
- sched_ext kernel module loaded (for LAVD schedulers)

### What gets captured

- Scheduling trace data
- `provenance.json` with host/binary/git metadata

## Remote capture

```bash
# SCP repm to remote, capture, copy results back
repm capture --ssh root@prod-host --duration 300
```

### Prerequisites (remote)

- SSH access to target host (key-based auth preferred)
- Root access on target for scheduler + tracing
- sched_ext support in target kernel

### Important

- **The capture pipeline needs `--scheduler-cmd`** -- monitor mode is
  a stats client, not a standalone capture mode.
- Remote captures use `scp` to transfer the `repm` binary -- no
  pre-installation required on the target.
- Trace files can be large (100MB+ for long captures). Verify disk
  space on both local and remote hosts.

## Post-capture

```bash
# Traces land in traces/ directory
ls traces/

# Analyze a trace
repm analyze --experiment traces/
```

## Provenance for captures

Every capture writes metadata:
- Hostname, kernel version, CPU model
- Scheduler binary path + mtime + size
- Capture duration and timestamp
- Git revision of workspace

This metadata is required for trace-to-experiment linking.
