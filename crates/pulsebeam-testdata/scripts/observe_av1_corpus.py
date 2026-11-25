#!/usr/bin/env python3
"""Check that the AV1 sender's RTP carries its original coded OBUs."""

import argparse
import csv
import hashlib
import json
from pathlib import Path

from collections import defaultdict

from observe_vp9_corpus import topology
from observe_vp9_pilot import (decode, indexed_bytes, require, rtp_fields,
                               resolve_dependencies, target_units, write_ivf)


def leb128(data, pos):
    value = 0
    for shift in range(0, 56, 7):
        require(pos < len(data), "truncated AV1 length")
        byte = data[pos]
        pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
    raise ValueError("invalid AV1 length")


def source_obus(data):
    pos = 0
    while pos < len(data):
        header = data[pos]
        pos += 1
        require(not header & 0x81, "invalid AV1 OBU header")
        prefix = bytes([header & ~2])
        if header & 4:
            require(pos < len(data), "truncated AV1 OBU extension")
            prefix += data[pos:pos + 1]
            pos += 1
        size, pos = leb128(data, pos) if header & 2 else (len(data) - pos, pos)
        require(pos + size <= len(data), "OBU extends past encoded unit")
        # Upstream RTP packetizer omits temporal delimiters, tile lists and padding.
        if (header >> 3) & 15 not in (2, 8, 15):
            yield prefix + data[pos:pos + size]
        pos += size


def packet_elements(data):
    count = (data[0] >> 4) & 3
    pos, seen = 1, 0
    while pos < len(data):
        seen += 1
        size, pos = (len(data) - pos, pos) if count == seen else leb128(data, pos)
        require(0 < size <= len(data) - pos, "invalid AV1 RTP element length")
        yield data[pos:pos + size]
        pos += size
    require(seen > 0 and (not count or count == seen), "invalid AV1 RTP element count")


def correlate(folder, *, retained=None):
    context = dict(line.split("\t", 1) for line in (folder / "context.tsv").read_text().splitlines())
    require(context["format"] == "AV1/90000" and context["profile_id"] == "0", "unexpected AV1 binding")
    if retained is None:
        units = list(indexed_bytes(folder, "encoded"))
    else:
        with (folder / "encoded.tsv").open() as index:
            units = [(row, None) for row in csv.DictReader(index, delimiter="\t")]
    observations, wire_units, obus, packets = [], [], [], []
    last_seq = last_frame = None
    pending = False
    fragmented = sr = False
    for packet_index, (row, packet) in enumerate(indexed_bytes(folder, "wire")):
        if row["kind"] == "rtcp":
            sr |= len(packet) >= 28 and packet[1] == 200
            continue
        require(row["kind"] == "rtp", "unknown wire record")
        pt, seq, ts, ssrc, dd, payload = rtp_fields(packet, int(context["dd_id"]))
        require((pt, ssrc) == (int(context["pt"]), int(context["ssrc"]))
                and len(packet) <= int(context["max_packet_bytes"]), "unexpected RTP binding or size")
        require(last_seq is None or seq == (last_seq + 1) % 65536, "RTP sequence gap")
        last_seq = seq
        frame = int.from_bytes(dd[1:3], "big")
        start, end = bool(dd[0] & 128), bool(dd[0] & 64)
        require(start == (not packets) and len(observations) < len(units), "unexpected AV1 frame boundary")
        if start:
            require(last_frame is None or frame == (last_frame + 1) % 65536, "DD frame number gap")
            last_frame = frame
        meta, expected = units[len(observations)]
        require(frame == last_frame and ts == (int(meta["input_rtp"]) + int(context["rtp_offset"])) % 2**32,
                "unexpected AV1 frame number or timestamp")
        z, y = bool(payload[0] & 128), bool(payload[0] & 64)
        require(z == pending and not payload[0] & 7, "invalid AV1 OBU continuation or reserved bits")
        elements = list(packet_elements(payload))
        if z:
            obus[-1] += elements.pop(0)
        obus.extend(elements)
        pending = y
        fragmented |= y
        packets.append(packet_index)
        require(bool(packet[1] & 128) == (end and meta["end_of_picture"] == "1"), "AV1 RTP marker mismatch")
        if end:
            require(not pending and int(meta["unit"]) == len(observations), "incomplete AV1 unit")
            if retained is None:
                require(obus == list(source_obus(expected)), "RTP OBUs differ from encoder OBUs")
            observations.append({"dd": frame, "rtp_timestamp": ts, "packets": packets,
                                 "obus_sha256": [hashlib.sha256(obu).hexdigest() for obu in obus]})
            wire_units.append(obus)
            obus, packets = [], []
    require(sr and fragmented and not packets and len(observations) == len(units) and units,
            "capture lacks complete units, fragmentation or a sender report")
    if retained is not None:
        require(observations == retained, "AV1 wire units differ from recorded encoder correlation")
    return context, units, observations, wire_units


