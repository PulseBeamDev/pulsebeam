"""Manifest-versioned npm plans, local login, and explicit SDK publication."""

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile

PACKAGES = {
    "web": "@pulsebeam/web",
    "react": "@pulsebeam/react",
    "server": "@pulsebeam/server",
}
REGISTRY = "https://registry.npmjs.org/"
VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-((?:[0-9A-Za-z-]+)(?:\.[0-9A-Za-z-]+)*))?"
)


def package_plan(key, manifests):
    name = PACKAGES[key]
    version = manifests[name]["version"]
    match = VERSION.fullmatch(version)
    if not match or (match[4] and any(
        part.isdigit() and len(part) > 1 and part.startswith("0")
        for part in match[4].split(".")
    )):
        raise ValueError("package must contain a canonical release version")
    return {"name": name, "version": version, "dist_tag": "next" if match[4] else "latest"}


def plan(tag, manifests):
    for key in PACKAGES:
        prefix = f"npm-{key}-v"
        if tag.startswith(prefix):
            release = package_plan(key, manifests)
            if tag[len(prefix):] != release["version"]:
                raise ValueError("tag does not match the package manifest version")
            return release
    raise ValueError("expected npm-web-v*, npm-react-v* or npm-server-v* tag")


def validate_archive(path, expected):
    with tarfile.open(path, "r:gz") as archive:
        files = {}
        for member in archive.getmembers():
            parts = PurePosixPath(member.name).parts
            if not parts or parts[0] != "package" or ".." in parts:
                raise ValueError("archive contains an unsafe package path")
            if member.isdir():
                continue
            if not member.isfile() or member.name in files or "node_modules" in parts:
                raise ValueError("archive must contain unique regular package files only")
            files[member.name] = archive.extractfile(member).read()
    manifest = json.loads(files["package/package.json"])
    if manifest != expected:
        raise ValueError("packed manifest differs from repository authority")
    if manifest.get("private") or manifest.get("publishConfig", {}).get("access") != "public":
        raise ValueError("package must explicitly support public publication")
    for document in ("README.md", "LICENSE"):
        if not files.get(f"package/{document}"):
            raise ValueError(f"package is missing {document}")
    for entry in manifest["exports"]["."].values():
        if not entry.startswith("./dist/") or not files.get("package/" + entry[2:]):
            raise ValueError("public export is missing from the archive")
    for group in ("dependencies", "peerDependencies"):
        for requirement in manifest.get(group, {}).values():
            if any(marker in requirement for marker in ("link:", "workspace:", "file:")):
                raise ValueError("public dependencies cannot use local workspace paths")
    if manifest["name"] == "@pulsebeam/web":
        wasm = [body for name, body in files.items() if name.endswith(".wasm")]
        if not wasm or not all(body.startswith(b"\0asm\x01\0\0\0") for body in wasm):
            raise ValueError("web package is missing its compiled WASM runtime")
    return manifest


def publish(release, manifest, archive, npm, *, trusted=True):
    if trusted and (os.environ.get("GITHUB_ACTIONS") != "true" or not all(os.environ.get(key) for key in (
        "ACTIONS_ID_TOKEN_REQUEST_URL", "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    ))):
        raise ValueError("automated publication requires GitHub Actions OIDC")
    if manifest["name"] == "@pulsebeam/react":
        requirement = manifest["dependencies"]["@pulsebeam/web"]
        # Resolve the actual public dependency before publishing a dependent SDK.
        available = subprocess.check_output([
            npm, "view", f"@pulsebeam/web@{requirement}", "version", "--json",
            "--registry", REGISTRY,
        ], text=True)
        if not available.strip() or not json.loads(available):
            raise ValueError("publish a compatible @pulsebeam/web version first")
    subprocess.run([
        npm, "publish", str(archive), "--access", "public",
        "--tag", release["dist_tag"],
        *(["--provenance"] if trusted else []), "--ignore-scripts",
        "--registry", REGISTRY,
    ], check=True)


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", action="append", type=Path, default=[])
    parser.add_argument("--archive", action="append", type=Path, default=[])
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--tag", help="CI Git tag, checked against the manifest")
    action.add_argument("--package", choices=PACKAGES, help="local release using the manifest version")
    action.add_argument("--login", action="store_true", help="log into npm with your local identity")
    parser.add_argument("--npm", type=Path)
    parser.add_argument("--publish", action="store_true")
    return parser.parse_args()


def main():
    args = arguments()
    if args.login:
        if args.npm is None:
            raise ValueError("login needs the Bazel npm CLI")
        subprocess.run([str(args.npm.resolve()), "login", "--registry", REGISTRY], check=True)
        return
    manifests = {document["name"]: document for document in (
        json.loads(path.read_text()) for path in args.manifest
    )}
    release = plan(args.tag, manifests) if args.tag else package_plan(args.package, manifests)
    archives = {}
    for path in args.archive:
        with tarfile.open(path) as archive:
            name = json.load(archive.extractfile("package/package.json"))["name"]
        validate_archive(path, manifests[name])
        archives[name] = path.resolve()
    if args.publish:
        if args.npm is None or release["name"] not in archives:
            raise ValueError("publication needs the Bazel CLI and validated archive")
        publish(
            release, manifests[release["name"]], archives[release["name"]],
            str(args.npm.resolve()), trusted=args.tag is not None,
        )
    print(json.dumps(release, sort_keys=True))


if __name__ == "__main__":
    main()
