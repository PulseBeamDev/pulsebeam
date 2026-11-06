"""Offline PyPI agent release plan and wheel contract; no publishing or builds.

CLI: release.py plan --pyproject PATH --tag TAG
     release.py validate --pyproject PATH --tag TAG --wheel PATH
Both print a JSON plan. Validation checks wheel structure and declared tags only;
it does not inspect ELF dependencies or establish manylinux/auditwheel compliance.
"""

import argparse
from email import policy
from email.parser import BytesParser
import json
from pathlib import Path
import re
import stat
import struct
import tomllib
import zipfile
import zlib

NAME = "pulsebeam-agent"
DIST = "pulsebeam_agent"
TAG = "py3-none-manylinux_2_39_x86_64"
# Canonical public PEP 440 spellings. PyPI does not accept local versions.
NUMBER = r"(?:0|[1-9][0-9]*)"
VERSION = re.compile(
    rf"(?:(?:[1-9][0-9]*)!)?{NUMBER}(?:\.{NUMBER})*"
    rf"(?P<pre>(?:a|b|rc){NUMBER})?"
    rf"(?:\.post{NUMBER})?(?P<dev>\.dev{NUMBER})?"
)


def plan(pyproject, tag):
    """Read static project identity and require its exact canonical release tag."""
    try:
        with Path(pyproject).open("rb") as source:
            document = tomllib.load(source)
        project = document["project"]
        name, version = project["name"], project["version"]
    except (OSError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise ValueError(f"invalid pyproject: {error}") from error
    dynamic = project.get("dynamic", [])
    if project.get("dependencies", []) != []:
        raise ValueError("agent wheel must not depend on external Python packages")
    if (name != NAME or not isinstance(version, str) or not isinstance(dynamic, list)
            or "version" in dynamic):
        raise ValueError("project must have the static pulsebeam-agent name and version")
    match = VERSION.fullmatch(version)
    if not match:
        raise ValueError("version must be a canonical public PEP 440 version")
    if tag != f"pypi-agent-v{version}":
        raise ValueError("tag does not match project version")
    return {
        "name": NAME,
        "version": version,
        "tag": tag,
        "intent": "prerelease" if match["pre"] or match["dev"] else "stable",
        "wheel_tag": TAG,
    }


def _headers(data, name):
    message = BytesParser(policy=policy.default).parsebytes(data, headersonly=True)
    if message.defects:
        raise ValueError(f"malformed {name} headers")
    return message


def _one(message, field, expected):
    if message.get_all(field, []) != [expected]:
        raise ValueError(f"invalid {field}")


def validate_wheel(wheel, release):
    """Validate one supplied wheel against a plan; raise ValueError on failure."""
    wheel = Path(wheel)
    version = release["version"]
    escaped = re.sub(r"[^A-Za-z0-9.]+", "_", version)
    prefix = f"{DIST}-{escaped}"
    # An optional PEP 427 build tag starts with a digit.
    filename = re.escape(prefix) + r"(?:-[0-9][A-Za-z0-9_]*)?-" + re.escape(TAG) + r"\.whl"
    if not re.fullmatch(filename, wheel.name):
        raise ValueError("wheel filename does not match release identity and tag")
    dist_info = prefix + ".dist-info"
    required = {
        f"{DIST}/__init__.py", f"{DIST}/{DIST}_core.py",
        f"{DIST}/{DIST}_native.py", f"{DIST}/lib{DIST}_native.so",
        f"{dist_info}/METADATA", f"{dist_info}/WHEEL", f"{dist_info}/RECORD",
    }
    try:
        with zipfile.ZipFile(wheel) as archive:
            names = set()
            files = set()
            for info in archive.infolist():
                name = info.filename
                parts = name.rstrip("/").split("/")
                if (not name or name != info.orig_filename or name.startswith("/")
                        or "\\" in name or ":" in parts[0]
                        or any(part in ("", ".", "..") for part in parts)
                        or any("\x00" in part for part in parts)):
                    raise ValueError(f"unsafe wheel path: {name!r}")
                if name in names:
                    raise ValueError(f"duplicate wheel entry: {name}")
                names.add(name)
                mode = info.external_attr >> 16
                if info.create_system == 3 and stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR):
                    raise ValueError(f"non-regular wheel entry: {name}")
                if not info.is_dir():
                    files.add(name)
                    # Reading every member verifies its CRC and catches truncated entries.
                    with archive.open(info) as member:
                        while member.read(1024 * 1024):
                            pass
            if not required <= files:
                raise ValueError(f"missing wheel entries: {sorted(required - files)}")
            for name in names:
                parts = name.rstrip("/").split("/")
                if any("/".join(parts[:i]) in files for i in range(1, len(parts))):
                    raise ValueError("wheel contains a file as a parent directory")
                if name.endswith("/") and name[:-1] in files:
                    raise ValueError("wheel contains a file and directory at the same path")
            if any(p.split("/", 1)[0].endswith(".dist-info") and not p.startswith(dist_info + "/") for p in names):
                raise ValueError("unexpected dist-info directory")
            license_roots = (f"{dist_info}/licenses/", f"{DIST}/")
            license_names = set()
            for entry in files:
                for root in license_roots:
                    if entry.startswith(root):
                        license_names.add(entry[len(root):].rsplit("/", 1)[-1].upper())
            if not any(entry.startswith(license_roots)
                       and re.fullmatch(r"LICENSE(?:\.(?:TXT|MD))?", entry.rsplit("/", 1)[-1].upper())
                       and archive.read(entry) for entry in files):
                raise ValueError("missing or empty project license")
            if not {"THIRD_PARTY_NOTICES.MD", "RUST_STANDARD_LIBRARY_NOTICES.HTML"} <= license_names:
                raise ValueError("missing native third-party or Rust standard-library notice")
            for basename in ("THIRD_PARTY_NOTICES.MD", "RUST_STANDARD_LIBRARY_NOTICES.HTML"):
                if not any(entry.startswith(license_roots) and entry.rsplit("/", 1)[-1].upper() == basename
                           and archive.read(entry) for entry in files):
                    raise ValueError(f"empty {basename}")
            if not archive.read(f"{DIST}/lib{DIST}_native.so").startswith(b"\x7fELF"):
                raise ValueError("native library is not ELF")
            for name in (f"{DIST}/{DIST}_core.py", f"{DIST}/{DIST}_native.py"):
                if not archive.read(name):
                    raise ValueError(f"empty generated module: {name}")
            metadata = _headers(archive.read(f"{dist_info}/METADATA"), "METADATA")
            if len(metadata.get_all("Metadata-Version", [])) != 1 or not re.fullmatch(
                r"2\.[1-5]", metadata["Metadata-Version"] or ""
            ):
                raise ValueError("invalid Metadata-Version")
            _one(metadata, "Name", NAME)
            _one(metadata, "Version", version)
            if metadata.get_all("Requires-Dist", []):
                raise ValueError("agent wheel must not depend on external Python packages")
            wheel_metadata = _headers(archive.read(f"{dist_info}/WHEEL"), "WHEEL")
            _one(wheel_metadata, "Wheel-Version", "1.0")
            _one(wheel_metadata, "Root-Is-Purelib", "false")
            _one(wheel_metadata, "Tag", TAG)
    except (OSError, zipfile.BadZipFile, zipfile.LargeZipFile, EOFError,
            NotImplementedError, RuntimeError, UnicodeError, zlib.error, struct.error) as error:
        raise ValueError(f"invalid wheel archive: {error}") from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("plan", "validate"):
        sub = commands.add_parser(command)
        sub.add_argument("--pyproject", required=True, type=Path)
        sub.add_argument("--tag", required=True)
        if command == "validate":
            sub.add_argument("--wheel", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        release = plan(args.pyproject, args.tag)
        if args.command == "validate":
            wheel = args.wheel
            if wheel.is_dir():
                wheels = list(wheel.glob("*.whl"))
                if len(wheels) != 1:
                    raise ValueError("wheel directory must contain exactly one wheel")
                wheel = wheels[0]
            validate_wheel(wheel, release)
    except ValueError as error:
        parser.exit(1, f"error: {error}\n")
    print(json.dumps(release, sort_keys=True))


if __name__ == "__main__":
    main()
