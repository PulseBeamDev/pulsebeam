#!/usr/bin/env python3
"""Generate a fail-closed inventory from the configured aspect's action inputs."""

import hashlib
import json
from pathlib import Path, PurePosixPath
import sys
import tomllib


class InventoryError(ValueError):
    pass


def nonempty(value, description):
    if not isinstance(value, str) or not value.strip():
        raise InventoryError(f"missing or empty {description}")
    return value


def safe_path(value):
    nonempty(value, "file path")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or str(path) != value:
        raise InventoryError(f"unsafe or noncanonical file path: {value!r}")
    return value


def kind(path):
    name = PurePosixPath(path).name.lower()
    if name.startswith(("license", "licence", "copying", "unlicense")):
        return "license"
    if name.startswith(("notice", "third_party_notice", "third-party-notice", "third_party_license", "third-party-license")):
        return "notice"
    return None


def text_file(path, description):
    try:
        data = Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise InventoryError(f"cannot read {description}: {exc}") from exc
    if not data.strip():
        raise InventoryError(f"empty {description}")
    return data.replace("\r\n", "\n").replace("\r", "\n")


def generate(index):
    repositories = {}
    for encoded in index["records"]:
        record = json.loads(encoded) if isinstance(encoded, str) else encoded
        repo = nonempty(record["repository"], "repository")
        files = repositories.setdefault(repo, {})
        for relative, source in record["files"]:
            safe_path(relative)
            if relative in files and files[relative] != source:
                raise InventoryError(f"conflicting inputs for {repo}/{relative}")
            files[relative] = source

    supplements = {}
    for item in index["supplemental"]:
        parts = item["spec"].split("|")
        if len(parts) != 3 or any(not p.strip() or p != p.strip() for p in parts):
            raise InventoryError("supplement spec must be repository|name|version")
        key = tuple(parts)
        if key in supplements:
            raise InventoryError(f"duplicate supplement: {key}")
        supplements[key] = item

    manifest = []
    notices = []
    for repo, files in sorted(repositories.items()):
        if "Cargo.toml" not in files:
            raise InventoryError(f"{repo}: missing root Cargo.toml in compile_data")
        cargo = text_file(files["Cargo.toml"], f"{repo}/Cargo.toml")
        try:
            package = tomllib.loads(cargo)["package"]
            name = nonempty(package.get("name"), f"{repo} package.name")
            version = nonempty(package.get("version"), f"{repo} package.version")
            license_expr = package.get("license")
            license_file = package.get("license-file")
            if license_expr is not None:
                nonempty(license_expr, f"{repo} package.license")
            if license_file is not None:
                safe_path(license_file)
            if license_expr is None and license_file is None:
                raise InventoryError(f"{repo}: missing license metadata")
        except (tomllib.TOMLDecodeError, KeyError, AttributeError, TypeError) as exc:
            raise InventoryError(f"{repo}: invalid Cargo.toml: {exc}") from exc
        if license_file is not None and license_file not in files:
            raise InventoryError(f"{repo}: declared license-file {license_file!r} missing from compile_data")

        selected = {p: kind(p) for p in files if kind(p)}
        if license_file is not None:
            selected[license_file] = "license"
        key = (repo, name, version)
        supplement = supplements.pop(key, None)
        if not any(v == "license" and "/" not in p for p, v in selected.items()) and license_file is None and supplement is None:
            raise InventoryError(f"{repo}: no license text for {name} {version}")
        texts = []
        for relative, category in sorted(selected.items()):
            content = text_file(files[relative], f"{repo}/{relative}")
            texts.append((relative, category, content))
        if supplement is not None:
            label = nonempty(supplement["label"], "supplement label")
            content = text_file(supplement["path"], f"supplement {label}")
            texts.append((label, "supplemental", content))
        entries = [{"path": p, "kind": k, "sha256": hashlib.sha256(t.encode("utf-8")).hexdigest()} for p, k, t in texts]
        manifest.append({"repository": repo, "name": name, "version": version, "license": license_expr, "license_file": license_file, "texts": entries})
        notices.append(f"===== {name} {version} ({repo}) =====\n")
        for path, category, content in texts:
            notices.append(f"--- {category}: {path} ---\n{content}")
            if not content.endswith("\n"):
                notices.append("\n")
            notices.append("\n")
    if supplements:
        raise InventoryError(f"unmatched supplemental license(s): {sorted(supplements)}")
    if not manifest:
        raise InventoryError("no crate-universe Rust crates found")
    return json.dumps({"crates": manifest}, indent=2, ensure_ascii=False) + "\n", "".join(notices)


def main():
    try:
        index = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
        manifest, notices = generate(index)
        Path(sys.argv[2]).write_text(manifest, encoding="utf-8")
        Path(sys.argv[3]).write_text(notices, encoding="utf-8")
    except (InventoryError, OSError, ValueError, KeyError, IndexError, TypeError) as exc:
        print(f"native license inventory: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
