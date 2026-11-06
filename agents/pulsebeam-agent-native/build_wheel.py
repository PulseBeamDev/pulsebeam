"""Build and audit the generated native Python distribution."""

import argparse
import os
from pathlib import Path
import shutil
import sys
import tempfile

from auditwheel.main import main as auditwheel_main
from elftools.elf.elffile import ELFFile
from setuptools import build_meta


# Only the declared manylinux system closure may be resolved from the host.
# A new non-system dependency must be supplied as an explicit packaging input.
SYSTEM_LIBRARIES = {"libc.so.6", "libm.so.6", "libgcc_s.so.1", "ld-linux-x86-64.so.2"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("manifest", "bindings", "library", "third-party", "rust-std", "patchelf", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    manifest, bindings, library, third_party, rust_std, patchelf, output = (
        getattr(args, name).absolute()
        for name in ("manifest", "bindings", "library", "third_party", "rust_std", "patchelf", "output")
    )
    with library.open("rb") as stream:
        elf = ELFFile(stream)
        if elf["e_machine"] != "EM_X86_64":
            raise ValueError("native wheel requires an x86_64 library")
        dynamic = elf.get_section_by_name(".dynamic")
        if dynamic is None:
            raise ValueError("native library lacks a dynamic section")
        needed = {tag.needed for tag in dynamic.iter_tags() if tag.entry.d_tag == "DT_NEEDED"}
        if not needed or needed - SYSTEM_LIBRARIES:
            raise ValueError(f"undeclared native library dependencies: {sorted(needed - SYSTEM_LIBRARIES)}")
        if any(tag.entry.d_tag in ("DT_RPATH", "DT_RUNPATH") for tag in dynamic.iter_tags()):
            raise ValueError("native library contains an unqualified runtime search path")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="pulsebeam-agent-wheel-") as temporary:
        root = Path(temporary)
        source = root / "source"
        source.mkdir()
        for name in ("pyproject.toml", "setup.py", "README.md", "LICENSE"):
            shutil.copyfile(manifest.parent / name, source / name)
        shutil.copyfile(third_party, source / "THIRD_PARTY_NOTICES.md")
        shutil.copyfile(rust_std, source / "RUST_STANDARD_LIBRARY_NOTICES.html")
        package = source / "pulsebeam_agent"
        package.mkdir()
        shutil.copyfile(manifest.parent / "pulsebeam_agent/__init__.py", package / "__init__.py")
        for module in ("pulsebeam_agent_core.py", "pulsebeam_agent_native.py"):
            shutil.copyfile(bindings / module, package / module)
        shutil.copyfile(library, package / "libpulsebeam_agent_native.so")
        raw = root / "raw"
        raw.mkdir()
        os.environ["SOURCE_DATE_EPOCH"] = "0"
        os.chdir(source)
        filename = build_meta.build_wheel(str(raw))
        os.environ["PATH"] = str(patchelf.parent) + os.pathsep + os.defpath
        sys.argv = ["auditwheel", "repair", "--plat", "manylinux_2_39_x86_64", "--only-plat", "--wheel-dir", str(output), str(raw / filename)]
        if auditwheel_main():
            raise RuntimeError("auditwheel repair failed")
        wheels = list(output.glob("*.whl"))
        if len(wheels) != 1 or not wheels[0].name.endswith("-py3-none-manylinux_2_39_x86_64.whl"):
            raise ValueError(f"unexpected audited wheel output: {wheels}")
        sys.argv = ["auditwheel", "show", str(wheels[0])]
        if auditwheel_main():
            raise RuntimeError("auditwheel show failed")


if __name__ == "__main__":
    main()
