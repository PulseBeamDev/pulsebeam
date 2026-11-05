#!/usr/bin/env python3
"""Start or reuse the per-user NativeLink service, without invoking the launcher."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import tempfile
import time

from resources import client_jobs

PORTS = (50051, 50061)
TIMEOUT = 30.0
INPUTS = (
    "tools/nativelink/server.py",
    "tools/nativelink/config.py",
    "tools/nativelink/resources.py",
    "crates/pulsebeam-libwebrtc-sys/gn/host.nix",
    "tools/host/flake.lock",
)


class StartupError(Exception):
    pass


def identity(workspace):
    digest = hashlib.sha256(b"pulsebeam-nativelink-autostart-v1\0")
    for name in INPUTS:
        data = (workspace / name).read_bytes()
        digest.update(name.encode() + b"\0" + len(data).to_bytes(8, "big") + data)
    # Other MODULE changes must not prevent worktrees sharing the service.
    module = (workspace / "MODULE.bazel").read_text()
    pin = re.search(r'(?ms)^http_archive\(\s*name\s*=\s*"nativelink",.*?^\)', module)
    if pin is None:
        raise StartupError("Cannot locate the NativeLink pin in MODULE.bazel")
    digest.update(pin.group().encode())
    # The scheduler's host capacity is selected once at startup.
    return digest.hexdigest()


def state_directory():
    home = Path.home()
    path = home
    for component in (".cache", "pulsebeam", "nativelink", "v1"):
        path = path / component
        path.mkdir(mode=0o700, exist_ok=True)
        info = path.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o022 or (component == "v1" and info.st_mode & 0o077)):
            raise StartupError(f"Unsafe state directory {path}: must be owned, not a symlink or writable by others; v1 must be private")
    return path


def open_private(path, flags):
    fd = os.open(path, flags | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        os.close(fd)
        raise StartupError(f"Unsafe state file {path}: must be an owned private regular file")
    return fd


def read_marker(state):
    try:
        fd = open_private(state / "autostart.json", os.O_RDONLY)
    except FileNotFoundError:
        return None
    try:
        with os.fdopen(fd) as stream:
            marker = json.load(stream)
        if (not isinstance(marker, dict) or type(marker.get("pid")) is not int
                or marker["pid"] <= 0 or not isinstance(marker.get("start"), str)
                or not isinstance(marker.get("boot"), str) or not isinstance(marker.get("identity"), str)):
            return None
        return marker
    except (ValueError, UnicodeError):
        return None


def write_marker(state, marker):
    # Replace atomically so a crash cannot leave a partially written marker.
    fd, name = tempfile.mkstemp(prefix=".autostart-", dir=state)
    try:
        with os.fdopen(fd, "w") as stream:
            stream.write(json.dumps(marker) + "\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, state / "autostart.json")
    finally:
        if os.path.exists(name):
            os.unlink(name)


def process_start(pid):
    try:
        # comm may contain spaces and parentheses; fields after the last ')' start at 3.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return None if fields[0] == "Z" else fields[19]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return None


def boot_id():
    return Path("/proc/sys/kernel/random/boot_id").read_text().strip()


def alive(marker):
    return marker["boot"] == boot_id() and process_start(marker["pid"]) == marker["start"]


def server_locked(state):
    fd = open_private(state / "server.lock", os.O_RDWR | os.O_CREAT)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(fd, fcntl.LOCK_UN)
        return False
    finally:
        os.close(fd)


def owns_lock(pid, state):
    try:
        target = (state / "server.lock").stat()
        for fd in Path(f"/proc/{pid}/fd").iterdir():
            try:
                info = fd.stat()
                if (info.st_dev, info.st_ino) == (target.st_dev, target.st_ino):
                    return True
            except FileNotFoundError:
                continue
    except FileNotFoundError:
        pass
    return False


def listening_ports(pid):
    """Find loopback listeners owned by this process, not just connectable ports."""
    inodes = set()
    try:
        for fd in Path(f"/proc/{pid}/fd").iterdir():
            try:
                link = os.readlink(fd)
                if link.startswith("socket:[") and link.endswith("]"):
                    inodes.add(link[8:-1])
            except FileNotFoundError:
                continue
    except FileNotFoundError:
        return set()
    ports = set()
    for line in Path("/proc/net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        address, port = fields[1].split(":")
        if address == "0100007F" and fields[3] == "0A" and fields[9] in inodes:
            ports.add(int(port, 16))
    return ports


def ports_free():
    sockets = []
    try:
        for port in PORTS:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sockets.append(sock)
            try:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                sock.bind(("127.0.0.1", port))
            except OSError as error:
                raise StartupError(f"Port 127.0.0.1:{port} is occupied ({error}); stop the other listener and retry") from error
    finally:
        for sock in sockets:
            sock.close()


def wait_ready(state, marker, child=None):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        if not alive(marker):
            if child is not None:
                child.poll()
            raise StartupError(f"NativeLink exited during startup; inspect {state / 'autostart.log'}")
        if (server_locked(state) and owns_lock(marker["pid"], state)
                and set(PORTS) <= listening_ports(marker["pid"])):
            return
        time.sleep(0.1)
    raise StartupError(f"NativeLink did not become ready within {TIMEOUT:g}s; inspect {state / 'autostart.log'} and stop the service before retrying")


def start(workspace, bazelisk, output_base, state):
    fd = open_private(state / "autostart.lock", os.O_RDWR | os.O_CREAT)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        expected = identity(workspace)
        marker = read_marker(state)
        if marker and alive(marker):
            if marker["identity"] != expected:
                raise StartupError(f"Incompatible NativeLink service (pid {marker['pid']}); stop it and retry. State: {state}")
            wait_ready(state, marker)
            return
        if server_locked(state):
            raise StartupError(f"An unrecognized service holds {state / 'server.lock'}; identify it with fuser {state / 'server.lock'}, stop it and retry (it will not be killed)")
        ports_free()
        subprocess.run([str(bazelisk), f"--output_base={output_base}", "build", "--config=local", "//tools/nativelink:server"], cwd=workspace, check=True, stdout=sys.stderr)
        if identity(workspace) != expected:
            raise StartupError("NativeLink inputs changed during bootstrap; retry")
        binary = output_base / "external/+http_archive+nativelink/nativelink"
        host_file = output_base / "external/+webrtc+webrtc_host/host-bin"
        if not binary.is_file() or not host_file.is_file():
            raise StartupError(f"Bazel did not produce NativeLink or host-bin in {output_base}")
        ports_free()
        log = open_private(state / "autostart.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND)
        try:
            child = subprocess.Popen([sys.executable, str(workspace / "tools/nativelink/server.py"),
                                      "--binary", str(binary), "--host-bin-file", str(host_file),
                                      "--state", str(state)], cwd=state, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
        finally:
            os.close(log)
        start_time = process_start(child.pid)
        if start_time is None:
            child.poll()
            raise StartupError(f"NativeLink exited during startup; inspect {state / 'autostart.log'}")
        marker = {"pid": child.pid, "start": start_time, "boot": boot_id(), "identity": expected}
        write_marker(state, marker)
        try:
            wait_ready(state, marker, child)
        except BaseException:
            # This process started the service, so clean up a failed startup.
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            (state / "autostart.json").unlink(missing_ok=True)
            raise
    finally:
        os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--bazelisk", type=Path, required=True)
    parser.add_argument("--output-base", type=Path, required=True)
    parser.add_argument("--print-host-tools", action="store_true")
    args = parser.parse_args()
    if not all(path.is_absolute() for path in (args.workspace, args.bazelisk, args.output_base)):
        parser.error("all paths must be absolute")
    try:
        state = state_directory()
        start(args.workspace, args.bazelisk, args.output_base, state)
        if args.print_host_tools:
            config = json.loads((state / "config.json").read_text())
            properties = config["workers"][0]["local"]["platform_properties"]
            print(properties["pulsebeam-host-tools"]["values"][0])
            print(client_jobs(int(properties["cpu_count"]["values"][0])))
    except (StartupError, OSError, subprocess.CalledProcessError) as error:
        print(f"NativeLink autostart failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
