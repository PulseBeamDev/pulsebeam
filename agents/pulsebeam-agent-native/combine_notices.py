"""Combine the configured native dependency notice inventories."""

from pathlib import Path
import sys

webrtc, rust, output = (Path(value) for value in sys.argv[1:])
parts = [webrtc.read_bytes(), rust.read_bytes()]
if any(not part.strip() for part in parts):
    raise ValueError("native dependency notice inventory must not be empty")
output.write_bytes(b"# Native dependency notices\n\n" + b"\n\n".join(parts) + b"\n")
