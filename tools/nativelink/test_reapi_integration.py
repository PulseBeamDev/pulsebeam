#!/usr/bin/env python3
"""Focused real-REAPI deduplication proof; requires Linux and the pinned tools.

Pass --server, --binary, --host-bin-file and --siso (or set the corresponding
REAPI_TEST_SERVER, REAPI_TEST_BINARY, REAPI_TEST_HOST_BIN_FILE, REAPI_TEST_SISO
environment variables). Include server.py, config.py, resources.py and
analyze.py together in the runfiles. The host-bin file must contain the
absolute pinned host-tools directory. No existing service or checkout is used.

The test needs unprivileged bubblewrap support and /proc. It creates one tiny
remote action per client, with an eight-second delay to make overlap observable.
On failure all artifacts are retained under TEST_UNDECLARED_OUTPUTS_DIR if set,
otherwise in the system temporary directory. Success removes the artifacts.
"""

import argparse
from datetime import datetime, timedelta
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


STARTUP_TIMEOUT = 30
CLIENT_TIMEOUT = 75
STOP_TIMEOUT = 5
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("server", "binary", "host-bin-file", "siso"):
        env = "REAPI_TEST_" + name.upper().replace("-", "_")
        parser.add_argument("--" + name, default=os.environ.get(env),
                            required=not os.environ.get(env), type=Path)
    args = parser.parse_args()
    for name in ("server", "binary", "host_bin_file", "siso"):
        path = getattr(args, name).resolve(strict=True)
        if not path.is_file():
            parser.error(f"{name} is not a file: {path}")
        setattr(args, name, path)
    return args


def ports():
    # Hold both reservations simultaneously, then release just before launch.
    # The server cannot inherit listeners, so ownership is checked before use.
    reservations = []
    try:
        while len(reservations) < 2:
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            if sock.getsockname()[1] in (50051, 50061):
                sock.close()
                continue
            reservations.append(sock)
        return [sock.getsockname()[1] for sock in reservations]
    finally:
        for sock in reservations:
            sock.close()


def owned_listeners(pid):
    """Only accept listeners owned by the exec'd server, never another service."""
    inodes = set()
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        try:
            target = os.readlink(fd)
        except FileNotFoundError:
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            inodes.add(target[8:-1])
    result = set()
    for line in Path(f"/proc/{pid}/net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        if fields[3] == "0A" and fields[9] in inodes:
            address, port = fields[1].split(":")
            if address == "0100007F":
                result.add(int(port, 16))
    return result


def wait_ready(server, selected):
    deadline = time.monotonic() + STARTUP_TIMEOUT
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"NativeLink exited during startup: {server.returncode}")
        try:
            if set(selected) <= owned_listeners(server.pid):
                return
        except FileNotFoundError:
            pass
        time.sleep(0.1)
    raise TimeoutError("NativeLink did not acquire both loopback listeners")


def stop_group(process):
    if process is None:
        return
    # Each launched process has its own session. Also kill descendants when the
    # session leader has already exited, as clients may have spawned helpers.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=STOP_TIMEOUT)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=STOP_TIMEOUT)


