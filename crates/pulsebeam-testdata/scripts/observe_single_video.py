#!/usr/bin/env python3
"""Correlate pinned VP8 and H.264 sender RTP and decode its temporal targets."""

import argparse
import csv
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import re

from observe_vp9_corpus import topology
from observe_vp9_pilot import indexed_bytes, picture, require, resolve_dependencies, rtp_fields, target_units


def vp8_media(payload, start, tid):
    header = payload[0]
    require(bool(header & 0x10) == start and not header & 0x47, "unexpected VP8 partition descriptor")
    pos = 1
    if header & 0x80:
        extension = payload[pos]
        pos += 1
        if extension & 0x80:
            pos += 2 if payload[pos] & 0x80 else 1
        if extension & 0x40:
            pos += 1
        if extension & 0x30:
            if extension & 0x20:
                require(payload[pos] >> 6 == tid, "VP8 wire temporal ID mismatch")
            pos += 1
    require(pos < len(payload), "empty VP8 coded fragment")
    return payload[pos:]


def annex_b(data):
    nals = re.split(rb"\x00\x00(?:\x00)?\x01", data)
    require(not nals[0] and all(nals[1:]), "invalid H.264 Annex B unit")
    return nals[1:]


def h264_packet(nals, pending, payload):
    kind = payload[0] & 31
    if 1 <= kind <= 23:
        require(not pending, "single NAL inside H.264 fragmented unit")
        nals.append(payload)
    elif kind == 24:
        require(not pending, "STAP-A inside H.264 fragmented unit")
        pos = 1
        while pos < len(payload):
            require(pos + 2 <= len(payload), "truncated H.264 STAP-A length")
            size = int.from_bytes(payload[pos:pos + 2], "big")
            pos += 2
            require(0 < size <= len(payload) - pos, "invalid H.264 STAP-A size")
            nals.append(payload[pos:pos + size])
            pos += size
    elif kind == 28:
        require(len(payload) > 2, "empty H.264 FU-A")
        flags = payload[1]
        start, end = bool(flags & 128), bool(flags & 64)
        require(start == (pending is None) and not (start and end), "invalid H.264 FU-A order")
        header = (payload[0] & 0xe0) | (flags & 31)
        if start:
            pending = bytearray([header])
        require(pending[0] == header, "inconsistent H.264 FU-A type")
        pending.extend(payload[2:])
        if end:
            nals.append(bytes(pending))
            pending = None
    else:
        raise ValueError(f"unexpected H.264 packetization type {kind}")
    return pending


def correlate(folder, retained=None):
    context = dict(line.split("\t", 1) for line in (folder / "context.tsv").read_text().splitlines())
    codec = context["format"].split("/")[0]
    require(codec in ("VP8", "H264") and context["format"] == f"{codec}/90000", "unexpected binding")
    require(context["requested_mode"] == folder.name, "capture name and request disagree")
    if retained is None:
        units = list(indexed_bytes(folder, "encoded"))
    else:
        with (folder / "encoded.tsv").open() as index:
            units = [(row, None) for row in csv.DictReader(index, delimiter="\t")]
    profiles = set()
    mappings, wire, packets, parts = [], [], [], []
    pending = last_seq = last_frame = None
    sr = False
    for packet_index, (entry, packet) in enumerate(indexed_bytes(folder, "wire")):
        if entry["kind"] == "rtcp":
            sr |= len(packet) >= 28 and packet[1] == 200
            continue
        require(entry["kind"] == "rtp", "unknown wire record")
        pt, seq, ts, ssrc, dd, payload = rtp_fields(packet, int(context["dd_id"]))
        require((pt, ssrc) == (int(context["pt"]), int(context["ssrc"]))
                and len(packet) <= int(context["max_packet_bytes"]), "unexpected RTP binding or size")
        require(last_seq is None or seq == (last_seq + 1) % 65536, "RTP sequence gap")
        last_seq = seq
        start, end = bool(dd[0] & 128), bool(dd[0] & 64)
        frame = int.from_bytes(dd[1:3], "big")
        require(start == (not packets) and len(mappings) < len(units), "wrong DD frame boundary")
        if start:
            require(last_frame is None or frame == (last_frame + 1) % 65536, "DD frame number gap")
            last_frame = frame
        row, expected = units[len(mappings)]
        require(frame == last_frame and ts == (int(row["input_rtp"]) + int(context["rtp_offset"])) % 2**32
                and bool(packet[1] & 128) == end, "wrong RTP frame timestamp, number, or marker")
        packets.append(packet_index)
        if codec == "VP8":
            parts.append(vp8_media(payload, start, int(row["temporal"])))
        else:
            pending = h264_packet(parts, pending, payload)
        if end:
            require(pending is None, "missing final H.264 fragment")
            require(int(row["unit"]) == len(mappings), "encoder unit order differs")
            if codec == "VP8":
                data = b"".join(parts)
                profiles.add(str((data[0] >> 1) & 7))
                if retained is None:
                    require(data == expected, "VP8 payload differs from original frame")
            else:
                profiles.update(nal[1:4].hex() for nal in parts if nal[0] & 31 == 7)
                if retained is None:
                    require(parts == annex_b(expected), "H.264 NALs differ from original access unit")
                data = b"".join(b"\x00\x00\x00\x01" + p for p in parts)
            mappings.append({"dd": frame, "rtp_timestamp": ts, "packets": packets,
                             "unit_sha256": hashlib.sha256(data).hexdigest()})
            wire.append(data)
            packets, parts = [], []
    require(sr and len(mappings) == len(units) and not packets
            and any(len(m["packets"]) > 1 for m in mappings), "incomplete capture")
    require(len(profiles) == 1, "missing or inconsistent emitted profile")
    profile = next(iter(profiles))
    if codec == "VP8":
        require(context["profile_id"] == profile, "emitted VP8 profile differs from capture binding")
    else:
        require(context["packetization_mode"] == "1" and len(profile) == 6, "missing H.264 SPS or mode 1")
    if retained is not None:
        require(mappings == retained, "wire units differ from recorded encoder correlation")
    return codec, context, units, mappings, wire, profile


