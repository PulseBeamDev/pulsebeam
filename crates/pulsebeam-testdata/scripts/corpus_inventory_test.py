#!/usr/bin/env python3
"""Require complete, case-scoped evidence for the six-codec inventory."""

import argparse
import csv
import hashlib
import json
from pathlib import Path


EVIDENCE = ("sender", "wire", "fixed_target", "switch", "loss")
TEMPORAL = {f"L1T{t}" for t in (1, 2, 3)}
LAYERED = TEMPORAL | {
    f"{family}{s}T{t}{ratio}"
    for family in ("L", "S") for s in (2, 3) for t in (1, 2, 3) for ratio in ("", "h")
} | {
    f"L{s}T{t}_KEY{phase}"
    for s, t in ((2, 2), (2, 3), (3, 1), (3, 2), (3, 3))
    for phase in (("",) if t == 1 else ("", "_SHIFT"))
}
VIDEO = {"AV1": LAYERED, "VP9": LAYERED, "H264": TEMPORAL, "VP8": TEMPORAL}
EXPECTED = {(codec, mode) for codec, modes in VIDEO.items() for mode in modes} | {
    ("OPUS", "-"), ("DATA_V1", "-"),
}
NOT_APPLICABLE = {"OPUS": {"switch"}, "DATA_V1": {"fixed_target", "switch"}}
DEFERRED_VIDEO = {
    ("AV1", "S3T1"), ("AV1", "S3T1h"), ("AV1", "S3T2"),
    ("AV1", "S3T2h"), ("AV1", "S3T3"), ("AV1", "S3T3h"),
    *{
        (codec, mode)
        for codec in ("AV1", "VP9")
        for mode in ("L2T3_KEY_SHIFT", "L3T2_KEY_SHIFT", "L3T3_KEY_SHIFT")
    },
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def case_directory(codec, mode):
    if (codec, mode) in (("VP9", "L2T1"), ("VP9", "S2T1")):
        return Path("vp9-pilot", mode)
    root = Path("data-v1" if codec == "DATA_V1" else f"{codec.lower()}-v1")
    return root / mode if codec in VIDEO else root


def inspect_data_receipts(path):
    evidence = json.loads(path.read_text())
    require(evidence["version"] == 1, "unexpected DATA_V1 receipt format")
    cases = {case["delivery"]: case for case in evidence["cases"]}
    require(len(evidence["cases"]) == 3 and set(cases) == {"Latest", "Reliable", "Unreliable"},
            "DATA_V1 needs one mixed-carrier receipt per delivery mode")
    for case in cases.values():
        require(bytes(case["quic_to_sctp"]) == b"QUIC to SCTP"
                and bytes(case["sctp_to_quic"]) == b"SCTP to QUIC", "DATA_V1 peer payload differs")
    repaired = {"unit_seq": 0, "repaired": True}
    unrepaired = {"unit_seq": 0, "repaired": False}
    require((cases["Reliable"]["publisher_loss"], cases["Reliable"]["subscriber_loss"]) == (repaired, repaired)
            and (cases["Unreliable"]["publisher_loss"], cases["Unreliable"]["subscriber_loss"]) == (unrepaired, None)
            and (cases["Latest"]["publisher_loss"], cases["Latest"]["subscriber_loss"]) == (None, None),
            "DATA_V1 loss observations differ from the mixed-carrier fault cases")


def inspect(inventory, source_dir):
    with inventory.open() as source:
        rows = csv.DictReader(source, delimiter="\t")
        require(rows.fieldnames == ["codec", "mode", *EVIDENCE, "gap"], "invalid inventory schema")
        indexed = {}
        for row in rows:
            require(None not in row and all(value is not None for value in row.values()), "malformed inventory row")
            pair = row["codec"], row["mode"]
            require(pair not in indexed, f"duplicate inventory row: {pair}")
            indexed[pair] = row
    require(indexed.keys() == EXPECTED,
            f"inventory differs: missing {sorted(EXPECTED - indexed.keys())}; unexpected {sorted(indexed.keys() - EXPECTED)}")

    deferred = []
    gaps = []
    wire_digests = {}
    for (codec, mode), row in indexed.items():
        label = f"{codec} {mode}"
        if (codec, mode) in DEFERRED_VIDEO:
            require(all(row[field] == "missing" for field in EVIDENCE) and row["gap"].strip(),
                    f"{label}: deferred row must identify the evidence gap, not claim qualification")
            deferred.append(f"{label}: {row['gap']}")
            continue
        case = case_directory(codec, mode)
        not_applicable = {"switch"} if codec in VIDEO and mode == "L1T1" else NOT_APPLICABLE.get(codec, ())
        for field in EVIDENCE:
            value = row[field]
            if field in not_applicable:
                require(value == "n/a", f"{label}: {field} must be n/a")
            elif value != "missing":
                path = Path(value)
                require(path.parent == case and path.name not in ("", ".", ".."),
                        f"{label}: {field} points outside its case")
                if codec in VIDEO and field in ("sender", "wire", "fixed_target"):
                    expected_name = {"sender": "encoded.tsv", "wire": "wire.bin",
                                     "fixed_target": "observations.json"}[field]
                    require(path.name == expected_name, f"{label}: {field} must reference {expected_name}")
                require((inventory.parent / path).is_file(), f"{label}: missing file for {field}: {value}")
        absent = [field for field in EVIDENCE if row[field] == "missing"]
        if absent:
            require(row["gap"].strip(), f"{label}: missing evidence must explain the gap")
            gaps.append(f"{label}: {', '.join(absent)} ({row['gap']})")
        else:
            require(not row["gap"], f"{label}: complete evidence must not claim gaps")
        if codec in VIDEO and row["wire"] != "missing":
            wire = inventory.parent / case / "wire.bin"
            digest = hashlib.sha256(wire.read_bytes()).hexdigest()
            require(digest not in wire_digests, f"{label}: duplicate wire payload from {wire_digests.get(digest)}")
            wire_digests[digest] = label

    opus = indexed["OPUS", "-"]["fixed_target"]
    if opus != "missing":
        observations = json.loads((inventory.parent / opus).read_text())["sources"]
        require([item["file"] for item in observations] == [f"quality_a{i}_48k_mono.opus" for i in (0, 1)],
                "Opus evidence does not cover both sources")
        for item in observations:
            require(hashlib.sha256((source_dir / item["file"]).read_bytes()).hexdigest() == item["source_sha256"],
                    f"Opus source digest differs: {item['file']}")
            require(item["time_base"] == "1/48000" and item["samples"] == 288000
                    and item["max_sample_error"] <= 1 and len(item["packets"]) >= 300,
                    f"incomplete Opus packet/decode evidence: {item['file']}")
    data = indexed["DATA_V1", "-"]
    if data["wire"] != "missing":
        require(data["sender"] == data["wire"] == data["loss"] == "data-v1/receipts.json",
                "DATA_V1 observations must point to the reviewed mixed-carrier receipts")
        inspect_data_receipts(inventory.parent / data["wire"])
    return deferred, gaps, len(wire_digests)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("source_dir", type=Path)
    args = parser.parse_args()
    deferred, gaps, video_count = inspect(args.inventory, args.source_dir)
    print(f"{video_count} of 66 required video pairs have retained wire evidence.")
    print("\nDeferred (unqualified):\n" + "\n".join(sorted(deferred)))
    if gaps:
        print("\nIncomplete required evidence:\n" + "\n".join(gaps))
        return 1
    require(len(deferred) == 12 and video_count == 66, "unexpected corpus qualification scope")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
