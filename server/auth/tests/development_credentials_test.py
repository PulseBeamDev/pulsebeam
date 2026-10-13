"""Packaged SDKs must embed the same public local-development identity."""
from pathlib import Path

root = Path(__file__).resolve().parents[3]
source = (root / "server/auth/development.json").read_bytes()
for path in (
    "server/go/development.json",
    "server/rust/src/development.json",
    "server/python/pulsebeam_server/development.json",
    "server/typescript/src/development.json",
):
    assert (root / path).read_bytes() == source, f"development credentials differ: {path}"
