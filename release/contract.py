"""Exercise installation and the shipped updater against local release fixtures."""

import argparse
from functools import partial
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
from threading import Thread


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--local", required=True)
    parser.add_argument("--global", dest="global_assets", required=True)
    args = parser.parse_args()
    local = Path(args.local).resolve()
    global_assets = Path(args.global_assets).resolve()
    arch = {"x86_64": "x86_64", "aarch64": "aarch64"}[platform.machine()]
    stem = f"pulsebeam-{arch}-unknown-linux-gnu"
    installer = global_assets / "pulsebeam-installer.sh"
    for checksum in [*local.glob("*.sha256"), *global_assets.glob("*.sha256")]:
        expected, name = checksum.read_text().split()
        assert hashlib.sha256((checksum.parent / name).read_bytes()).hexdigest() == expected, checksum
    with tarfile.open(local / (stem + ".tar.xz")) as bundle:
        assert set(bundle.getnames()) == {stem, f"{stem}/pulsebeam", f"{stem}/LICENSE", f"{stem}/README.md"}
        assert bundle.getmember(f"{stem}/pulsebeam").mode == 0o755
    with tempfile.TemporaryDirectory(prefix="pulsebeam-distribution-") as temp:
        root = Path(temp)
        upgraded = root / (stem + ".tar.xz")
        replacement = b"#!/bin/sh\nprintf 'controlled fixture upgrade\\n'\n"
        with tarfile.open(upgraded, "w:xz") as bundle:
            info = tarfile.TarInfo(stem + "/pulsebeam")
            info.mode = 0o755
            info.size = len(replacement)
            bundle.addfile(info, io.BytesIO(replacement))
        (root / (upgraded.name + ".sha256")).write_text(
            f"{hashlib.sha256(upgraded.read_bytes()).hexdigest()}  {upgraded.name}\n"
        )
        for name in (stem + "-update", stem + "-update.sha256"):
            (root / name).write_bytes((local / name).read_bytes())
        fixture_installer = root / installer.name
        requests = []
        release = {}

        class Handler(SimpleHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                if "/repos/PulseBeamDev/pulsebeam/releases" in self.path:
                    payload = [release] if self.path.rstrip("/").endswith("/releases") else release
                    data = json.dumps(payload).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                elif self.path.startswith("/install/"):
                    self.path = self.path.removeprefix("/install")
                    original = self.directory
                    self.directory = str(local)
                    super().do_GET()
                    self.directory = original
                else:
                    super().do_GET()

        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(root)))
        base = f"http://127.0.0.1:{server.server_port}"
        release.update({
            "id": 1, "tag_name": "", "name": "Controlled fixture",
            "draft": False, "prerelease": False, "body": "Non-production updater fixture",
            "url": base + "/repos/PulseBeamDev/pulsebeam/releases/1", "html_url": base + "/release",
            "assets": [],
        })
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            env = {key: value for key, value in os.environ.items() if key not in {"GH_TOKEN", "GITHUB_TOKEN", "CARGO_DIST_FORCE_INSTALL_DIR"}}
            env.update({"HOME": str(root / "home"), "CARGO_HOME": str(root / "cargo"),
                        "XDG_CONFIG_HOME": str(root / "config"),
                        "PULSEBEAM_DOWNLOAD_URL": base + "/install/" + stem + ".tar.xz",
                        "PULSEBEAM_INSTALLER_GHE_BASE_URL": base,
                        "HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                        "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "127.0.0.1,localhost"})
            Path(env["HOME"]).mkdir()
            subprocess.run(["sh", str(installer), "--no-modify-path"], env=env, check=True)
            binary = Path(env["CARGO_HOME"]) / "bin/pulsebeam"
            updater = binary.with_name("pulsebeam-update")
            subprocess.run([str(binary), "--version"], env=env, check=True)
            assert updater.read_bytes() == (local / (stem + "-update")).read_bytes()
            receipt_file = Path(env["XDG_CONFIG_HOME"]) / "pulsebeam/pulsebeam-receipt.json"
            receipt = json.loads(receipt_file.read_text())
            assert receipt["source"] == {"app_name": "pulsebeam", "name": "pulsebeam", "owner": "PulseBeamDev", "release_type": "github"}
            assert receipt["install_prefix"] == str(binary.parent)
            assert receipt["install_layout"] == "flat" and receipt["binaries"] == ["pulsebeam"]
            assert receipt["modify_path"] is False
            assert receipt["provider"] == {"source": "cargo-dist", "version": "0.30.3"}
            assert not (Path(env["CARGO_HOME"]) / "env").exists()
            major, minor, patch = receipt["version"].split("-", 1)[0].split("+", 1)[0].split(".")
            new_version = f"{major}.{minor}.{int(patch) + 1}"
            release["tag_name"] = "pulsebeam-v" + new_version
            # The real updater invokes a release installer, not the tarball directly.
            script = installer.read_text()
            old_version = f"VERSION='{receipt['version']}'"
            old_tag = f"TAG='pulsebeam-v{receipt['version']}'"
            assert old_version in script and old_tag in script
            fixture_installer.write_text(script.replace(old_version, f"VERSION='{new_version}'")
                                         .replace(old_tag, f"TAG='{release['tag_name']}'"))
            assets = [upgraded, fixture_installer, root / (stem + "-update")]
            release["assets"] = [
                {"id": index, "name": asset.name, "size": asset.stat().st_size,
                 "browser_download_url": base + "/" + asset.name,
                 "url": base + "/" + asset.name, "content_type": "application/octet-stream",
                 "state": "uploaded", "download_count": 0,
                 "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}
                for index, asset in enumerate(assets, 2)
            ]
            env["PULSEBEAM_DOWNLOAD_URL"] = base + "/" + upgraded.name
            subprocess.run([str(updater)], env=env, check=True, timeout=90)
            updated_receipt = json.loads(receipt_file.read_text())
            assert updated_receipt["version"] == new_version
            assert updated_receipt["install_prefix"] == str(binary.parent)
            assert updated_receipt["modify_path"] is False
            assert not (Path(env["CARGO_HOME"]) / "env").exists()
            assert binary.read_bytes() == replacement, "shipped updater did not install the controlled archive"
            assert any("/repos/PulseBeamDev/pulsebeam/releases" in path for path in requests), requests
            assert any(path == "/" + upgraded.name for path in requests), requests
            print("Archive/hash/installer/receipt and standalone updater contracts passed against local fixtures.")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
