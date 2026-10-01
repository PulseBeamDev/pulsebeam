"""Add hashes and release metadata to the archive assembled by rules_pkg."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tomllib


def main():
    parser = argparse.ArgumentParser()
    for name in ("archive", "manifest", "config", "out", "triple"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    config = tomllib.loads(Path(args.config).read_text())["distribution"]
    package = tomllib.loads(Path(args.manifest).read_text())["package"]
    if args.triple not in config["targets"]:
        parser.error("unsupported release target")
    stem = f"{package['name']}-{args.triple}"
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    archive = out / f"{stem}.tar.xz"
    shutil.copyfile(args.archive, archive)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (out / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")
    metadata = {
        "package": package["name"], "version": package["version"], "target": args.triple,
        "tag": config["tag-prefix"] + package["version"],
        "artifacts": {archive.name: {"sha256": digest, "size": archive.stat().st_size}},
    }
    (out / f"{stem}.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
