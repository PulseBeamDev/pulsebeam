#!/usr/bin/env python3
"""Correlate wire VP9/DD units with pre-packetization observations, then decode."""

import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import struct
import tarfile


def require(condition, message):
    if not condition:
        raise ValueError(message)


def indexed_bytes(directory, name):
    data = (directory / f"{name}.bin").read_bytes()
    offset = 0
    with (directory / f"{name}.tsv").open() as index:
        for row in csv.DictReader(index, delimiter="\t"):
            size = int(row["size"])
            require(int(row["offset"]) == offset and 0 < size <= len(data) - offset,
                    f"invalid {name} byte range")
            yield row, data[offset:offset + size]
            offset += size
    require(offset == len(data), f"unindexed {name} bytes")


def vp9_payload(payload):
    # RFC 9628 sections 4.2 and 4.3. The pilot always sends I and L.
    flags, pic = payload[:2]
    require(flags & 0xA0 == 0xA0, "VP9 packet missing I or L")
    pos = 2
    if pic & 0x80:
        pic = ((pic & 0x7F) << 8) | payload[pos]
        pos += 1
    layer = payload[pos]
    pos += 1
    if not flags & 0x10:
        pos += 1  # TL0PICIDX
    elif flags & 0x40:
        for _ in range(3):
            ref = payload[pos]
            pos += 1
            if not ref & 1:
                break
        else:
            raise ValueError("VP9 reference list exceeds three entries")
    if flags & 0x02:
        ss = payload[pos]
        pos += 1
        if ss & 0x10:
            pos += ((ss >> 5) + 1) * 4
        if ss & 0x08:
            groups = payload[pos]
            pos += 1
            for _ in range(groups):
                pos += 1 + ((payload[pos] >> 2) & 3)
    require(pos < len(payload), "empty or truncated VP9 payload")
    return (pic, (layer >> 1) & 7), bool(flags & 8), bool(flags & 4), payload[pos:]


def rtp_fields(packet, dd_id):
    require(len(packet) >= 16 and packet[0] & 0xF0 == 0x90,
            "expected unpadded RTP v2 with extensions")
    pt = packet[1] & 0x7F
    seq, ts, ssrc = struct.unpack_from("!HII", packet, 2)
    start = 12 + (packet[0] & 15) * 4
    profile, words = struct.unpack_from("!HH", packet, start)
    end = start + 4 + words * 4
    require(end < len(packet), "RTP extension exceeds packet")
    pos = start + 4
    dd = None
    while pos < end:
        header = packet[pos]
        pos += 1
        if not header:
            continue
        if profile == 0xBEDE:
            ext, size = header >> 4, (header & 15) + 1
            require(ext not in (0, 15), "invalid one-byte RTP extension ID")
        else:
            require(profile & 0xFFF0 == 0x1000 and pos < end,
                    "invalid two-byte RTP extension")
            ext, size = header, packet[pos]
            pos += 1
        require(pos + size <= end, "RTP extension length exceeds block")
        if ext == dd_id:
            require(dd is None and size >= 3, "invalid mandatory DD prefix")
            dd = packet[pos:pos + size]
        pos += size
    require(dd is not None, "missing DD")
    return pt, seq, ts, ssrc, dd, packet[end:]


