"""Size the shared NativeLink scheduler and its client dispatch window."""

import os
from pathlib import Path


def machine_budget():
    cpu = len(os.sched_getaffinity(0))
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            return cpu, int(line.split()[1])
    raise ValueError("cannot determine host memory from /proc/meminfo")


def client_jobs(cpu):
    # Keep enough requests queued to hide cache and scheduler latency.
    return max(8, min(128, cpu * 2))