def prepare_client(root, name, host, siso, reapi_port):
    workspace = root / name
    out = workspace / "out" / "Default"
    config = out / "build/config/siso/main.star"
    config.parent.mkdir(parents=True)
    config.write_text(
        'load("@builtin//struct.star", "module")\n'
        'load("@builtin//encoding.star", "json")\n\n'
        'def init(ctx):\n'
        '    step_config = {"platforms": {"default": {"OSFamily": "Linux", '
        '"pulsebeam-host-tools": ' + json.dumps(str(host)) + '}}, '
        '"rules": [{"name": "shared", "action": "cc", "remote": True, '
        '"use_system_input": True}]}\n'
        '    return module("config", step_config = json.encode(step_config), '
        'handlers = {}, filegroups = {})\n'
    )
    (out / "build.ninja").write_text(
        f"host = {host}\nrule cc\n"
        '  command = /bin/sh -c "${host}/sleep 8; printf shared > result.txt"\n'
        "  description = CXX result.txt\n"
        "build result.txt: cc\ndefault result.txt\n"
    )
    (out / "gn_logs.txt").write_text("use_remoteexec=true\nuse_reclient=false\n")
    (out / "build.ninja.d").write_text("build.ninja:\n")
    env = {"PATH": str(host), "HOME": str(workspace), "TMPDIR": str(root),
           "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "SISO_TELEMETRY": "0",
           "RBE_service_no_security": "true"}
    command = [str(siso), "ninja", "-C", str(out),
               f"-reapi_address=127.0.0.1:{reapi_port}", "-reapi_instance=",
               "-reapi_insecure", "-strict_remote", "-local_jobs=1",
               "-remote_jobs=2", "result.txt"]
    return workspace, out, env, command


def client_evidence(info):
    digests = re.findall(r"\] ([0-9a-f-]{36}) digest: ([0-9a-f]{64}/\d+), skipCacheLookup:(\w+) opName: (\S+)", info)
    if len(digests) != 1 or digests[0][2] != "false":
        raise AssertionError(f"expected one cacheable remote action: {digests}")
    request, digest, _, operation = digests[0]
    # Siso logs this stage only after receiving a non-completed operation from
    # the server. A late completed-cache hit cannot satisfy this assertion.
    if not re.search(r"exec\.go:\d+\] " + re.escape(request) + r" operation stage: \w+ -> EXECUTING\b", info):
        raise AssertionError(f"request {request} never observed an executing operation")
    events = {}
    for kind in ("starts", "done"):
        matches = re.findall(
            r"^\s*I(\d{4}) (\d\d:\d\d:\d\d\.\d+)\s+\d+ "
            r"exec\.go:\d+\].*?operation " + kind + r": (\S+)", info, re.M)
        if len(matches) != 1 or matches[0][2] != operation:
            raise AssertionError(f"expected one matching operation {kind}: {matches}")
        events[kind] = datetime.strptime("2000" + matches[0][0] + " " + matches[0][1],
                                          "%Y%m%d %H:%M:%S.%f")
    return digest, events


def check_evidence(infos, server_log, analyze):
    clients = [client_evidence(info) for info in infos]
    if clients[0][0] != clients[1][0]:
        raise AssertionError(f"different action digests: {clients}")
    times = [events[kind] for _, events in clients for kind in ("starts", "done")]
    # Siso's glog timestamps omit the year. Handle the New Year boundary.
    if max(times) - min(times) > timedelta(days=180):
        times = [t + timedelta(days=366) if t.month == 1 else t for t in times]
    a_start, a_done, b_start, b_done = times
    if not (a_start < a_done and b_start < b_done and max(a_start, b_start) < min(a_done, b_done)):
        raise AssertionError(f"client operations did not overlap: {times}")
    summary = analyze(ANSI.sub("", server_log))
    diagnostics = summary["diagnostics"]
    if (summary["starts"] != 1 or summary["completions"] != 1
            or summary["nonzero_exits"] != 0 or summary["incomplete_operation_ids"]
            or summary["unmatched_completion_operation_ids"] or any(diagnostics.values())):
        raise AssertionError(f"expected exactly one successful worker execution: {summary}")
    return clients[0][0]


