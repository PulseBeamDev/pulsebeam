#!/usr/bin/env python3
"""Verify immutable layered corpus evidence without linking a decoder."""

import argparse
import csv
import hashlib
import json
from pathlib import Path

from observe_av1_corpus import correlate as correlate_av1, observe as observe_av1
from observe_single_video import correlate as correlate_single, observe as observe_single_video
from observe_vp9_corpus import topology
from observe_video_faults import observe_wire as observe_faults
from observe_vp9_pilot import correlate, elementary, require, resolve_dependencies


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("codec", choices=("AV1", "H264", "VP8", "VP9"))
    parser.add_argument("inventory", type=Path)
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    with args.inventory.open() as inventory:
        modes = {row["mode"] for row in csv.DictReader(inventory, delimiter="\t")
                 if row["codec"] == args.codec and row["wire"] == f"{args.root.name}/{row['mode']}/wire.bin"}
    require(modes and modes == {path.name for path in args.root.iterdir() if path.is_dir()},
            "inventory and retained mode directories differ")
    checksums = {}
    for line in (args.root / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        require(name not in checksums, f"duplicate checksum: {name}")
        checksums[name] = digest
    filenames = {"context.tsv", "encoded.tsv", "observations.json", "wire.bin", "wire.tsv"}
    filenames.add("faults.json")
    require(set(checksums) == {f"{mode}/{name}" for mode in modes for name in filenames},
            "manifest and expected file set differ")
    for mode in sorted(modes):
        folder = args.root / mode
        for name in filenames:
            require(hashlib.sha256((folder / name).read_bytes()).hexdigest() == checksums[f"{mode}/{name}"],
                    f"changed evidence: {mode}/{name}")
        obs = json.loads((folder / "observations.json").read_text())
        faults = json.loads((folder / "faults.json").read_text())
        if args.codec != "VP9":
            require(obs["context"]["format"] == f"{args.codec}/90000", f"incorrect codec: {mode}")
            if args.codec == "AV1":
                capture = correlate_av1(folder, retained=obs["wire_units"])
                _, units, mapping, wire = capture
                observe_av1(folder, retained=obs, capture=capture)
            else:
                capture = correlate_single(folder, retained=obs["wire_units"])
                _, _, units, mapping, wire, _ = capture
                observe_single_video(folder, retained=obs, capture=capture)
            dependencies = obs["encoder_dependencies"]
            sizes, temporal = topology(units, dependencies, mode)
            observe_faults(units, dependencies, mapping, wire, sizes, temporal, obs,
                           codec=args.codec, retained=faults)
            continue
        context, units, mapping, wire = correlate(folder, pre_packetization_sha256=obs["pre_packetization_sha256"])
        require(obs["codec"] == "VP9" and obs["requested_mode"] == context["requested_mode"] == mode,
                f"inconsistent capture identity: {mode}")
        dependencies = resolve_dependencies(units)
        require(dependencies == obs["encoder_dependencies"] and mapping == obs["wire_units"],
                f"changed dependency or wire mapping: {mode}")
        sizes, temporal = topology(units, dependencies, mode)
        observe_faults(units, dependencies, mapping, wire, sizes, temporal, obs, retained=faults)
        targets = {(item["spatial"], item["temporal"]): item for item in obs["fixed_targets"]}
        require(len(obs["fixed_targets"]) == len(targets)
                and set(targets) == {(s, t) for s in range(len(sizes)) for t in range(temporal)},
                f"incomplete fixed target observations: {mode}")
        received = [(row, data, item) for (row, _), data, item in zip(units, wire, mapping, strict=True)]
        for target, observation in targets.items():
            sid, tid = target
            size = sizes[sid]
            ivf = elementary(received, dependencies, target=target, dimensions=size)
            require(hashlib.sha256(ivf).hexdigest() == observation["ivf_sha256"],
                    f"changed decoder input: {mode} {target}")
            timestamps = [int(row["input_rtp"]) for row, _ in units
                          if int(row["spatial"]) == sid and int(row["temporal"]) <= tid]
            require([frame["pts"] for frame in observation["pictures"]] == timestamps
                    and all((f["width"], f["height"]) == size and f["time_base"] == "1/90000"
                            for f in observation["pictures"]), f"changed presentation: {mode} {target}")


if __name__ == "__main__":
    main()
