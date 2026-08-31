#!/usr/bin/env python3

import hashlib
import json
import platform
import sys
import urllib.request
import zipfile
from pathlib import Path


def fail(message: str) -> None:
    raise SystemExit(message)


def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: install-firefox-openh264.py <playwright-browser-directory>")
    root = Path(sys.argv[1]).resolve()
    archives = sorted(root.glob("firefox-*/firefox/omni.ja"))
    if len(archives) != 1:
        fail(f"expected one Playwright Firefox archive under {root}, found {len(archives)}")
    with zipfile.ZipFile(archives[0]) as archive:
        metadata = json.loads(
            archive.read("chrome/toolkit/content/global/gmp-sources/openh264.json")
        )
    machine = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(platform.machine())
    if machine is None:
        fail(f"unsupported Firefox OpenH264 architecture: {platform.machine()}")
    vendor = metadata["vendors"]["gmp-gmpopenh264"]
    details = vendor["platforms"][f"Linux_{machine}-gcc3"]
    expected_hash = details["hashValue"]
    cache = root / "openh264-cache"
    cache.mkdir(parents=True, exist_ok=True)
    package = cache / f"{expected_hash}.zip"
    if not package.exists():
        urllib.request.urlretrieve(details["fileUrl"], package)
    contents = package.read_bytes()
    if len(contents) != details["filesize"]:
        fail(f"Firefox OpenH264 size mismatch: {len(contents)} != {details['filesize']}")
    actual_hash = hashlib.sha512(contents).hexdigest()
    if actual_hash != expected_hash:
        fail(f"Firefox OpenH264 SHA-512 mismatch: {actual_hash} != {expected_hash}")
    destination = root / "gmp-gmpopenh264" / vendor["version"]
    destination.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(package) as archive:
        names = set(archive.namelist())
        expected = {"gmpopenh264.info", "libgmpopenh264.so"}
        if names != expected:
            fail(f"unexpected Firefox OpenH264 archive contents: {sorted(names)}")
        archive.extractall(destination)
    print(destination)


if __name__ == "__main__":
    main()
