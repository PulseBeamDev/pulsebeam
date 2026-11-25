"""Check projected RTP with the pinned WebRTC assembler and FFmpeg decoders."""

import argparse
from collections import defaultdict
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

if __name__ == "__main__" and sys.argv[1:2] == ["--python"]:
    python = str(Path(sys.argv[2]).resolve())
    os.execv(python, [python, __file__, *sys.argv[3:]])

import av

sys.path.insert(0, str(Path(__file__).parent))
from corpus_inventory_test import inspect_data_receipts
from observe_opus_rtp import decode as decode_opus
from observe_single_video import decode_units
from observe_vp9_pilot import decode, indexed_bytes, superframe, target_units, vp9_ivf, write_ivf


def check(condition, message):
    if not condition:
        raise ValueError(message)


def frame_units(receive, codec, rtp, output, encoded, wire, selected, edges, loss=None):
    assembled = subprocess.run([receive, codec, rtp, output], check=True, text=True,
                               stdout=subprocess.PIPE)
    frames = list(csv.DictReader(io.StringIO(assembled.stdout), delimiter="\t"))
    payloads = output.read_bytes()
    check(frames and selected, "no RTP frames assembled")
    id_delta = int(frames[0]["id"]) - wire[selected[0]]["dd"]
    units = {(wire[i]["dd"] + id_delta) & 65535: i for i in selected}
    ancestors = []
    for refs in edges:
        ancestors.append(set(refs).union(*(ancestors[ref] for ref in refs)))
    accepted = []
    released = set()
    safe_units = set(loss["units"]) if loss is not None else None
    for frame in frames:
        unit = units[int(frame["id"]) & 65535]
        check(not accepted or unit > accepted[-1], f"duplicate or out-of-order unit {unit}")
        check(int(frame["rtp_timestamp"]) == wire[unit]["rtp_timestamp"], f"unit {unit} presentation differs")
        parents = {units[int(ref) & 65535] for ref in frame["references"].split(",") if ref}
        observed = parents.union(*(ancestors[parent] for parent in parents))
        check(observed == ancestors[unit],
              f"unit {unit} WebRTC ancestry {sorted(observed)} != {sorted(ancestors[unit])}")
        accepted.append(unit)
        if loss is not None:
            check(unit != loss["lost_unit"], "WebRTC assembled a partially received frame")
            # Assembly yields complete frames before the receiver resolves their references.
            if not parents <= released:
                continue
            check(unit in safe_units, f"receiver released unit {unit} with a lost ancestor")
        released.add(unit)
        offset, size = int(frame["offset"]), int(frame["size"])
        check(0 < size and 0 <= offset <= len(payloads) - size, f"unit {unit} invalid assembled length")
        yield int(encoded[unit]["input_rtp"]), payloads[offset:offset + size]
    if loss is None:
        check(accepted == selected, f"WebRTC assembled {len(accepted)} of {len(selected)} expected frames")


def pixels(codec, units, dimensions):
    if codec in ("VP8", "H264"):
        rows, payloads = zip(*(({"input_rtp": pts}, payload) for pts, payload in units), strict=True)
        return decode_units(av, codec, rows, payloads, range(len(rows)))
    by_pts = defaultdict(list)
    for pts, payload in units:
        by_pts[pts].append(payload)
    if codec == "AV1":
        ivf = write_ivf(b"AV01", dimensions, {pts: b"".join(parts) for pts, parts in by_pts.items()})
        outcome = decode(av, ivf, decoder="libdav1d")
    else:
        ivf = write_ivf(b"VP90", dimensions, {pts: superframe(parts) for pts, parts in by_pts.items()})
        outcome = decode(av, ivf)
    check(outcome["error"] is None and not outcome["decoder_errors"], f"decode failed: {outcome['decoder_errors']}")
    return outcome["frames"]


def for_target(receive, output, rtp, codec, rows, observed, selected, expected, *, restart=None, loss=None):
    first = (expected[0]["width"], expected[0]["height"])
    last = (expected[-1]["width"], expected[-1]["height"])
    units = frame_units(receive, codec, rtp, output, rows, observed["wire_units"], selected,
                        observed["encoder_dependencies"], loss)
    actual = [f for f in pixels(codec, units, last) if (f["width"], f["height"]) ==
              (first if restart is not None and f["pts"] < restart else last)]
    if loss is not None:
        by_pts = {f["pts"]: f for f in expected}
        check(bool(actual) and actual[0]["pts"] == expected[0]["pts"], "no intact prefix before loss")
        check(all(f == by_pts.get(f["pts"]) for f in actual), "decoded a non-intact picture after loss")
        actual = [f for f in actual if f["pts"] >= restart]
        expected = [f for f in expected if f["pts"] >= restart]
        check(bool(expected), "missing independent post-loss progress witness")
    check(actual == expected,
          f"decoded pictures differ ({len(actual)} vs {len(expected)}), first mismatch: "
          f"{next(((have, want) for have, want in zip(actual, expected) if have != want), None)}")


def fixed_targets(rows, observed, folder):
    if "fixed_targets" in observed:
        yield from observed["fixed_targets"]
    else:
        raw = list(indexed_bytes(folder, "encoded"))
        wire = list(zip(rows, (payload for _, payload in raw), observed["wire_units"], strict=True))
        for spatial in (0, 1):
            layer = next(row for row in rows if int(row["spatial"]) == spatial)
            dimensions = int(layer["width"]), int(layer["height"])
            selected = target_units(rows, observed["encoder_dependencies"], (spatial, 0))
            outcome = decode(av, vp9_ivf(wire, selected, dimensions))
            check(outcome["error"] is None and not outcome["decoder_errors"], "pilot reference decode failed")
            yield {"spatial": spatial, "temporal": 0,
                   "pictures": [f for f in outcome["frames"] if (f["width"], f["height"]) == dimensions]}


