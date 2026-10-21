"""Apply producer provenance checks to the prebuilt Bazel/Cargo candidate."""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path


def module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def validate(source: Path, native: Path) -> None:
    provenance = module("candidate_cxx", source / "tools/cxx_provenance.py")
    configuration = module("candidate_native", source / "tools/native_provenance.py")
    manifest = json.loads((native / "manifest.json").read_text())
    if manifest["schema_version"] != 4 or manifest["artifact"] != {
        "flavor": "core", "target": "linux-x86_64", "cargo_target": "x86_64-unknown-linux-gnu"
    }:
        raise ValueError("candidate must be schema 4, core Linux x86_64")
    if manifest["bridge"]["identity"] != "pulsebeam-webrtc-sys-bridge-v2":
        raise ValueError("candidate bridge identity mismatch")
    provenance.validate_artifact_provenance(manifest["bridge"]["cxx"], source, native)
    if manifest["bridge"]["native_adapter_sha256"] != configuration.adapter_digest(source):
        raise ValueError("candidate native adapter checksum mismatch")
    configuration.validate_configuration(manifest, source)
    if manifest["archive"]["member"] != "lib/libwebrtc.a":
        raise ValueError("invalid candidate native archive path")
    with (native / "lib/libwebrtc.a").open("rb") as archive:
        if hashlib.file_digest(archive, "sha256").hexdigest() != manifest["archive"]["sha256"]:
            raise ValueError("candidate native library checksum mismatch")
    notices = manifest["licenses"]["files"]
    for notice in notices:
        path = Path(notice["path"])
        if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "LICENSES":
            raise ValueError("invalid candidate license path")
        if provenance.sha256(native / path) != notice["sha256"]:
            raise ValueError(f"candidate license checksum mismatch: {path}")
    if configuration.configuration_digest(notices) != manifest["licenses"]["sha256"]:
        raise ValueError("candidate license inventory checksum mismatch")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-manifest", type=Path, required=True)
    parser.add_argument("--native-manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    validate(args.source_manifest.parent, args.native_manifest.parent)
    args.output.write_text("validated source/native candidate\n")


if __name__ == "__main__":
    main()