def encode_length(size):
    data = bytearray()
    while size >= 128:
        data.append(128 | (size & 127))
        size >>= 7
    data.append(size)
    return bytes(data)


def sized_obu(obu):
    prefix = 2 if obu[0] & 4 else 1
    return bytes([obu[0] | 2]) + obu[1:prefix] + encode_length(len(obu) - prefix) + obu[prefix:]


def av1_ivf(rows, wire, selected, dimensions):
    packets = defaultdict(bytearray)
    for unit in selected:
        pts = int(rows[unit]["input_rtp"])
        packets[pts].extend(b"".join(sized_obu(obu) for obu in wire[unit]))
    return write_ivf(b"AV01", dimensions, packets)


def observe(folder, av=None, *, retained=None, capture=None):
    if capture is None:
        capture = correlate(folder, retained=None if retained is None else retained["wire_units"])
    context, units, observations, wire = capture
    rows = [row for row, _ in units]
    dependencies = resolve_dependencies(units)
    require(context["requested_mode"] == folder.name, "capture name and request disagree")
    sizes, temporal = topology(units, dependencies, folder.name)
    targets = []
    known_targets = {} if retained is None else {
        (item["spatial"], item["temporal"]): item for item in retained["fixed_targets"]}
    for sid, tid in ((s, t) for s in range(len(sizes)) for t in range(temporal)):
        dimensions = sizes[sid]
        want = [int(r["input_rtp"]) for r in rows
                if int(r["spatial"]) == sid and int(r["temporal"]) <= tid]
        ivf = av1_ivf(rows, wire, target_units(rows, dependencies, (sid, tid)), dimensions)
        digest = hashlib.sha256(ivf).hexdigest()
        if retained is None:
            decoded = decode(av, ivf, decoder="libdav1d")
            require(decoded["error"] is None and not decoded["decoder_errors"],
                    f"AV1 S{sid}T{tid} decode failed: {decoded['decoder_errors']}")
            frames = [f for f in decoded["frames"] if (f["width"], f["height"]) == dimensions]
            options = decoded["options"]
        else:
            known = known_targets[sid, tid]
            require(digest == known["ivf_sha256"], f"changed AV1 decoder input: S{sid}T{tid}")
            frames, options = known["pictures"], known["decoder_options"]
        require([f["pts"] for f in frames] == want
                and all((f["width"], f["height"]) == dimensions and f["time_base"] == "1/90000" for f in frames),
                f"AV1 S{sid}T{tid} decoded target pictures differ from sender timestamps or dimensions")
        targets.append({"spatial": sid, "temporal": tid, "pictures": frames,
                        "ivf_sha256": digest, "decoder_options": options})
    build = retained["decoder_build"] if retained is not None else {
        "pyav": av.__version__, "ffmpeg_libraries": av.library_versions, "decoder": "libdav1d"}
    result = {"context": context, "wire_units": observations, "encoder_dependencies": dependencies,
              "decoder_build": build, "fixed_targets": targets}
    if retained is not None:
        require(result == retained, "AV1 observations differ from versioned evidence")
    return result


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    print(json.dumps(observe(args.capture, av), indent=2))


if __name__ == "__main__":
    main()