FAMILY_TRACES = {"AV1": "L2T1", "H264": "L1T2", "VP8": "L1T2", "VP9": "L2T2"}


def inspect_opus(work, fixtures, sources):
    reference = json.loads((fixtures / "opus-v1/rtp.json").read_text())
    original = json.loads((fixtures / "opus-v1/observations.json").read_text())
    packet_facts = {source["file"]: source["packets"] for source in original["sources"]}
    files = set()
    for source in reference["sources"]:
        source_name = source["source"]
        name = f"OPUS-{Path(source_name).stem}.rtp"
        files.add(name)
        data = memoryview((work / name).read_bytes())
        media = []
        while data:
            size, = struct.unpack_from("!H", data)
            check(12 < size <= 1147 and len(data) >= 2 + size, "invalid Opus RTP length")
            packet = data[2:2 + size]
            data = data[2 + size:]
            header, pt, seq, ts, ssrc = struct.unpack_from("!BBHII", packet)
            i = len(media)
            check(header == 0x80 and pt & 127 == 111 and ssrc == 0x506c7363 and seq == (65532 + i) & 65535,
                  "Opus RTP binding differs")
            check(ts == source["wire_units"][i]["timestamp"], "Opus packet duration or presentation differs")
            media.append(bytes(packet[12:]))
        facts = packet_facts[source_name][:len(media)]
        check(len(media) == len(source["wire_units"]) and all(
            len(p) == f["bytes"] and f["duration"] == 960 and hashlib.sha256(p).hexdigest() == f["sha256"]
            for p, f in zip(media, facts, strict=True)), "Opus payload, size or cadence differs")
        with av.open(str(sources / source_name), format="ogg") as original_ogg:
            stream, = original_ogg.streams.audio
            extra = stream.codec_context.extradata
        check(decode_opus(av, media, extra) == source["intact"], f"{source_name}: Opus decoded samples differ")
    return files


def check_data_v1(args, work, reference):
    actual = work / "data-receipts.json"
    subprocess.run([args.data_export, "--runner", args.sim_runner, "--sim-binary", args.sim_binary,
                    "--candidate", args.sim_candidate, "--out", actual], check=True)
    inspect_data_receipts(actual)
    check(json.loads(actual.read_text()) == json.loads(reference.read_text()),
          "live mixed-carrier DATA_V1 receipts differ from the reference")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("export", type=Path)
    parser.add_argument("receive", type=Path)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("data_export", type=Path)
    parser.add_argument("sim_runner", type=Path)
    parser.add_argument("sim_binary", type=Path)
    parser.add_argument("sim_candidate", type=Path)
    parser.add_argument("codec", nargs="?")
    parser.add_argument("mode", nargs="?")
    args = parser.parse_args()
    with args.inventory.open() as source:
        cases = [r for r in csv.DictReader(source, delimiter="\t")
                 if r["mode"] != "-" and r["wire"] != "missing"]
    check(len(cases) == 66, "unexpected independent qualification inventory")
    export = [args.export.resolve()]
    av.logging.set_level(av.logging.ERROR)
    with TemporaryDirectory(prefix="codec-interop-") as temp:
        work = Path(temp)
        selectors = [value for value in (args.codec, args.mode) if value is not None]
        subprocess.run([*export, work, *selectors], check=True)
        expected_files = set()
        if args.codec is None:
            expected_files.update(inspect_opus(work, args.inventory.parent, args.inventory.parent.parent / "src"))
            check_data_v1(args, work, args.inventory.parent / "data-v1/receipts.json")
        for case in cases:
            codec, mode = case["codec"], case["mode"]
            if (args.codec and codec != args.codec) or (args.mode and mode != args.mode):
                continue
            observed = json.loads((args.inventory.parent / case["fixed_target"]).read_text())
            folder = (args.inventory.parent / case["wire"]).parent
            with (args.inventory.parent / case["sender"]).open() as source:
                rows = list(csv.DictReader(source, delimiter="\t"))
            if FAMILY_TRACES[codec] == mode:
                recipe = json.loads((args.inventory.parent / case["loss"]).read_text())
                spatial, temporal = recipe["target"]
                for scenario in ("switch", "loss"):
                    name = f"{codec}-{mode}-{scenario}.rtp"
                    expected_files.add(name)
                    witness = recipe[scenario]
                    selected = witness["units"] if scenario == "switch" else target_units(
                        rows, observed["encoder_dependencies"], (spatial, temporal))
                    try:
                        for_target(args.receive.resolve(), work / "assembled.bin", work / name, codec, rows,
                                   observed, selected, witness["pictures"], restart=recipe["restart_rtp"],
                                   loss=witness if scenario == "loss" else None)
                    except Exception as error:
                        raise ValueError(f"{codec} {mode} {scenario} {spatial}/{temporal}: {error}") from error
            for target in fixed_targets(rows, observed, folder):
                spatial, temporal = target.get("spatial", 0), target["temporal"]
                name = f"{codec}-{mode}-fixed-{spatial}-{temporal}.rtp"
                expected_files.add(name)
                selected = target_units(rows, observed["encoder_dependencies"], (spatial, temporal))
                try:
                    for_target(args.receive.resolve(), work / "assembled.bin", work / name, codec, rows,
                               observed, selected, target["pictures"])
                except Exception as error:
                    raise ValueError(f"{codec} {mode} fixed target {spatial}/{temporal}: {error}") from error
        check(expected_files and expected_files == {p.name for p in work.glob("*.rtp")},
              "missing or unexpected projected qualification streams")
        print(f"Verified {len(expected_files)} RTP outputs with pinned receivers and decoders")


if __name__ == "__main__":
    main()
