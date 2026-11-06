"""Size the shared NativeLink scheduler and its client dispatch window."""

import os
from pathlib import Path, PurePosixPath
import re


_PROC_CGROUP = Path("/proc/self/cgroup")
_PROC_MOUNTINFO = Path("/proc/self/mountinfo")
_PROC_MEMINFO = Path("/proc/meminfo")


def _absolute_path(value):
    path = PurePosixPath(value)
    if not value.startswith("/") or ".." in path.parts or "\\" in value:
        raise ValueError(f"invalid cgroup path: {value!r}")
    return path


def _mount_path(value):
    # mountinfo escapes space, tab, newline and backslash as octal sequences.
    value = re.sub(r"\\([0-7]{3})", lambda m: chr(int(m.group(1), 8)), value)
    return _absolute_path(value)


def _cgroup_mounts():
    membership = None
    for line in _PROC_CGROUP.read_text().splitlines():
        parts = line.split(":", 2)
        if len(parts) != 3:
            raise ValueError(f"invalid cgroup membership: {line!r}")
        if parts[0] == "0" and parts[1] == "":
            if membership is not None:
                raise ValueError("multiple cgroup v2 memberships")
            membership = _absolute_path(parts[2])
    if membership is None:
        return []

    mounts = []
    for line in _PROC_MOUNTINFO.read_text().splitlines():
        before, separator, after = line.partition(" - ")
        fields, filesystem = before.split(), after.split()
        if not separator or not filesystem or len(fields) < 6:
            raise ValueError(f"invalid mountinfo entry: {line!r}")
        if filesystem[0] != "cgroup2":
            continue
        root, mountpoint = _mount_path(fields[3]), _mount_path(fields[4])
        try:
            relative = membership.relative_to(root)
        except ValueError:
            continue
        mounts.append((Path(str(mountpoint)), root, relative))
    if not mounts:
        raise ValueError(f"cannot locate cgroup v2 membership {membership} in mountinfo")
    return mounts


def _limit_text(directory, name, root_without_parent):
    path = directory / name
    try:
        return path.read_text().strip()
    except FileNotFoundError:
        # A controller not enabled by the parent has no interface here; its
        # ancestors may still impose limits and are checked separately.
        try:
            controllers = (directory / "cgroup.controllers").read_text().split()
        except OSError as error:
            raise ValueError(f"cannot read cgroup directory {directory}: {error}") from error
        controller = name.split(".", 1)[0]
        if root_without_parent or controller not in controllers:
            return "max" if name == "memory.max" else "max 100000"
        raise ValueError(f"missing cgroup limit: {path}") from None
    except OSError as error:
        raise ValueError(f"cannot read cgroup limit: {path}: {error}") from error


def _positive_integer(value, path):
    if not re.fullmatch(r"[0-9]+", value) or int(value) == 0:
        raise ValueError(f"invalid cgroup limit in {path}: {value!r}")
    return int(value)


def _cgroup_budget(cpu, memory_kb):
    for mountpoint, root, relative in _cgroup_mounts():
        directory = mountpoint / str(relative)
        while True:
            root_without_parent = directory == mountpoint and root == PurePosixPath("/")
            cpu_path = directory / "cpu.max"
            cpu_fields = _limit_text(directory, "cpu.max", root_without_parent).split()
            if len(cpu_fields) != 2:
                raise ValueError(f"invalid cgroup limit in {cpu_path}")
            period = _positive_integer(cpu_fields[1], cpu_path)
            if cpu_fields[0] != "max":
                quota = _positive_integer(cpu_fields[0], cpu_path)
                cpu = min(cpu, max(1, quota // period))

            memory_path = directory / "memory.max"
            memory = _limit_text(directory, "memory.max", root_without_parent)
            if memory != "max":
                if not re.fullmatch(r"[0-9]+", memory):
                    raise ValueError(f"invalid cgroup limit in {memory_path}: {memory!r}")
                limit_kb = int(memory) // 1024
                if limit_kb == 0:
                    raise ValueError(f"cgroup memory limit is less than 1 KiB: {memory_path}")
                memory_kb = min(memory_kb, limit_kb)
            if directory == mountpoint:
                break
            directory = directory.parent
    return cpu, memory_kb


def machine_budget():
    cpu = len(os.sched_getaffinity(0))
    for line in _PROC_MEMINFO.read_text().splitlines():
        if line.startswith("MemTotal:"):
            return _cgroup_budget(cpu, int(line.split()[1]))
    raise ValueError("cannot determine host memory from /proc/meminfo")


def client_jobs(cpu):
    # Keep enough requests queued to hide cache and scheduler latency.
    return max(8, min(128, cpu * 2))
