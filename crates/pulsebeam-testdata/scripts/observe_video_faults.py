#!/usr/bin/env python3
"""Author loss and restart-switch observations from retained video RTP bytes."""

import argparse
import hashlib
import json
from pathlib import Path
import struct

from observe_av1_corpus import av1_ivf, correlate as correlate_av1
from observe_single_video import correlate as correlate_single, decode_units
from observe_vp9_corpus import topology
from observe_vp9_pilot import correlate, decode, require, resolve_dependencies, target_units, vp9_ivf


def recipes(rows, edges, mapping, target):
    restart, = [int(r["input_rtp"]) for r in rows if r["key"] == "1" and r["spatial"] == "0"
                and int(r["input_rtp"]) > 0]
    pts = [int(r["input_rtp"]) for r in rows]
    base = target_units(rows, edges, (0, 0))
    full = target_units(rows, edges, target)
    candidates = [i for i in base if 0 < pts[i] < restart]
    require(candidates, "no non-key base units before restart")
    drop = next((i for i in candidates if len(mapping[i]["packets"]) > 1), candidates[0])
    missing = mapping[drop]["packets"][-1]
    unavailable = set()
    for i, refs in enumerate(edges):
        if missing in mapping[i]["packets"] or unavailable.intersection(refs):
            unavailable.add(i)
    retained = [i for i in full if i not in unavailable]
    require(all(i in retained for i in full if pts[i] >= restart), "target did not recover at restart")
    loss = {"lost_packet": missing, "lost_unit": drop, "units": retained}
    switch = None
    if target != (0, 0):
        selected = [i for i in base if pts[i] < restart] + [i for i in full if pts[i] >= restart]
        available = set(selected)
        require(all(available.issuperset(edges[i]) for i in selected), "switch omits dependencies")
        switch = {"units": selected}
    return restart, loss, switch


def observe(folder, av=None, *, codec="VP9", retained=None):
    baseline = json.loads((folder / "observations.json").read_text())
    if codec == "VP9":
        _, units, mapping, wire = correlate(folder, pre_packetization_sha256=baseline["pre_packetization_sha256"])
    elif codec == "AV1":
        _, units, mapping, wire = correlate_av1(folder, retained=baseline["wire_units"])
    else:
        emitted, _, units, mapping, wire, _ = correlate_single(folder, retained=baseline["wire_units"])
        require(emitted == codec, "capture codec and requested codec differ")
    edges = resolve_dependencies(units)
    require(edges == baseline["encoder_dependencies"] and mapping == baseline["wire_units"],
            "baseline is not for these packets")
    sizes, temporals = topology(units, edges, folder.name)
    return observe_wire(units, edges, mapping, wire, sizes, temporals, baseline, av, codec=codec, retained=retained)


def decoder_input(codec, rows, mapping, wire, selected, dimensions):
    if codec == "VP9":
        received = list(zip(rows, wire, mapping, strict=True))
        return "ivf_sha256", vp9_ivf(received, selected, dimensions)
    if codec == "AV1":
        return "ivf_sha256", av1_ivf(rows, wire, selected, dimensions)
    # Hash exact decoder packet boundaries and timestamps as well as bytes.
    return "packet_stream_sha256", b"".join(
        struct.pack("<II", int(rows[i]["input_rtp"]), len(wire[i])) + wire[i] for i in selected)


def observe_wire(units, edges, mapping, wire, sizes, temporals, baseline, av=None, *, codec="VP9", retained=None):
    target = (len(sizes) - 1, temporals - 1)
    baseline_targets = {(o.get("spatial", 0), o["temporal"]): o["pictures"] for o in baseline["fixed_targets"]}
    rows = [row for row, _ in units]
    restart, loss, switch = recipes(rows, edges, mapping, target)
    by_pts = {int(rows[i]["input_rtp"]) for i in loss["units"] if int(rows[i]["spatial"]) == target[0]}
    expected_loss = [frame for frame in baseline_targets[target] if frame["pts"] in by_pts]
    for name, recipe in (("loss", loss), ("switch", switch)):
        if recipe is None:
            continue
        selected = recipe["units"]
        hash_key, data = decoder_input(codec, rows, mapping, wire, selected, sizes[-1])
        if retained is None:
            if codec in ("AV1", "VP9"):
                decoded = decode(av, data, decoder="libdav1d" if codec == "AV1" else None)
                require(decoded["error"] is None and not decoded["decoder_errors"],
                        f"{name} decode failed: {decoded['decoder_errors']}")
                pictures = decoded["frames"]
            else:
                pictures = decode_units(av, codec, rows, wire, selected)
            frames = [f for f in pictures if (f["width"], f["height"]) ==
                      (sizes[0] if name == "switch" and f["pts"] < restart else sizes[-1])]
        else:
            frames = retained[name]["pictures"]
        expected = expected_loss if name == "loss" else (
            [f for f in baseline_targets[0, 0] if f["pts"] < restart] +
            [f for f in baseline_targets[target] if f["pts"] >= restart])
        require(frames == expected, f"{name} output differs from fixed decoded targets")
        recipe.update({hash_key: hashlib.sha256(data).hexdigest(), "pictures": frames})
    result = {"restart_rtp": restart, "target": list(target), "loss": loss, "switch": switch}
    if retained is not None:
        require(result == retained, "fault observations differ from retained evidence")
    return result


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--codec", choices=("AV1", "H264", "VP8", "VP9"), required=True)
    parser.add_argument("captures", nargs="+", type=Path)
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    for folder in args.captures:
        try:
            result = observe(folder, av, codec=args.codec)
        except ValueError as exc:
            raise ValueError(f"{folder.name} fault observation failed: {exc}") from exc
        destination = args.out / folder.name
        destination.mkdir(parents=True, exist_ok=True)
        (destination / "faults.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
