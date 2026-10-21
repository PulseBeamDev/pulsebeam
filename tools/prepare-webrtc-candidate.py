"""Install checksum-pinned candidate archives for Cargo metadata and builds."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import tarfile
import tempfile

from webrtc_candidate import validate


def unpack(archive: Path, digest: str, destination: Path, revision: str | None = None) -> None:
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
        if actual != digest:
            raise ValueError(f"candidate checksum mismatch: {archive}: expected {digest}, got {actual}")
        source.seek(0)
        with tarfile.open(fileobj=source, mode="r:gz") as bundle:
            if revision is not None and bundle.pax_headers.get("comment") != revision:
                raise ValueError("candidate source archive revision mismatch")
            members = bundle.getmembers()
            names = {}
            for member in members:
                path = PurePosixPath(member.name)
                if (path.is_absolute() or ".." in path.parts or "\\" in member.name
                        or not (member.isfile() or member.isdir()) or path in names):
                    raise ValueError(f"invalid candidate archive member: {member.name}")
                names[path] = member.isdir()
            for path in names:
                if any(parent in names and not names[parent] for parent in path.parents):
                    raise ValueError(f"candidate archive file/directory conflict: {path}")
            bundle.extractall(destination, members=members, filter="data")


def prepare(root: Path, lock: dict, source: Path, native: Path) -> Path:
    store = root / ".native-candidate"
    store.mkdir(exist_ok=True)
    identity = lock["source"]["sha256"] + "-" + lock["native"]["sha256"]
    destination = store / identity
    # Always validate the supplied archives, including when selecting an existing
    # candidate. Only this script's content-addressed outputs may be replaced.
    with tempfile.TemporaryDirectory(dir=store, prefix="prepare-") as temporary:
        stage = Path(temporary)
        unpack(source, lock["source"]["sha256"], stage / "source", lock["source"]["revision"])
        unpack(native, lock["native"]["sha256"], stage / "native")
        validate(stage / "source", stage / "native")
        if destination.exists():
            shutil.rmtree(destination)
        stage.rename(destination)
    current = store / "current"
    if current.exists() and not current.is_symlink():
        raise ValueError(f"refusing to replace non-symlink candidate selection: {current}")
    selection = store / "next"
    selection.unlink(missing_ok=True)
    selection.symlink_to(identity, target_is_directory=True)
    os.replace(selection, current)
    return current


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--native", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    lock = json.loads((root / "tools/webrtc-candidate.json").read_text())
    print(prepare(root, lock, args.source, args.native))


if __name__ == "__main__":
    main()
