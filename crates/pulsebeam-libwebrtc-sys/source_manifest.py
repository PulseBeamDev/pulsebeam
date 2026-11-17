"""Identify the acquired GN inputs without modifying the upstream checkout."""

import hashlib
import os
from pathlib import Path
import sys


def excluded(parts):
    return (
        ".git" in parts
        or "__pycache__" in parts
        or parts[-1].endswith(".pyc")
        or any(parts[i:i + 2] == ("lib", "systemd") for i in range(len(parts) - 1))
    )


def prepare(source, manifest):
    source = source.resolve(strict=True)
    identity = hashlib.sha256(b"pulsebeam-gn-source-manifest-v2\0")
    directory_edges = {}

    def record(kind, path, content=b""):
        for value in (kind, os.fsencode(path), content):
            identity.update(len(value).to_bytes(8, "big"))
            identity.update(value)

    def visit(parent=()):
        edges = directory_edges.setdefault(parent, [])
        with os.scandir(os.path.join(source, *parent)) as entries:
            for entry in sorted(entries, key=lambda item: os.fsencode(item.name)):
                parts = parent + (entry.name,)
                path = "/".join(parts)
                if excluded(parts):
                    continue
                if entry.is_symlink():
                    link = os.readlink(entry.path)
                    try:
                        resolved = Path(entry.path).resolve()
                    except (OSError, RuntimeError) as error:
                        raise ValueError(f"Invalid source link: {path}") from error
                    if os.path.isabs(link) or not resolved.is_relative_to(source):
                        raise ValueError(f"Source link escapes the acquired tree: {path} -> {link}")
                    if resolved != source and excluded(resolved.relative_to(source).parts):
                        raise ValueError(f"Source link targets excluded metadata: {path} -> {link}")
                    record(b"link", path, os.fsencode(link))
                    if resolved.is_dir():
                        edges.append(resolved.relative_to(source).parts)
                elif entry.is_dir(follow_symlinks=False):
                    record(b"directory", path)
                    edges.append(parts)
                    visit(parts)
                elif entry.is_file(follow_symlinks=False):
                    with open(entry.path, "rb") as stream:
                        executable = os.fstat(stream.fileno()).st_mode & 0o111
                        digest = hashlib.file_digest(stream, "sha256").digest()
                    record(b"executable" if executable else b"file", path, digest)
                else:
                    raise ValueError(f"Unsupported source entry: {path}")

    visit()
    active = set()
    complete = set()

    def validate_directory(path):
        if path in active:
            raise ValueError(f"Cyclic source directory link: {'/'.join(path)}")
        if path in complete:
            return
        active.add(path)
        for child in directory_edges.get(path, ()):
            validate_directory(child)
        active.remove(path)
        complete.add(path)

    validate_directory(())
    manifest.write_text(identity.hexdigest() + "\n")


if __name__ == "__main__":
    prepare(*(Path(value) for value in sys.argv[1:]))
