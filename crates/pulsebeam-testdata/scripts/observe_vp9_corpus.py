#!/usr/bin/env python3
"""Qualify VP9 topology and fixed targets from upstream capture observations."""

import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import re

from observe_vp9_pilot import correlate, decode, elementary, require, resolve_dependencies


def topology(units, dependencies, mode):
    match = re.fullmatch(r"([LS])([1-3])T([1-3])(h|_KEY(?:_SHIFT)?)?", mode)
    require(match is not None, f"invalid VP9 mode: {mode}")
    family, spatial, temporal, modifier = match.groups()
    spatial, temporal = int(spatial), int(temporal)
    rows = [r for r, _ in units]
    dimensions = defaultdict(set)
    by_time = defaultdict(dict)
    for unit, row in enumerate(rows):
        sid, tid, pts = (int(row[x]) for x in ("spatial", "temporal", "input_rtp"))
        require(row["mode"] == mode and 0 <= sid < spatial and 0 <= tid < temporal,
                "unexpected emitted mode or layer ID")
        require(sid not in by_time[pts], "duplicate spatial unit at one timestamp")
        by_time[pts][sid] = (tid, unit)
        dimensions[sid].add((int(row["width"]), int(row["height"])))
    require(list(by_time) == list(range(0, 36 * 3000, 3000)), "unexpected input picture sequence")
    require(all(list(layers) == list(range(spatial)) for layers in by_time.values()),
            "missing or reordered spatial units")
    require(all(len(dimensions[sid]) == 1 for sid in range(spatial)), "unstable spatial dimensions")
    sizes = [next(iter(dimensions[sid])) for sid in range(spatial)]
    num, den = (3, 2) if modifier == "h" else (2, 1)
    require(all(all(a * num == b * den for a, b in zip(lower, upper, strict=True))
                for lower, upper in zip(sizes, sizes[1:])), "observed spatial ratio is wrong")

    key_pictures = {pts for pts, layers in by_time.items() if rows[layers[0][1]]["key"] == "1"}
    require({0, 24 * 3000}.issubset(key_pictures), "real restart not observed")
    for unit, row in enumerate(rows):
        sid, pts = int(row["spatial"]), int(row["input_rtp"])
        cross = [rows[ref] for ref in dependencies[unit] if int(rows[ref]["spatial"]) != sid]
        require(all(int(ref["input_rtp"]) == pts and int(ref["spatial"]) < sid for ref in cross),
                "unexpected cross-layer reference")
        expected_cross = (family == "L" and sid > 0
                          and (not (modifier or "").startswith("_KEY") or pts in key_pictures))
        require(bool(cross) == expected_cross, "observed cross-layer pattern does not match mode")
    steady = [layers for pts, layers in by_time.items() if 0 < pts < 24 * 3000]
    phases = [tuple(layers[sid][0] for layers in steady) for sid in range(spatial)]
    period = 1 << (temporal - 1)
    require(all(set(phase) == set(range(temporal)) and phase[period:] == phase[:-period]
                for phase in phases), "temporal cadence did not repeat with all layers")
    require((len(set(phases)) > 1) == (modifier == "_KEY_SHIFT"), "temporal phasing mismatch")
    return sizes, temporal


def observe(folder, av):
    mode = folder.name
    context, units, mapping, wire = correlate(folder)
    require(context["requested_mode"] == mode, "capture name and request disagree")
    dependencies = resolve_dependencies(units)
    sizes, temporal = topology(units, dependencies, mode)
    received = [(row, data, item) for (row, _), data, item in zip(units, wire, mapping, strict=True)]
    targets = []
    for sid, dimensions in enumerate(sizes):
        for tid in range(temporal):
            ivf = elementary(received, dependencies, target=(sid, tid), dimensions=dimensions)
            decoded = decode(av, ivf)
            require(decoded["error"] is None and not decoded["decoder_errors"],
                    f"{mode} S{sid}T{tid} decode failed: {decoded['decoder_errors']}")
            frames = [frame for frame in decoded["frames"]
                      if (frame["width"], frame["height"]) == dimensions]
            want_pts = [int(row["input_rtp"]) for row, _ in units
                        if int(row["spatial"]) == sid and int(row["temporal"]) <= tid]
            require([frame["pts"] for frame in frames] == want_pts
                    and all(frame["time_base"] == "1/90000" for frame in frames),
                    f"{mode} S{sid}T{tid} lost or added presentation pictures")
            targets.append({"spatial": sid, "temporal": tid, "ivf_sha256": hashlib.sha256(ivf).hexdigest(),
                            "decoder_options": decoded["options"], "pictures": frames})
    return {"requested_mode": mode, "codec": "VP9", "decoder_build": {
        "pyav": av.__version__, "ffmpeg_libraries": av.library_versions},
        "wire_units": mapping, "encoder_dependencies": dependencies,
        "pre_packetization_sha256": [hashlib.sha256(payload).hexdigest() for _, payload in units],
        "fixed_targets": targets}


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", nargs="+", type=Path)
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    for folder in args.captures:
        observed = observe(folder, av)
        (folder / "observations.json").write_text(json.dumps(observed, indent=2) + "\n")
        print(f"{folder.name}: {len(observed['fixed_targets'])} decoded fixed targets")


if __name__ == "__main__":
    main()
