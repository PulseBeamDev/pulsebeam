#!/usr/bin/env python3
"""Feed the same corpus motion to the two upstream VP9 pilot modes."""

import argparse
from pathlib import Path
import subprocess


# Same integer I420 box filter as quality_source.rs, without linking testdata
# or PulseBeam into the authoring tool.
def half_plane(plane, width, height):
    return bytes(
        (plane[i] + plane[i + 1] + plane[i + width] + plane[i + width + 1]) // 4
        for y in range(0, height, 2)
        for x in range(0, width, 2)
        for i in (y * width + x,)
    )


def read_source(archive, zstd):
    width, height = 1280, 720
    y_bytes = width * height
    uv_bytes = y_bytes // 4
    frame_bytes = y_bytes + 2 * uv_bytes
    result = bytearray()
    with archive.open("rb") as compressed, subprocess.Popen(
        [zstd, "-dc"], stdin=compressed, stdout=subprocess.PIPE
    ) as decoder:
        for _ in range(12):
            frame = decoder.stdout.read(frame_bytes)
            if len(frame) != frame_bytes:
                raise ValueError("source lacks twelve complete I420 frames")
            result.extend(half_plane(frame[:y_bytes], width, height))
            for start in (y_bytes, y_bytes + uv_bytes):
                result.extend(half_plane(frame[start:start + uv_bytes], width // 2, height // 2))
        # Check the complete archive, including its checksum, before using it.
        while decoder.stdout.read(1 << 20):
            pass
        if decoder.wait():
            raise ValueError("source archive did not decompress cleanly")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", required=True, type=Path)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--zstd", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    source = args.out / "source.i420"
    source.write_bytes(read_source(args.source, args.zstd))
    for mode in ("S2T1", "L2T1"):
        out = args.out / mode
        out.mkdir()
        subprocess.run([args.capture, mode, source, out], check=True)
    source.unlink()


if __name__ == "__main__":
    main()