def correlate(directory):
    context = dict(line.split("\t", 1) for line in
                   (directory / "context.tsv").read_text().splitlines())
    require(context["format"] == "VP9/90000" and context["profile_id"] == "0",
            "unexpected payload format")
    units = list(indexed_bytes(directory, "encoded"))
    active = None
    last_seq = last_dd = None
    by_unit = []
    wire_units = []
    sr = False
    last_us = -1
    for index, (row, packet) in enumerate(indexed_bytes(directory, "wire")):
        elapsed_us = int(row["relative_us"])
        require(elapsed_us >= last_us, "wire capture time went backwards")
        last_us = elapsed_us
        if row["kind"] == "rtcp":
            sr |= len(packet) >= 28 and packet[1] == 200
            continue
        require(row["kind"] == "rtp", "unknown wire record")
        pt, seq, ts, ssrc, dd, payload = rtp_fields(packet, int(context["dd_id"]))
        require((pt, ssrc) == (int(context["pt"]), int(context["ssrc"])),
                "wire crossed the single primary binding")
        require(len(packet) <= int(context["max_packet_bytes"]), "oversize RTP")
        require(last_seq is None or seq == (last_seq + 1) % 65536, "RTP sequence gap")
        last_seq = seq
        vp9_id, start, end, media = vp9_payload(payload)
        dd_id = int.from_bytes(dd[1:3], "big")
        require((start, end) == (bool(dd[0] & 0x80), bool(dd[0] & 0x40)),
                "VP9 and DD fragment boundaries differ")
        if start:
            require(active is None and (last_dd is None or dd_id == (last_dd + 1) % 65536),
                    "overlapping frame or nonconsecutive DD frame numbers")
            active = (vp9_id, dd_id, ts, bytearray(), [])
            last_dd = dd_id
        require(active is not None and active[:3] == (vp9_id, dd_id, ts),
                "RTP fragment associated with wrong frame")
        active[3].extend(media)
        active[4].append(index)
        if end:
            require(len(by_unit) < len(units), "unobserved RTP unit")
            meta, expected = units[len(by_unit)]
            require(int(meta["unit"]) == len(by_unit) and int(meta["spatial"]) == vp9_id[1]
                    and ts == (int(meta["input_rtp"]) + int(context["rtp_offset"])) % 2**32
                    and active[3] == expected, "wire-to-encoder unit mismatch")
            by_unit.append({"unit": len(by_unit), "dd": dd_id, "picture": vp9_id[0],
                            "rtp_timestamp": ts, "packets": active[4]})
            wire_units.append(bytes(active[3]))
            active = None
    require(active is None and len(by_unit) == len(units) == 24, "missing complete units")
    require(sr and any(len(unit["packets"]) > 1 for unit in by_unit),
            "capture needs a sender report and fragmented media")
    return context, units, by_unit, wire_units


def inspect_encoder(units, mapping, requested):
    dependencies = []
    buffers = {}
    for index, ((row, _), wire) in enumerate(zip(units, mapping, strict=True)):
        sid = index % 2
        require((int(row["spatial"]), int(row["temporal"]), row["mode"]) == (sid, 0, requested),
                "unexpected callback topology")
        require((int(row["width"]), int(row["height"])) == (320 << sid, 180 << sid),
                "unexpected spatial resolution")
        require(int(row["end_of_picture"]) == sid, "unexpected temporal unit boundary")
        if sid:
            prior = mapping[index - 1]
            require(wire["picture"] == prior["picture"]
                    and wire["rtp_timestamp"] == prior["rtp_timestamp"],
                    "spatial units are not at the same presentation instant")
        edges = []
        updates = []
        for fact in filter(None, row["buffers_id_ref_update"].split(",")):
            slot, ref, update = map(int, fact.split(":"))
            if ref:
                require(slot in buffers, "uninitialized encoder buffer reference")
                edges.append(buffers[slot])
            if update:
                updates.append(slot)
        same_picture = sid and index - 1 in edges
        require(bool(same_picture) == (row["inter_layer_predicted"] == "1"),
                "VP9 inter-layer flag contradicts encoder buffer observations")
        require(bool(same_picture) == (sid == 1 and requested == "L2T1"),
                "independence/dependence was not witnessed")
        if requested == "S2T1":
            require(all(edge % 2 == sid for edge in edges), "independent stream crossed layers")
        dependencies.append(sorted(set(edges)))
        for slot in updates:
            buffers[slot] = index
    return dependencies


def superframe(parts):
    if len(parts) == 1:
        return parts[0]
    size_bytes = (max(map(len, parts)).bit_length() + 7) // 8
    marker = bytes([0xC0 | ((size_bytes - 1) << 3) | (len(parts) - 1)])
    sizes = b"".join(len(part).to_bytes(size_bytes, "little") for part in parts)
    return b"".join(parts) + marker + sizes + marker


def elementary(units, dependencies, *, absent_packets=(), corrupt=None):
    # Fixed upper target spanning the whole capture. Select only the transitive
    # closure of upper units using encoder buffer events, never wire DD.
    selected = set(range(1, len(units), 2))
    for unit in reversed(range(len(units))):
        if unit in selected:
            selected.update(dependencies[unit])
    packets = {}
    for unit in sorted(selected):
        row, data, wire = units[unit]
        if set(wire["packets"]).intersection(absent_packets):
            continue  # Never offer a partial coded unit to the decoder.
        if unit == corrupt:
            data = b"\x00" * len(data)
        pts = int(row["input_rtp"])
        packets.setdefault(pts, []).append(data)
    out = io.BytesIO()
    out.write(struct.pack("<4sHH4sHHIIII", b"DKIF", 0, 32, b"VP90",
                          640, 360, 90000, 1, len(packets), 0))
    for pts, parts in packets.items():
        payload = superframe(parts)
        out.write(struct.pack("<IQ", len(payload), pts))
        out.write(payload)
    return out.getvalue()