def run(args, root):
    host = Path(args.host_bin_file.read_text().strip())
    if not host.is_absolute() or not host.is_dir() or not re.fullmatch(r"[a-zA-Z0-9_./+\-]+", str(host)):
        raise ValueError(f"host-bin must name an absolute, shell-safe directory: {host}")
    # analyze.py is the maintained parser for the pinned NativeLink log format.
    sys.path.insert(0, str(args.server.parent))
    from analyze import analyze

    state = root / "state"
    state.mkdir(mode=0o700)
    reapi_port, worker_port = ports()
    env = {"PATH": str(host), "HOME": str(root), "TMPDIR": str(root),
           "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "RUST_LOG": "info"}
    server_command = [sys.executable, str(args.server), "--binary", str(args.binary),
                      "--host-bin-file", str(args.host_bin_file), "--state", str(state),
                      "--cpu", "2", "--memory-kb", str(2 * 1024**2),
                      "--reapi-port", str(reapi_port), "--worker-port", str(worker_port)]
    clients = [prepare_client(root, name, host, args.siso, reapi_port) for name in ("a", "b")]
    processes = []
    server = None
    try:
        with (root / "server.log").open("w") as log:
            server = subprocess.Popen(server_command, cwd=root, env=env, stdout=log,
                                      stderr=subprocess.STDOUT, start_new_session=True)
            wait_ready(server, (reapi_port, worker_port))
            for name, (workspace, _, client_env, command) in zip(("a", "b"), clients):
                with (root / f"{name}.log").open("w") as client_log:
                    processes.append(subprocess.Popen(command, cwd=workspace, env=client_env,
                                                      stdout=client_log, stderr=subprocess.STDOUT,
                                                      start_new_session=True))
            deadline = time.monotonic() + CLIENT_TIMEOUT
            for process in processes:
                process.wait(timeout=max(0, deadline - time.monotonic()))
            if any(process.returncode != 0 for process in processes):
                raise RuntimeError(f"Siso exit codes: {[p.returncode for p in processes]}")
            if server.poll() is not None:
                raise RuntimeError(f"NativeLink exited: {server.returncode}")
            outputs = [(out / "result.txt").read_bytes() for _, out, _, _ in clients]
            if outputs != [b"shared", b"shared"]:
                raise AssertionError(f"unexpected output: {outputs}")
            infos = [(out / "siso.INFO").read_text() for _, out, _, _ in clients]
            digest = check_evidence(infos, (root / "server.log").read_text(), analyze)

            workspace, out, client_env, command = prepare_client(root, "cached", host, args.siso, reapi_port)
            with (root / "cached.log").open("w") as client_log:
                cached = subprocess.Popen(command, cwd=workspace, env=client_env,
                                          stdout=client_log, stderr=subprocess.STDOUT,
                                          start_new_session=True)
                processes.append(cached)
                cached.wait(timeout=CLIENT_TIMEOUT)
            if cached.returncode or (out / "result.txt").read_bytes() != b"shared":
                raise AssertionError(f"cached client failed: exit={cached.returncode}")
            info = (out / "siso.INFO").read_text()
            cached_digests = re.findall(r"cmd\.go:\d+\] [0-9a-f-]{36} action: ([0-9a-f]{64}/\d+)", info)
            if cached_digests != [digest] or not re.search(r"exec-remote-cache: n=1\b", info):
                raise AssertionError(f"expected a completed cache hit for {digest}: {cached_digests}")
            summary = analyze(ANSI.sub("", (root / "server.log").read_text()))
            if summary["starts"] != 1 or summary["completions"] != 1 or summary["nonzero_exits"]:
                raise AssertionError(f"cache hit unexpectedly executed work: {summary}")
            print(f"PASS: two executing Siso requests share digest {digest}; one execution and a subsequent cache hit")
    finally:
        for process in processes:
            stop_group(process)
        stop_group(server)


def interrupted(signum, _frame):
    raise RuntimeError(f"interrupted by signal {signum}")


def main():
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    args = arguments()
    artifact_parent = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR") or None
    root = Path(tempfile.mkdtemp(prefix="reapi-integration-", dir=artifact_parent)).resolve()
    try:
        run(args, root)
    except BaseException:
        print(f"REAPI integration artifacts retained at {root}", file=sys.stderr, flush=True)
        raise
    else:
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
