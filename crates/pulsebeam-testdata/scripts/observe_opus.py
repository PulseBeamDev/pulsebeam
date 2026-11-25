#!/usr/bin/env python3
"""Observe Ogg Opus packet boundaries and decoded PCM in the quality corpus."""

import argparse
from array import array
import hashlib
import json
from pathlib import Path
import subprocess
import sys


def digest(data):
    return hashlib.sha256(data).hexdigest()


def inspect(source, zstd, av):
    encoded = source.read_bytes()
    with source.with_suffix(".s16le.zst").open("rb") as compressed:
        reference = subprocess.check_output([zstd, "-dc"], stdin=compressed)
    with av.open(str(source), format="ogg") as container:
        stream, = container.streams.audio
        if (stream.codec_context.name, stream.rate, stream.channels) != ("opus", 48000, 1):
            raise ValueError(f"unexpected audio binding in {source.name}")
        convert = av.AudioResampler(format="s16", layout="mono", rate=48000)
        observed = bytearray()
        packets = []

        def collect(frames):
            for frame in frames:
                observed.extend(bytes(frame.planes[0])[:frame.samples * 2])

        for packet in container.demux(stream):
            if packet.size:
                data = bytes(packet)
                packets.append({"pts": packet.pts, "duration": packet.duration,
                                "bytes": len(data), "sha256": digest(data)})
            for frame in packet.decode():
                collect(convert.resample(frame))
        collect(convert.resample(None))
        clock = str(stream.time_base)
    if clock != "1/48000" or len(packets) < 300 or not all(p["duration"] == 960 for p in packets[:300]):
        raise ValueError(f"unexpected Opus cadence in {source.name}")
    if len(observed) != len(reference) or not observed:
        raise ValueError(f"decoded sample count differs from the reference for {source.name}")
    actual, expected = array("h", observed), array("h", reference)
    if sys.byteorder != "little":
        expected.byteswap()
        actual.byteswap()
    peak = max(abs(a - b) for a, b in zip(actual, expected, strict=True))
    # Floating-point decoder builds may round a sample differently at an S16 boundary.
    if peak > 1:
        raise ValueError(f"decoded PCM differs by {peak} LSBs for {source.name}")
    return {"file": source.name, "source_sha256": digest(encoded), "time_base": clock,
            "decoder": "opus", "packets": packets, "samples": len(observed) // 2,
            "reference_pcm_sha256": digest(reference), "decoded_pcm_sha256": digest(observed),
            "max_sample_error": peak}


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--zstd", required=True, type=Path)
    parser.add_argument("sources", nargs="+", type=Path)
    args = parser.parse_args()
    print(json.dumps({"decoder_build": {"pyav": av.__version__, "ffmpeg_libraries": av.library_versions},
                      "sources": [inspect(source, args.zstd, av) for source in args.sources]}, indent=2))


if __name__ == "__main__":
    main()
