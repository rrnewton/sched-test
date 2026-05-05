---
title: RIP offset corruption in preemption trace for non-.so addresses
status: closed
priority: 0
issue_type: bug
created_at: 2026-03-18T14:21:09.241848646+00:00
updated_at: 2026-03-18T14:27:37.584624130+00:00
closed_at: 2026-03-18T14:27:37.584624030+00:00
---

# Description

Trace serialization assumes all preemption RIPs are in the scheduler .so. PMU preemptions at timeslice_min=1 frequently land in the main binary or libc. The serializer stores raw absolute addresses as rip_offset, and the deserializer adds so_base, producing garbage addresses via overflow. HW breakpoints at garbage addresses never fire, so replay produces zero preemptions. Fix: omit rip_offset for non-.so RIPs in trace.rs serialization.
