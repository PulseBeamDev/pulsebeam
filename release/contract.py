"""Exercise versioned installation and updates against local archive fixtures."""

import argparse
from contextlib import ExitStack
from functools import partial
import hashlib
from http.client import HTTPConnection
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tarfile
import tempfile
from threading import Thread
import time


def smoke_production_listeners(binary, root, env):
    config = root / "runtime"
    config.mkdir()
    policy = config / "leap-seconds.list"
    # Isolated startup fixture, not an attestation of the runner's UTC source.
    policy.write_text("#@ 4500000000\n2272060800 10 # 1972 origin\n2287785600 11 # July 1972\n")
    # Public verification material only. The smoke never issues tokens.
    (config / "key-registry.json").write_text(json.dumps({
        "projectId": "p_00000000001R010000000000004",
        "keys": [{
            "keyId": "kid_00000000001R010000000000008",
            "publicKey": "MCowBQYDK2VwAyEA11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
        }],
    }))
    with ExitStack() as sockets:
        def reserve(family, kind, host, port=0):
            listener = sockets.enter_context(socket.socket(family, kind))
            listener.bind((host, port))
            return listener.getsockname()[1]

        http_port, internal_port = [
            reserve(socket.AF_INET, socket.SOCK_STREAM, "127.0.0.1") for _ in range(2)
        ]
        rtc_port = reserve(socket.AF_INET6, socket.SOCK_STREAM, "::")
        reserve(socket.AF_INET6, socket.SOCK_DGRAM, "::", rtc_port)

    def require_status(port, path, expected):
        conn = HTTPConnection("127.0.0.1", port, timeout=2)
        try:
            conn.request("GET", path)
            with conn.getresponse() as response:
                assert response.status == expected, (path, expected, response.status)
        finally:
            conn.close()

    log = config / "server.log"
    with log.open("wb") as output, subprocess.Popen([
        str(binary), "--shards", "1", "--rtc-port", str(rtc_port),
        "--utc-policy", str(policy), "--http-listen", f"127.0.0.1:{http_port}",
        "--internal-listen", f"127.0.0.1:{internal_port}",
    ], cwd=config, env=env, stdout=output, stderr=subprocess.STDOUT) as node:
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    require_status(internal_port, "/healthz", 200)
                    require_status(http_port, "/api/v1/native/admissions", 405)
                    require_status(http_port, "/metrics", 404)
                    require_status(internal_port, "/metrics", 200)
                    break
                except ConnectionError:
                    if node.poll() is not None or time.monotonic() >= deadline:
                        raise
                    time.sleep(0.025)
        except Exception:
            print(log.read_text(errors="replace"))
            raise
        finally:
            if node.poll() is None:
                node.terminate()
                try:
                    node.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    node.kill()
                    node.wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--local", required=True)
    parser.add_argument("--global", dest="global_assets", required=True)
    args = parser.parse_args()
    local = Path(args.local).resolve()
    global_assets = Path(args.global_assets).resolve()
    stem = f"pulsebeam-{platform.machine()}-unknown-linux-gnu"
    installer = global_assets / "pulsebeam-installer.sh"
    for checksum in [*local.glob("*.sha256"), *global_assets.glob("*.sha256")]:
        expected, name = checksum.read_text().split()
        assert hashlib.sha256((checksum.parent / name).read_bytes()).hexdigest() == expected, checksum
    with tarfile.open(local / (stem + ".tar.xz")) as bundle:
        entries = {member.name.removeprefix("./").rstrip("/"): member for member in bundle}
        assert set(entries) - {"", "."} == {stem, f"{stem}/pulsebeam", f"{stem}/LICENSE", f"{stem}/README.md"}
        assert entries[f"{stem}/pulsebeam"].mode == 0o755
        assert all(member.mtime == 0 for member in entries.values())
    assert not any(local.glob("*-update*")), "distribution must not fetch or ship a separate updater"
    with tempfile.TemporaryDirectory(prefix="pulsebeam-distribution-") as temp:
        root = Path(temp)
        upgraded = root / (stem + ".tar.xz")
        replacement = b"#!/bin/sh\nprintf 'controlled fixture upgrade\\n'\n"
        with tarfile.open(upgraded, "w:xz") as bundle:
            info = tarfile.TarInfo(stem + "/pulsebeam")
            info.mode = 0o755
            info.size = len(replacement)
            bundle.addfile(info, io.BytesIO(replacement))
        checksum = root / (upgraded.name + ".sha256")
        checksum.write_text(f"{hashlib.sha256(upgraded.read_bytes()).hexdigest()}  {upgraded.name}\n")
        requests = []

        class Handler(SimpleHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                if self.path.startswith("/install/"):
                    self.path = self.path.removeprefix("/install")
                    original = self.directory
                    self.directory = str(local)
                    try:
                        super().do_GET()
                    finally:
                        self.directory = original
                else:
                    super().do_GET()

        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(root)))
        base = f"http://127.0.0.1:{server.server_port}"
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            env = {key: value for key, value in os.environ.items() if key not in {"GH_TOKEN", "GITHUB_TOKEN"}}
            env.update({"HOME": str(root / "home"), "CARGO_HOME": str(root / "cargo"),
                        "XDG_CONFIG_HOME": str(root / "config"),
                        "PULSEBEAM_DOWNLOAD_URL": base + "/install/" + stem + ".tar.xz",
                        "HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                        "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "127.0.0.1,localhost"})
            Path(env["HOME"]).mkdir()
            command = ["sh", str(installer), "--no-modify-path"]
            subprocess.run(command, env=env, check=True)
            binary = Path(env["CARGO_HOME"]) / "bin/pulsebeam"
            subprocess.run([str(binary), "--version"], env=env, check=True)
            smoke_production_listeners(binary, root, env)
            assert not binary.with_name("pulsebeam-update").exists()
            assert not Path(env["XDG_CONFIG_HOME"]).exists(), "installation must not require a receipt"
            assert not (Path(env["CARGO_HOME"]) / "env").exists()
            env["PULSEBEAM_DOWNLOAD_URL"] = base + "/" + upgraded.name
            subprocess.run(command, env=env, check=True)
            assert binary.read_bytes() == replacement, "installer did not replace the existing installation"
            checksum.write_text(f"{'0' * 64}  {upgraded.name}\n")
            result = subprocess.run(command, env=env, capture_output=True, text=True)
            assert result.returncode != 0 and "checksum mismatch" in result.stderr
            assert binary.read_bytes() == replacement, "failed verification modified the installation"
            assert not (Path(env["CARGO_HOME"]) / "env").exists()
            assert len(requests) == 6, requests
            print("Archive, installation, production listeners and failure-atomicity contracts passed against local fixtures.")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
