#!/usr/bin/env python3
"""Produce a candidate for the pinned layered sender's non-pilot inventory rows."""

import argparse
import csv
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory

from observe_av1_corpus import observe as observe_av1
from observe_single_video import observe as observe_single_video
from observe_vp9_corpus import observe as observe_vp9
from observe_video_faults import observe as observe_faults
from observe_vp9_pilot import write_archive


# These modes have no controller in the pinned upstream. Keep the rows in
# the normative inventory so absence of a candidate cannot imply qualification.
NO_UPSTREAM_CONTROLLER = {"L2T3_KEY_SHIFT", "L3T2_KEY_SHIFT", "L3T3_KEY_SHIFT"}
PILOT = {"S2T1", "L2T1"}
# The pinned AV1 sender forces a full keyframe for each S3 start. The
# upper keyframe resets the reference slots needed by the lower layer.
UNQUALIFIED_AV1_S3 = {"S3T1", "S3T1h", "S3T2", "S3T2h", "S3T3", "S3T3h"}


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("prepare", "capture", "source", "zstd", "inventory", "out"):
        parser.add_argument("--" + arg, required=True, type=Path)
    observers = {"VP9": observe_vp9, "AV1": observe_av1, "VP8": observe_single_video, "H264": observe_single_video}
    parser.add_argument("--codec", choices=observers, required=True)
    args = parser.parse_args()
    observe = observers[args.codec]
    excluded = NO_UPSTREAM_CONTROLLER | {"VP9": PILOT, "AV1": UNQUALIFIED_AV1_S3}.get(args.codec, set())
    with args.inventory.open() as index:
        modes = [row["mode"] for row in csv.DictReader(index, delimiter="\t")
                 if row["codec"] == args.codec and row["mode"] not in excluded]
    if not modes:
        parser.error(f"no candidate modes for {args.codec}")
    av.logging.set_level(av.logging.ERROR)
    with TemporaryDirectory() as temp:
        capture = Path(temp)
        subprocess.run([sys.executable, args.prepare, "--capture", args.capture,
                        "--source", args.source, "--zstd", args.zstd, "--out", capture,
                        "--codec", args.codec,
                        *(arg for mode in modes for arg in ("--mode", mode))], check=True)
        for mode in modes:
            folder = capture / mode
            try:
                observed = observe(folder, av)
            except ValueError as exc:
                raise ValueError(f"{mode}: {exc}") from exc
            (folder / "observations.json").write_text(json.dumps(observed, indent=2) + "\n")
            faults = observe_faults(folder, av, codec=args.codec)
            (folder / "faults.json").write_text(json.dumps(faults, indent=2) + "\n")
        # Original encoded bytes are checked against RTP during authoring,
        # then retained once on wire. Observations record the pre-wire digests.
        write_archive(args.out, capture, exclude=("encoded.bin",))


if __name__ == "__main__":
    main()
