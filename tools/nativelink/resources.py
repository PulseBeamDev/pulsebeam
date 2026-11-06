"""Size the shared NativeLink scheduler and its client dispatch window."""

import os
from pathlib import Path


def machine_budget():
    cpu = len(os.sched_getaffinity(0))
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            # Local Bazel analysis, agents and the coordinator share this host.
            return cpu, max(1, int(line.split()[1]) * 3 // 4)
    raise ValueError("cannot determine host memory from /proc/meminfo")


def cache_budget(state):
    volume = os.statvfs(state)
    capacity = volume.f_blocks * volume.f_frsize
    if capacity <= 0:
        raise ValueError("cannot determine cache filesystem capacity")
    return max(1, capacity // 10)


def client_jobs(cpu):
    # Keep enough requests queued to hide cache and scheduler latency.
    if cpu <= 0:
        raise ValueError("CPU capacity must be positive")
    return cpu * 2