def decode_units(av, codec, rows, wire, selected):
    decoder = av.CodecContext.create(codec.lower(), "r")
    decoder.options = {"threads": "1", "err_detect": "explode"}
    frames = []
    with av.logging.Capture() as log:
        for unit in selected:
            packet = av.Packet(wire[unit])
            packet.pts = packet.dts = int(rows[unit]["input_rtp"])
            packet.time_base = Fraction(1, 90000)
            frames.extend(picture(frame) for frame in decoder.decode(packet))
        frames.extend(picture(frame) for frame in decoder.decode(None))
    require(not any(level <= av.logging.ERROR for level, _, _ in log), f"decode errors: {log}")
    return frames


def observe(folder, av=None, *, retained=None, capture=None):
    if capture is None:
        capture = correlate(folder, None if retained is None else retained["wire_units"])
    codec, config, units, mapping, wire, profile = capture
    dependencies = resolve_dependencies(units)
    dimensions, temporal = topology(units, dependencies, folder.name)
    require(len(dimensions) == 1, "not a single-spatial capture")
    rows = [row for row, _ in units]
    targets = []
    for tid in range(temporal):
        selected = target_units(rows, dependencies, (0, tid))
        if retained is None:
            frames = decode_units(av, codec, rows, wire, selected)
        else:
            frames = retained["fixed_targets"][tid]["pictures"]
        want = [int(row["input_rtp"]) for row in rows if int(row["temporal"]) <= tid]
        require([f["pts"] for f in frames] == want and
                all((f["width"], f["height"]) == dimensions[0] and f["time_base"] == "1/90000" for f in frames),
                f"decoded temporal target {tid} does not match the sender")
        targets.append({"temporal": tid, "units": selected, "pictures": frames})
    build = retained["decoder_build"] if retained is not None else {
        "pyav": av.__version__, "ffmpeg_libraries": av.library_versions}
    result = {"context": config, "emitted_profile": profile, "encoder_dependencies": dependencies,
              "wire_units": mapping, "decoder_build": build, "fixed_targets": targets}
    if retained is not None:
        require(result == retained, "single-spatial observations differ from versioned evidence")
    return result


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", type=Path, nargs="+")
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    for folder in args.captures:
        try:
            result = observe(folder, av)
        except ValueError as exc:
            raise ValueError(f"{folder.parent.name} {folder.name}: {exc}") from exc
        (folder / "observations.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"{folder.parent.name} {folder.name}: {len(result['fixed_targets'])} decoded targets")


if __name__ == "__main__":
    main()
