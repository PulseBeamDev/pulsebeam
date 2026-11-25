#!/usr/bin/env python3
"""Replay retained Opus packet and loss evidence without loading a decoder."""

import json
from pathlib import Path
import sys

from observe_opus_rtp import LOSS_INDEX, PACKETS, STEP, correlate
from observe_vp9_pilot import require


def main(folder):
    sources = {r["file"]: r for r in json.loads((folder / "observations.json").read_text())["sources"]}
    evidence = json.loads((folder / "rtp.json").read_text())
    require(len(evidence["sources"]) == len(sources)
            and {r["source"] for r in evidence["sources"]} == set(sources), "Opus source set differs")
    for case in evidence["sources"]:
        source = case["source"]
        context, _, wire, reports = correlate(folder / Path(source).stem, sources[source])
        require((context, wire, reports) == (case["context"], case["wire_units"], case["sender_reports"]),
                "Opus wire mapping differs")
        intact, loss = case["intact"], case["loss"]
        require(loss["missing"] == wire[LOSS_INDEX] and len(intact) == PACKETS,
                "incorrect Opus loss case")
        # The original Ogg timeline starts one Opus pre-skip before zero.
        pre_skip = -sources[source]["packets"][0]["pts"]
        require(0 <= pre_skip < STEP and [f["pts"] for f in intact] == list(range(0, PACKETS * STEP, STEP)),
                "wrong Opus PCM timestamps or pre-skip")
        for frames in (intact, loss["frames"]):
            require(all(f["samples"] == STEP - (pre_skip if f["pts"] == 0 else 0) for f in frames),
                    "wrong Opus PCM frame length")
        require(loss["frames"][:LOSS_INDEX] == intact[:LOSS_INDEX]
                and [f["pts"] for f in loss["frames"]]
                == [f["pts"] for i, f in enumerate(intact) if i != LOSS_INDEX], "missing exact Opus PCM gap")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
