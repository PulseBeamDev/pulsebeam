#!/usr/bin/env python3
"""Correlate stock WebRTC Opus RTP with the original Ogg packet observations."""

import argparse
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import struct
import subprocess
from tempfile import TemporaryDirectory

from observe_vp9_pilot import indexed_bytes, require, write_archive


PACKETS = 300
STEP = 960
CLOCK = 48000
LOSS_INDEX = 50


def correlate(folder, original):
    context = dict(line.split("\t", 1) for line in (folder / "context.tsv").read_text().splitlines())
    require(context["format"] == "opus/48000/1", "unexpected Opus format")
    payload_type, ssrc, offset = (int(context[k]) for k in ("pt", "ssrc", "rtp_offset"))
    mapping, media, reports = [], [], []
    previous_elapsed = -1
    payload_octets = 0
    for record, (row, packet) in enumerate(indexed_bytes(folder, "wire")):
        elapsed = int(row["relative_us"])
        require(elapsed >= previous_elapsed, "wire time moved backwards")
        previous_elapsed = elapsed
        if row["kind"] == "rtcp":
            require(len(packet) >= 28 and packet[:2] == b"\x80\xc8", "expected an RTCP sender report")
            sender, ntp, timestamp, count, octets = struct.unpack_from("!IQIII", packet, 4)
            require(sender == ssrc and count == len(media) and octets == payload_octets,
                    "Opus sender report counters disagree with wire")
            reports.append({"record": record, "ntp": ntp, "timestamp": timestamp, "packet_count": count})
            continue
        i = len(media)
        require(row["kind"] == "rtp" and i < PACKETS and len(packet) >= 13 and packet[0] == 0x80,
                "expected plain Opus RTP v2")
        seq, timestamp, sender = struct.unpack_from("!HII", packet, 2)
        require((packet[1] & 127, sender) == (payload_type, ssrc)
                and seq == (65510 + i) % 65536 and timestamp == (offset + STEP * i) % 2**32
                and elapsed == i * 20000, "Opus transport binding or cadence differs")
        payload = packet[12:]
        expected = original["packets"][i]
        require(expected["duration"] == STEP and len(payload) == expected["bytes"]
                and hashlib.sha256(payload).hexdigest() == expected["sha256"],
                "RTP payload differs from original Opus packet")
        media.append(payload)
        payload_octets += len(payload)
        mapping.append({"record": record, "sequence": seq, "timestamp": timestamp})
    require(len(media) == PACKETS and len(reports) >= 2, "Opus capture is incomplete")
    return context, media, mapping, reports


def decode(av, media, extradata, *, skip=None):
    decoder = av.CodecContext.create("opus", "r")
    decoder.extradata = extradata
    observations = []
    with av.logging.Capture() as log:
        for i, data in enumerate(media):
            if i == skip:
                continue  # Observe timestamp gaps, not synthesized receiver PLC.
            packet = av.Packet(data)
            packet.pts = packet.dts = i * STEP
            packet.time_base = Fraction(1, CLOCK)
            for frame in decoder.decode(packet):
                require(frame.sample_rate == CLOCK and frame.layout.name == "mono", "unexpected Opus decoded format")
                pcm = bytes(frame.planes[0])[:frame.samples * 4]
                require(frame.format.name == "fltp", "unexpected Opus PCM representation")
                observations.append({"pts": frame.pts, "samples": frame.samples,
                                     "sha256": hashlib.sha256(pcm).hexdigest()})
        require(not decoder.decode(None), "unexpected delayed Opus frames")
    require(not any(level <= av.logging.ERROR for level, _, _ in log), "Opus decode logged an error")
    return observations


def capture(av, runner, source, original, folder):
    folder.mkdir(parents=True)
    packet_input = folder / "packets.bin"
    with av.open(str(source), format="ogg") as container, packet_input.open("wb") as out:
        stream, = container.streams.audio
        extradata = stream.codec_context.extradata
        for i, packet in enumerate(container.demux(stream)):
            if i == PACKETS:
                break
            require(packet.duration == STEP and packet.size, "wrong Opus cadence")
            out.write(struct.pack("<H", packet.size) + bytes(packet))
    subprocess.run([runner, "--opus", packet_input, folder], check=True)
    packet_input.unlink()
    context, media, mapping, reports = correlate(folder, original)
    intact = decode(av, media, extradata)
    loss = decode(av, media, extradata, skip=LOSS_INDEX)
    require(len(intact) == PACKETS and len(loss) == PACKETS - 1 and loss[:LOSS_INDEX] == intact[:LOSS_INDEX]
            and [f["pts"] for f in loss] == [f["pts"] for i, f in enumerate(intact) if i != LOSS_INDEX],
            "loss observation must preserve the exact gap and unaffected prefix")
    return {"source": source.name, "context": context, "wire_units": mapping, "sender_reports": reports,
            "intact": intact, "loss": {"missing": mapping[LOSS_INDEX], "frames": loss}}


def main():
    import av

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", type=Path, required=True)
    parser.add_argument("--observations", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("sources", nargs="+", type=Path)
    args = parser.parse_args()
    av.logging.set_level(av.logging.ERROR)
    references = {r["file"]: r for r in json.loads(args.observations.read_text())["sources"]}
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        cases = [capture(av, args.capture, source, references[source.name], root / "opus-v1" / source.stem)
                 for source in args.sources]
        result = {"decoder_build": {"pyav": av.__version__, "ffmpeg_libraries": av.library_versions},
                  "sources": cases}
        (root / "opus-v1" / "rtp.json").write_text(json.dumps(result, indent=2) + "\n")
        write_archive(args.out, root)


if __name__ == "__main__":
    main()
