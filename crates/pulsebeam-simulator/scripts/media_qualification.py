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
CORPUS_SELECTORS = {"other-corpus", "other-corpus+DD"}
STATUSES = {"-", "observed", "encoded", "warm", "partial", "gap", "blocked", "candidate", "rejected", "legacy"}
SIM = "//crates/pulsebeam-simulator:media_fast"
CORPUS = "//crates/pulsebeam-testdata:corpus_inventory"
REPLAY = "//agents/pulsebeam-agent-quic:codec_matrix"
CROSS_SHARD_TEMPORAL = "quic_temporal_targets_change_native_decode_cadence_across_shards"
PROOFS = {
    "local-h264": f"{SIM} filter=whole_source_fabric_h264_reaches_",
    "native-h264-temporal": f"{SIM} filters=dependency_descriptor_stream_is_forwarded_decodably_test,dependency_descriptor_stream_is_forwarded_decodably_across_shards_test",
    "native-h264-selectable": f"{SIM} filter=selected_fabric_temporal_dd_reaches (15 to 30 fps on the same receiver after settling)",
    "quic-vp8-temporal": f"{SIM} filters=quic_temporal_vp8_targets_change_native_decode_cadence,{CROSS_SHARD_TEMPORAL} (bounded reacquisition, not gapless)",
    "quic-h264-temporal": f"{SIM} filters=quic_temporal_h264_targets_change_native_decode_cadence,provisioned_wan_h264_temporal_targets_change_native_decode_cadence,{CROSS_SHARD_TEMPORAL} (bounded reacquisition, not gapless)",
    "native-av1": f"{SIM} filter=native_claims_h264_and_av1_senders_after_join",
    "av1-no-dd": f"{SIM} filters=native_state_av1_decodes_with_adopted_fid,provisioned_av1_warm_whole_without_dd",
    "av1-dd": f"{SIM} filters=native_state_av1_carries_dd_without_selection,provisioned_av1_warm_whole_with_dd",
    "vp9-quic-whole": f"{SIM} filter=quic_whole_vp9_preserves_encoded_units (DD-free local only)",
    "av1-quic-wan": f"{SIM} filter=provisioned_wan_av1_whole_preserves_encoded_units",
    "quic-vp9-key-picture": f"{SIM} filter=quic_key_picture_vp9_targets_decode_complete_pictures (projected markers and native spatial decode)",
    "quic-av1-spatial": f"{SIM} filter=quic_spatial_av1_targets_decode_complete_pictures (same shard and measured cross-shard)",
    "quic-av1-key-picture": f"{SIM} filter=quic_key_picture_av1_targets_decode_complete_pictures (projected markers and native spatial decode)",
    "quic-av1-temporal": f"{SIM} filters=quic_temporal_av1_targets_change_native_decode_cadence,{CROSS_SHARD_TEMPORAL} (explicit DD emission, bounded reacquisition)",
    "av1-rtc-to-quic-dd": f"{SIM} filter=quic_whole_stock_av1_with_dd_decodes (whole-source only; RTC ingress does not guarantee DD)",
    "vp8-wan": f"{SIM} filter=provisioned_wan_vp8_whole_preserves_encoded_units",
    "vp9": f"{SIM} filters=quic_spatial_vp9_native_receiver,provisioned_wan_vp9_dd_decodes_and_recovers_after_loss",
    "vp9-independent": f"{SIM} filter=quic_independent_vp9_targets_decode_and_switch_on_one_subscription",
    "vp9-temporal": f"{SIM} filters=quic_temporal_vp9_targets_change_native_decode_cadence,{CROSS_SHARD_TEMPORAL} (bounded reacquisition, not gapless)",
    "vp9-structure": f"{SIM} filter=quic_vp9_replaces_dependency_structure_on_one_binding (L1T2 to L1T3 at fixed target 1)",
    "browser": "//agents/pulsebeam-agent-web:web_runtime_browser native_platform_smoke (live-agent-contract and quic-agent-smoke)",
    "mixed-h264": f"{SIM} filters=quic_media_h264,quic_mixed",
    "h264-whole": f"{SIM} filters=provisioned_wan_h264_whole_preserves_encoded_units,quic_media_h264,quic_mixed",
    "opus": f"{SIM} filters=quic_media_opus,provisioned_wan_opus_repairs_exact_position; browser native_platform_smoke",
    "data": f"{SIM} filters=sctp_and_quic_exchange_,local_ordered_,provisioned_wan_,sibling_regions_share_a_wan_branch; browser native_platform_smoke",
    "rtc-admission": "//crates/pulsebeam:unit_tests control::rtp::policy (applied binding and publication check)",
    "rtc-deferred-modes": "//crates/pulsebeam:unit_tests native_publication_rejects_deferred_modes_without_redefining_negotiation (publication binding only)",
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


def directions(row):
    return ("publish", "receive") if row["direction"] == "both" else (row["direction"],)


def explicit_structures(contract_rows):
    explicit = {}
    for row in contract_rows:
        modes = set(row["structure"].split(","))
        if modes & CORPUS_SELECTORS:
            continue
        for endpoint in row["endpoint"].split(","):
            for direction in directions(row):
                for codec in row["codec"].split(","):
                    key = (endpoint, direction, codec, row["capability"])
                    explicit.setdefault(key, set()).update(modes)
    return explicit


def matrix(contract, corpus):
    fixtures = {}
    for row in rows(corpus):
        pair = row["codec"], row["mode"]
        require(pair not in fixtures, f"duplicate fixture: {pair}")
        fixtures[pair] = row
    require({codec for codec, _ in fixtures} == VIDEO | {"OPUS", "DATA_V1"}, "unmapped corpus codec")
    contract_rows = list(rows(contract))
    for row in contract_rows:
        require(set(row) == {*KEY, *PATHS, PROTECTION, "evidence"} and None not in row.values(), "invalid contract row")
    explicit = explicit_structures(contract_rows)
    cells = {}
    for row in contract_rows:
        evidence = PROOFS[row["evidence"]]
        for endpoint in row["endpoint"].split(","):
            require(endpoint in ENDPOINTS, f"unknown endpoint: {endpoint}")
            for codec in row["codec"].split(","):
                require(codec in VIDEO | {"OPUS", "DATA_V1"}, f"unknown codec: {codec}")
                capability = row["capability"]
                allowed = {"whole", "selectable"} if codec in VIDEO else {"audio" if codec == "OPUS" else "data"}
                require(capability in allowed, f"inapplicable capability: {codec} {capability}")
                for direction in directions(row):
                    require(direction in {"publish", "receive"}, f"unknown direction: {direction}")
                    modes = set(row["structure"].split(","))
                    if modes & CORPUS_SELECTORS:
                        require(len(modes) == 1, "corpus selector must stand alone")
                        selector = modes.pop()
                        suffix = "+DD" if selector.endswith("+DD") else ""
                        modes = {
                            mode + suffix for (profile, mode), fixture in fixtures.items()
                            if profile == codec and fixture["sender"] != "missing"
                        }
                        modes -= explicit.get((endpoint, direction, codec, capability), set())
                        require(modes, f"empty corpus selector: {codec} {selector}")
                    for mode in modes:
                        if codec in VIDEO and mode not in {"-", "multiple-encodings"}:
                            require((codec, mode.removesuffix("+DD")) in fixtures, f"unknown mode: {codec} {mode}")
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