def picture(frame):
    require(frame.format.name == "yuv420p", "unexpected VP9 decoded sample format")
    digests = []
    for plane in frame.planes:
        digest = hashlib.sha256()
        data = memoryview(plane)
        for row in range(plane.height):
            offset = row * plane.line_size
            digest.update(data[offset:offset + plane.width])
        digests.append(digest.hexdigest())
    return {"pts": frame.pts, "time_base": str(frame.time_base),
            "width": frame.width, "height": frame.height, "planes_sha256": digests}


def decode(av, ivf):
    # Observe AVFrames before any scaler or fixed-resolution output encoder.
    # Both spatial pictures can be displayed at one PTS.
    options = {"threads": "1", "err_detect": "explode"}
    frames = []
    error = None
    with av.logging.Capture() as log, av.open(io.BytesIO(ivf), format="ivf") as source:
        stream = source.streams.video[0]
        require(stream.codec_context.name == "vp9", "unexpected decoder")
        stream.codec_context.options = options.copy()
        try:
            for packet in source.demux(stream):
                frames.extend(picture(frame) for frame in packet.decode())
        except av.error.InvalidDataError as exc:
            error = exc.errno
    errors = [message.strip() for level, component, message in log
              if level <= av.logging.ERROR and component == "vp9"]
    return {"error": error, "decoder_errors": errors, "frames": frames, "options": options}


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", required=True, type=Path)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    version = {"pyav": av.__version__, "ffmpeg_libraries": av.library_versions}
    for mode in ("S2T1", "L2T1"):
        folder = args.capture / mode
        context, units, mapping, wire_units = correlate(folder)
        require(mode == context["requested_mode"], "incorrect capture label")
        references = inspect_encoder(units, mapping, mode)
        wire_input = [(row, data, wire)
                      for (row, _), data, wire in zip(units, wire_units, mapping, strict=True)]
        lost_packets = mapping[0]["packets"]
        streams = {
            "intact": elementary(units=[(row, data, wire) for (row, data), wire in
                                       zip(units, mapping, strict=True)], dependencies=references),
            "lost_lower": elementary(wire_input, references, absent_packets=lost_packets),
            "corrupt_retained": elementary(wire_input, references, corrupt=1),
        }
        result = {"encoder_dependencies": references, "wire_units": mapping,
                  "decoder_build": version, "lost_unit": 0, "lost_packets": lost_packets,
                  "corrupt_unit": 1}
        for name, ivf in streams.items():
            result[name] = {"ivf_sha256": hashlib.sha256(ivf).hexdigest(), **decode(av, ivf)}
        (folder / "observations.json").write_text(json.dumps(result, indent=2) + "\n")
        intact = result["intact"]
        lost = result["lost_lower"]
        corrupt = result["corrupt_retained"]
        require(intact["error"] is None and not intact["decoder_errors"],
                f"{mode} intact decode failed: {intact['decoder_errors']}")
        upper = [frame for frame in intact["frames"] if (frame["width"], frame["height"]) == (640, 360)]
        require([frame["pts"] for frame in upper] == [int(row["input_rtp"]) for row, _ in units[1::2]]
                and all(frame["time_base"] == "1/90000" for frame in upper),
                f"{mode} incomplete upper decode: {intact['frames']}")
        require(corrupt["error"] is not None, f"{mode} strict decoder accepted corrupt unit")
        if mode == "S2T1":
            require(lost == intact, "independent upper decode changed after lower loss")
        else:
            require(lost["error"] is not None
                    and any("references are available" in message for message in lost["decoder_errors"]),
                    f"dependent decode did not reject a missing reference: {lost}")
            require(not any(frame["pts"] == upper[0]["pts"] and frame["width"] == 640
                            for frame in lost["frames"]), "dependent upper was presented despite loss")

    if args.archive:
        with tarfile.open(args.archive, "w", format=tarfile.USTAR_FORMAT) as archive:
            for path in sorted(args.capture.glob("*/*")):
                data = path.read_bytes()
                entry = tarfile.TarInfo(path.relative_to(args.capture).as_posix())
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))


if __name__ == "__main__":
    main()
