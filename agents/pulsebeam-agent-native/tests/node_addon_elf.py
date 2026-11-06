"""Audit the pinned Node N-API addon against the declared GNU baseline."""

import hashlib
import json
from pathlib import Path
import re
import sys

from elftools.elf.elffile import ELFFile


manifest = Path(sys.argv[1]).absolute()
modules = manifest.parent / "node_modules" / "@ubjs"
node_runtime = (modules / "node").resolve(strict=True)
node_package = json.loads((node_runtime / "package.json").read_text())
addon_name = "@ubjs/node-linux-x64-gnu"
version = node_package["optionalDependencies"][addon_name]
assert version == node_package["version"] == "0.31.0-5", "Node addon differs from the inventoried runtime version"
addon = node_runtime.parent / "node-linux-x64-gnu"
addon_package = json.loads((addon / "package.json").read_text())
assert addon_package["name"] == addon_name and addon_package["version"] == version
binaries = list(addon.glob("*.node"))
assert len(binaries) == 1, f"expected exactly one pinned Node addon: {binaries}"
assert hashlib.sha256(binaries[0].read_bytes()).hexdigest() == (
    "7cef651b6ab0b45ca7992bfb1a236d956f053d9f95a186f04ec763769c22a0c4"
), "Node addon differs from the independently compared upstream CI artifact"

with binaries[0].open("rb") as source:
    elf = ELFFile(source)
    assert elf["e_machine"] == "EM_X86_64", "Node addon is not x86_64"
    dynamic = elf.get_section_by_name(".dynamic")
    assert dynamic is not None, "Node addon lacks a dynamic section"
    tags = list(dynamic.iter_tags())
    needed = {tag.needed for tag in tags if tag.entry.d_tag == "DT_NEEDED"}
    allowed = {"libc.so.6", "libm.so.6", "libgcc_s.so.1", "libpthread.so.0", "libdl.so.2", "librt.so.1", "ld-linux-x86-64.so.2"}
    assert needed and not needed - allowed, f"undeclared Node addon dependencies: {sorted(needed - allowed)}"
    assert not any(tag.entry.d_tag in ("DT_RPATH", "DT_RUNPATH") for tag in tags), "Node addon has a runtime search path"
    versions = elf.get_section_by_name(".gnu.version_r")
    assert versions is not None, "Node addon lacks GNU version requirements"
    for _, entries in versions.iter_versions():
        for entry in entries:
            match = re.fullmatch(r"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?", entry.name)
            assert match, f"unsupported addon symbol version: {entry.name}"
            actual = tuple(int(part or 0) for part in match.groups())
            assert actual <= (2, 39, 0), f"Node addon requires {entry.name}, newer than glibc 2.39"
