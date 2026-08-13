#!/usr/bin/env bash
# Print the scx#3618 patch state of every built libscx_lavd.so, from the
# ARTIFACT rather than from the source tree or anyone's memory.
#
# Discriminator is `scx_cgroup_bw_pressure`, a symbol the patch introduces.
# Do NOT grep for "pressure": clean lavd already contains hw_pressure,
# enter_memory_pressure and leave_memory_pressure, so a bare "pressure" count
# is 7 either way and reads as evidence when it is not.
set -uo pipefail
cd "$(dirname "$0")/.."
echo "scx source:  $(git -C ../scx rev-parse --short HEAD)  $(git -C ../scx status --porcelain | wc -l) modified"
echo "gitlink:     $(git -C .. ls-tree HEAD scx 2>/dev/null | awk '{print substr($3,1,8)}')"
for so in $(ls target/*/build/scx_simulator-*/out/schedulers/libscx_lavd.so 2>/dev/null); do
  m=$(strings "$so" | grep -c 'scx_cgroup_bw_pressure')
  printf '%-9s %s  %s\n' "$(echo "$so" | cut -d/ -f2)" "$(stat -c %y "$so" | cut -c1-19)" \
    "$([ "$m" -gt 0 ] && echo 'PATCHED (scx#3618)' || echo 'unpatched')"
done
