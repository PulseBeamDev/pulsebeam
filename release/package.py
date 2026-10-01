"""Package Bazel's server and immutable updater without invoking Cargo."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tarfile
import tomllib


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    for name in ("binary", "updater", "manifest", "config", "license", "readme", "out"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--triple", required=True)
    args = parser.parse_args()
    inputs = {name: Path(getattr(args, name)).resolve() for name in ("binary", "updater", "manifest", "config", "license", "readme")}
    config = tomllib.loads(inputs["config"].read_text())["distribution"]
    package = tomllib.loads(inputs["manifest"].read_text())["package"]
    if args.triple not in config["targets"]:
        parser.error("unsupported release target")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    archive_name = f"{package['name']}-{args.triple}"
    archive = out / f"{archive_name}.tar.xz"
    with tarfile.open(archive, "w:xz", format=tarfile.GNU_FORMAT) as bundle:
        directory = tarfile.TarInfo(archive_name)
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        bundle.addfile(directory)
        for source, name, mode in ((inputs["binary"], package["name"], 0o755),
                                   (inputs["license"], "LICENSE", 0o644), (inputs["readme"], "README.md", 0o644)):
            info = tarfile.TarInfo(f"{archive_name}/{name}")
            info.size = source.stat().st_size
            info.mode = mode
            info.mtime = 0
            with source.open("rb") as content:
                bundle.addfile(info, content)
    updater = out / f"{archive_name}-update"
    shutil.copyfile(inputs["updater"], updater)
    updater.chmod(0o755)
    for artifact in (archive, updater):
        (out / f"{artifact.name}.sha256").write_text(f"{digest(artifact)}  {artifact.name}\n")
    metadata = {
        "package": package["name"], "version": package["version"], "target": args.triple,
        "tag": config["tag-prefix"] + package["version"],
        "artifacts": {artifact.name: {"sha256": digest(artifact), "size": artifact.stat().st_size}
                      for artifact in (archive, updater)},
    }
    (out / f"{archive_name}.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
