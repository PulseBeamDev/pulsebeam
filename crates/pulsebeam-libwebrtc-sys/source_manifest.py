"""Materialize the declared GN file view after acquisition and identify its contents."""

import hashlib
import os
from pathlib import Path
import shutil
import sys
import tempfile


def excluded(parts):
    return (
        parts[0] == "resources"
        or ".git" in parts
        or "__pycache__" in parts
        or parts[-1].endswith(".pyc")
        or any(parts[i:i + 2] == ("lib", "systemd") for i in range(len(parts) - 1))
    )


def prepare(source, manifest):
    source = source.resolve(strict=True)
    identity = hashlib.sha256(b"pulsebeam-gn-source-manifest-v1\0")

    def record(kind, path, content=b""):
        for value in (kind, os.fsencode(path), content):
            identity.update(len(value).to_bytes(8, "big"))
            identity.update(value)

    def link_tree(original, destination, parts, ancestors):
        if excluded(parts):
            return None
        if original.is_symlink():
            link = os.readlink(original)
            resolved = original.resolve()
            if os.path.isabs(link) or not resolved.is_relative_to(source):
                raise ValueError(f"Source link escapes the acquired tree: {original} -> {link}")
            if resolved != source and excluded(resolved.relative_to(source).parts):
                raise ValueError(f"Source link targets excluded metadata: {original} -> {link}")
            if not resolved.exists():
                return None
            original = resolved
        if original.is_dir():
            if original in ancestors:
                raise ValueError(f"Cyclic source directory link: {original}")
            destination.mkdir()
            with os.scandir(original) as entries:
                for entry in entries:
                    link_tree(Path(entry.path), destination / entry.name, parts + (entry.name,), ancestors + (original,))
            return "directory"
        if original.is_file():
            os.link(original, destination)
            return "file"
        raise ValueError(f"Unsupported source entry: {original}")

    def materialize(physical, parts):
        # Match the previous Bazel file-glob view, including directory aliases,
        # without changing native action keys or copying the whole checkout.
        original = Path(physical)
        with tempfile.TemporaryDirectory(prefix=".gn-link-", dir=original.parent) as temporary:
            destination = Path(temporary) / "entry"
            kind = link_tree(original, destination, parts, tuple(original.parents))
            if kind != "file":
                original.unlink()
            if kind is not None:
                os.replace(destination, original)
            return kind

    def record_file(path, physical):
        with open(physical, "rb") as stream:
            executable = os.fstat(stream.fileno()).st_mode & 0o111
            digest = hashlib.file_digest(stream, "sha256").digest()
        record(b"executable" if executable else b"file", path, digest)

    def visit(parent=()):
        with os.scandir(os.path.join(source, *parent)) as entries:
            for entry in sorted(entries, key=lambda item: os.fsencode(item.name)):
                parts = parent + (entry.name,)
                path = "/".join(parts)
                physical = entry.path
                if excluded(parts):
                    if entry.is_dir(follow_symlinks=False):
                        shutil.rmtree(physical)
                    else:
                        os.unlink(physical)
                elif entry.is_dir(follow_symlinks=False):
                    record(b"directory", path)
                    visit(parts)
                elif entry.is_file(follow_symlinks=False):
                    record_file(path, physical)
                elif entry.is_symlink():
                    kind = materialize(physical, parts)
                    if kind == "directory":
                        record(b"directory", path)
                        visit(parts)
                    elif kind == "file":
                        record_file(path, physical)
                else:
                    raise ValueError(f"Unsupported source entry: {path}")

    visit()
    manifest.write_text(identity.hexdigest() + "\n")


if __name__ == "__main__":
    prepare(*(Path(value) for value in sys.argv[1:]))
