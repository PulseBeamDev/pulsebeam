#!/usr/bin/env python3
"""Feed the same corpus motion to the two upstream VP9 pilot modes."""

import argparse
from pathlib import Path
import re
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


def center_crop(plane, width, target):
    margin = (width - target) // 2
    return b"".join(plane[start + margin:start + margin + target]
                    for start in range(0, len(plane), width))


def read_source(archive, zstd, *, pilot):
    width, height = 1280, 720
    y_bytes = width * height
    uv_bytes = y_bytes // 4
    frame_bytes = y_bytes + 2 * uv_bytes
    result = bytearray()
    with archive.open("rb") as compressed, subprocess.Popen(
        [zstd, "-dc"], stdin=compressed, stdout=subprocess.PIPE
    ) as decoder:
        for _ in range(12 if pilot else 36):
            frame = decoder.stdout.read(frame_bytes)
            if len(frame) != frame_bytes:
                raise ValueError("source lacks complete I420 frames")
            planes = (frame[:y_bytes], frame[y_bytes:y_bytes + uv_bytes], frame[y_bytes + uv_bytes:])
            for i, plane in enumerate(planes):
                shift = int(i != 0)
                if pilot:
                    result.extend(half_plane(plane, width >> shift, height >> shift))
                else:
                    # 1152 and 720 support three even I420 spatial rungs at
                    # both 2:1 and 3:2, without interpolation or codec reuse.
                    result.extend(center_crop(plane, width >> shift, 1152 >> shift))
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
    parser.add_argument("--mode", action="append")
    parser.add_argument("--codec", choices=("VP9", "AV1", "VP8", "H264"), default="VP9")
    args = parser.parse_args()
    source = args.out / "source.i420"
    modes = args.mode or ("S2T1", "L2T1")
    if any(not re.fullmatch(r"[A-Z0-9_h]+", mode) for mode in modes):
        parser.error("invalid mode name")
    for pilot in (True, False):
        selected = [mode for mode in modes if (args.codec == "VP9" and mode in ("S2T1", "L2T1")) == pilot]
        if not selected:
            continue
        source.write_bytes(read_source(args.source, args.zstd, pilot=pilot))
        for mode in selected:
            out = args.out / mode
            out.mkdir()
            codec_arg = [] if args.codec == "VP9" else ["--" + args.codec.lower()]
            subprocess.run([args.capture, *codec_arg, mode, source, out], check=True)
        source.unlink()


if __name__ == "__main__":
    main()
