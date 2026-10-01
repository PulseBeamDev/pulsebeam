"""Generate package-scoped release plans and the shell installer."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib


def main():
    parser = argparse.ArgumentParser()
    for name in ("manifest", "config", "template"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--out")
    parser.add_argument("--tag")
    args = parser.parse_args()
    package = tomllib.loads(Path(args.manifest).read_text())["package"]
    config = tomllib.loads(Path(args.config).read_text())["distribution"]
    version = package["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        parser.error("manifest has an invalid release version")
    tag = config["tag-prefix"] + version
    if args.tag and args.tag != tag:
        parser.error(f"tag {args.tag!r} does not match authoritative package version ({tag})")
    plan = {"version": version, "tag": tag, "prerelease": "-" in version.split("+", 1)[0], "targets": config["targets"]}
    if args.out:
        out = Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        installer = Path(args.template).read_text()
        for key, value in {"VERSION": version, "TAG": tag, "REPOSITORY": config["repository"]}.items():
            installer = installer.replace(f"@{key}@", value)
        path = out / f"{package['name']}-installer.sh"
        path.write_text(installer)
        path.chmod(0o755)
        (out / (path.name + ".sha256")).write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
        (out / "release-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    else:
        print(json.dumps(plan))


if __name__ == "__main__":
    main()
