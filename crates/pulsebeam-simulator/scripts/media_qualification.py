#!/usr/bin/env python3
"""Expand and check the runtime qualification matrix and its fixture inputs."""

import argparse
import csv
from pathlib import Path


PATHS = ("same-shard", "cross-shard", "direct-lan", "direct-wan", "regional")
PROTECTION = "protected-base"
KEY = ("endpoint", "direction", "codec", "structure", "capability")
ENDPOINTS = {"native-rtc", "native-rtc-encoded-h264", "web-rtc", "linux-quic", "rtc-wire"}
VIDEO = {"AV1", "H264", "VP8", "VP9"}
STATUSES = {"-", "observed", "encoded", "warm", "partial", "gap", "blocked", "candidate", "rejected", "legacy"}
SIM = "//crates/pulsebeam-simulator:media_fast"
CORPUS = "//crates/pulsebeam-testdata:corpus_inventory"
REPLAY = "//agents/pulsebeam-agent-quic:codec_matrix"
PROOFS = {
    "local-h264": f"{SIM} filter=whole_source_fabric_h264_reaches_",
    "native-av1": f"{SIM} filter=native_claims_h264_and_av1_senders_after_join",
    "av1-no-dd": f"{SIM} filters=native_state_av1_decodes_with_adopted_fid,provisioned_av1_warm_whole_without_dd",
    "av1-dd": f"{SIM} filters=native_state_av1_carries_dd_without_selection,provisioned_av1_warm_whole_with_dd",
    "vp9": f"{SIM} filters=quic_spatial_vp9_native_receiver,provisioned_wan_vp9_dd_decodes_and_recovers_after_loss",
    "browser": "//agents/pulsebeam-agent-web:web_runtime_browser native_platform_smoke (live-agent-contract and quic-agent-smoke)",
    "mixed-h264": f"{SIM} filters=quic_media_h264,quic_mixed",
    "opus": f"{SIM} filters=quic_media_opus,provisioned_wan_opus_repairs_exact_position; browser native_platform_smoke",
    "data": f"{SIM} filters=sctp_and_quic_exchange_,local_ordered_,provisioned_wan_,sibling_regions_share_a_wan_branch; browser native_platform_smoke",
    "rtc-admission": "//crates/pulsebeam:unit_tests control::rtp::policy (applied binding and publication check)",
    "native-admission": "//agents/pulsebeam-agent-native:unit_tests native_source (source shape and codec validation)",
    "one-encoding": "cutover: public shape and raw RTC admission in //crates/pulsebeam:unit_tests and SDK owner gates",
    "codecs": f"codecs: {SIM}, {REPLAY} and browser owner smoke where applicable",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def rows(path):
    with path.open(newline="", encoding="utf-8") as file:
        yield from csv.DictReader(file, delimiter="\t")


def matrix(contract, corpus):
    fixtures = {}
    for row in rows(corpus):
        pair = row["codec"], row["mode"]
        require(pair not in fixtures, f"duplicate fixture: {pair}")
        fixtures[pair] = row
    require({codec for codec, _ in fixtures} == VIDEO | {"OPUS", "DATA_V1"}, "unmapped corpus codec")
    cells = {}
    for row in rows(contract):
        require(set(row) == {*KEY, *PATHS, PROTECTION, "evidence"} and None not in row.values(), "invalid contract row")
        evidence = PROOFS[row["evidence"]]
        for endpoint in row["endpoint"].split(","):
            require(endpoint in ENDPOINTS, f"unknown endpoint: {endpoint}")
            for codec in row["codec"].split(","):
                require(codec in VIDEO | {"OPUS", "DATA_V1"}, f"unknown codec: {codec}")
                capability = row["capability"]
                allowed = {"whole", "selectable"} if codec in VIDEO else {"audio" if codec == "OPUS" else "data"}
                require(capability in allowed, f"inapplicable capability: {codec} {capability}")
                modes = set(row["structure"].split(","))
                if modes & {"capture-modes", "other-corpus", "other-corpus+DD"}:
                    require(len(modes) == 1, "corpus selector must stand alone")
                    selector = modes.pop()
                    for (profile, mode), fixture in fixtures.items():
                        if profile != codec or fixture["sender"] == "missing":
                            continue
                        if selector == "capture-modes" or mode != ("L3T1" if codec == "VP9" else "L1T1"):
                            modes.add(mode + ("+DD" if selector.endswith("+DD") else ""))
                    require(modes, f"empty corpus selector: {codec} {selector}")
                for mode in modes:
                    if codec in VIDEO and mode not in {"-", "multiple-encodings"}:
                        require((codec, mode.removesuffix("+DD")) in fixtures, f"unknown mode: {codec} {mode}")
                    for direction in (("publish", "receive") if row["direction"] == "both" else (row["direction"],)):
                        require(direction in {"publish", "receive"}, f"unknown direction: {direction}")
                        subject = (endpoint, direction, codec, mode)
                        for path in PATHS:
                            status = row[path]
                            require(status in STATUSES, f"unknown disposition: {status}")
                            if status == "-":
                                continue
                            owner = evidence
                            if status in {"gap", "blocked", "candidate"}:
                                owner = f"codecs: {SIM}; add path/endpoint evidence (related proof: {evidence})"
                            key = (*subject, capability, path)
                            require(key not in cells, f"overlapping dispositions: {key}")
                            cells[key] = (status, owner)
                        if row[PROTECTION] != "-":
                            require(codec in VIDEO and row[PROTECTION] == "gap",
                                    "protected base requires a continuity witness")
                            cells[(*subject, PROTECTION, "regional-diamond")] = (
                                "gap", "continuity: fresh base decode across loss/drain with N-1 capacity",
                            )
    require({cell[0] for cell in cells} == ENDPOINTS, "endpoint missing from contract")
    for (codec, mode), row in fixtures.items():
        gap = row["sender"] == "missing"
        if gap or codec not in VIDEO:
            owner = CORPUS
        else:
            owner = f"{CORPUS}, {REPLAY}"
        cells[("fixture", "-", codec, mode, "corpus", "-")] = ("fixture-gap" if gap else "fixture-only", owner)
    return cells


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("contract", type=Path)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--show", action="store_true", help="print expanded cells as TSV")
    args = parser.parse_args()
    cells = matrix(args.contract, args.corpus)
    if args.show:
        print("\t".join((*KEY, "path", "disposition", "proof")))
        for key, value in sorted(cells.items()):
            print("\t".join((*key, *value)))


if __name__ == "__main__":
    main()
