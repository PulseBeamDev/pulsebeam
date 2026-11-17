"""Pack and check only the bounded rtc-uplink-v1 retained study."""
import gzip
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[3]
HERE = ROOT / "crates/pulsebeam-transport/experiments"
ARMS = {"native_screamv2", "native_gcc", "udp_scream", "quic_outer", "quic_cubic", "quic_bbr3"}
SCENARIOS = {"clean", "capacity_step", "loss_1pct", "rtt_160ms"}
ATTRIBUTION = [
    "Cargo.toml", "Cargo.lock", "MODULE.bazel", "MODULE.bazel.lock", ".bazelrc", "rust-toolchain.toml",
    "crates/pulsebeam-libwebrtc-sys/repositories.bzl",
    "crates/pulsebeam-cc/scripts/verify-scream-profile.sh",
    "crates/pulsebeam-testdata/src/quality-corpus.manifest",
    "crates/pulsebeam-testdata/src/quality_source.rs",
    "crates/pulsebeam-testdata/scripts/generate-quality-corpus.sh",
] + ["crates/pulsebeam-transport/experiments/" + name for name in [
    "comparison.rs", "corpus.rs", "decoder.rs", "encoder.rs", "measurement.rs", "native_profile.rs",
    "path.rs", "presentation.rs", "receiver.rs", "rtp_video.rs", "uplink.rs", "webtransport_poc.rs", "results.py",
]]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode()


def validate(run):
    s = run["summary"]
    assert run["schema"] == "rtc-uplink-v1" and run["arm"] in ARMS
    assert run["scenario"] in SCENARIOS and run["seed"] in [1, 2, 3]
    assert s["qualified"] and run["failure"] is None, (run["scenario"], run["arm"], run["seed"])
    assert s["invalid_identity"] == s["invalid_timestamp"] == s["unknown"] == 0
    assert s["offered_captures"] == sum(s[k] for k in ["timely", "late", "missing", "unknown"]) == 180
    assert s["observed_cutoff_ns"] == 7_500_000_000
    assert s["blank_ns"] + s["presented_ns"] == 6_000_000_000
    assert s["startup_blank_ns"] <= s["blank_ns"]
    assert s["freeze_ns"] <= s["held_ns"] <= s["presented_ns"]
    wire = run["wire"]
    assert all(a == b + c for a, b, c in zip(wire["attempted_bytes_by_direction"], wire["scheduled_bytes_by_direction"], wire["dropped_bytes_by_direction"], strict=True))
    trace = s["trace"]
    offered, decoded, ticks = trace["offered"], trace["decoded"], trace["presentation"]
    assert len(offered) == 180 and len(ticks) == 600
    assert all(c["id"] == [run["source_id"], 0, i // 90, i] and c["capture_ns"] == i * 1_000_000_000 // 30 for i, c in enumerate(offered))
    assert len({(tuple(d["id"]), d["layer"]) for d in decoded}) == len(decoded)
    first = {}
    for d in decoded:
        capture = d["id"][3]
        assert d["id"] == offered[capture]["id"] and d["layer"] in ["Q", "H", "F"]
        assert offered[capture]["capture_ns"] <= d["ready_ns"] <= s["observed_cutoff_ns"]
        first[capture] = min(first.get(capture, d["ready_ns"]), d["ready_ns"])
    assert sum(at - offered[i]["capture_ns"] <= 180_000_000 for i, at in first.items()) == s["timely"]
    assert len(first) == s["timely"] + s["late"]
    assert sum(d["reference_squared_error"] for d in decoded) == s["decoded_representation_squared_error"]
    assert sum(d["reference_samples"] for d in decoded) == s["decoded_representation_samples"]
    assert sum((t["reference_squared_error"] or 0) * t["duration_ns"] for t in ticks) == int(s["presented_squared_error_times_ns"])
    assert sum(86_400 * t["duration_ns"] for t in ticks if t["ready_sequence"] is not None) == int(s["presented_samples_times_ns"])
    by_sequence = {d["sequence"]: d for d in decoded}
    for tick in ticks:
        assert tick["reference_capture"] == min(tick["at_ns"] * 30 // 1_000_000_000, 179)
        if tick["ready_sequence"] is None:
            assert tick["reference_squared_error"] is None and tick["age_ns"] is None
        else:
            output = by_sequence[tick["ready_sequence"]]
            assert output["ready_ns"] <= tick["at_ns"]
            assert output["id"][:3] == offered[tick["reference_capture"]]["id"][:3]
            assert tick["age_ns"] == tick["at_ns"] - offered[output["id"][3]]["capture_ns"]
    assert sum((t["age_ns"] or 0) * t["duration_ns"] for t in ticks) == int(s["displayed_age_ns_times_duration_ns"])
    if run["arm"].startswith("native"):
        assert run["native_scream_feedback_observed"] == (run["arm"] == "native_screamv2")
        assert run["negotiation"]["receiver_only"] and run["negotiation"]["twcc"]
        assert run["source"]["produced"] is None and run["lifecycle"]["native_input_pending"] is None
    else:
        source = run["source"]
        assert run["outer_cc"] == "rust_screamv2"
        categories = ["produced_by_layer", "paused_by_layer", "encoder_omissions_by_layer", "backpressured_by_layer", "unobserved_by_layer"]
        assert all(sum(source[k][layer] for k in categories) == 180 for layer in range(3))
        units = source["produced"]
        assert len(units) == sum(source["produced_by_layer"]) == run["lifecycle"]["caller_admitted_pictures"] + len(run["lifecycle"]["caller_terminal"])
        assert all(u["caller_accepted_fragments"] + u["caller_queued_fragments_at_retirement"] == u["fragments"] for u in units)


def key(run):
    return run["source_id"], run["scenario"], run["seed"], run["arm"]


def expected_keys():
    primary = {(0, scenario, seed, arm) for scenario in SCENARIOS for seed in [1, 2, 3] for arm in ARMS}
    return primary | {(1, "clean", 1, arm) for arm in ARMS}


def summary(run):
    result = dict(run)
    result["summary"] = {k: v for k, v in run["summary"].items() if k != "trace"}
    result["source"] = {k: v for k, v in run["source"].items() if k != "produced"}
    return result


def check():
    meta = json.loads((HERE / "results.meta.json").read_bytes())
    assert meta["schema"] == "rtc-comparison-evidence-v1"
    assert meta["native_applied_patches"] == []
    assert set(meta["attribution_sha256"]) == set(ATTRIBUTION)
    for name, sha in meta["artifact_sha256"].items():
        assert digest((HERE / name).read_bytes()) == sha, name
    for name, sha in meta["attribution_sha256"].items():
        assert digest((ROOT / name).read_bytes()) == sha, name
    records = [json.loads(line) for line in gzip.decompress((HERE / "results.traces.jsonl.gz").read_bytes()).splitlines()]
    runs = [record for record in records if record["kind"] == "run"]
    assert len(runs) == 78 and {key(run) for run in runs} == expected_keys()
    for run in runs:
        validate(run)
    saved = json.loads((HERE / "results.json").read_bytes())
    assert saved["schema"] == meta["schema"] and saved["source_candidate"] == meta["source_candidate"]
    assert saved["runs"] == [summary(run) for run in runs]
    print("78 retained runs: candidate/profile attribution, coverage, pixel/time, wire and lifecycle contracts passed")


def pack(primary, sensitivity, evidence_path):
    evidence = json.loads(Path(evidence_path).read_bytes())
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    assert not subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT), "pack from the frozen clean source candidate before writing artifacts"
    assert evidence["status"] == "passed" and evidence["candidate"].split(":")[0] == revision
    datasets, records, runs = [], [], []
    for path, arguments in [(primary, "--seeds=3 --source=0"), (sensitivity, "--seeds=1 --source=1 --scenario=clean")]:
        raw = Path(path).read_bytes()
        rows = [json.loads(line) for line in raw.splitlines()]
        assert rows[0]["kind"] == "metadata" and rows[0]["schema"] == "rtc-uplink-v1"
        records.extend(rows)
        runs.extend(rows[1:])
        datasets.append({"command": "./bazel run //crates/pulsebeam-transport:webtransport_poc -- " + arguments,
                         "raw_sha256": digest(raw), "header": rows[0], "runs": len(rows) - 1})
    assert len(runs) == 78 and {key(run) for run in runs} == expected_keys()
    for run in runs:
        validate(run)
    attribution = {name: digest((ROOT / name).read_bytes()) for name in ATTRIBUTION}
    native = re.search(r'_WEBRTC = "([0-9a-f]+)"', (ROOT / ATTRIBUTION[6]).read_text()).group(1)
    rust = re.search(r"revision='([0-9a-f]+)'", (ROOT / ATTRIBUTION[7]).read_text()).group(1)
    assert native == rust
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    summary_data = encoded({"schema": "rtc-comparison-evidence-v1", "source_candidate": evidence["candidate"], "runs": [summary(run) for run in runs]})
    trace_data = gzip.compress(b"".join((json.dumps(record, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode() for record in records), mtime=0)
    codec = (HERE / "encoder.rs").read_text()
    integer_field = lambda name: int(re.search(r"(?:fields\." + name + r" = |" + name + r": )([0-9_]+)", codec).group(1).replace("_", ""))
    source_code = (HERE / "corpus.rs").read_text()
    minima = re.search(r"const MINIMUM: \[u32; 3\] = \[(.*?)\];", source_code, re.S).group(1)
    minima = [int(value.strip().replace("_", "")) for value in minima.split(",") if value.strip()]
    meta = {"schema": "rtc-comparison-evidence-v1", "source_candidate": evidence["candidate"], "source_revision": revision,
            "dirty_fingerprint": None, "verifier": evidence["id"], "commands": evidence["commands"], "datasets": datasets,
            "native_google_webrtc_revision": native, "rust_scream_source_profile_revision": rust,
            "native_applied_patches": [],
            "noq_proto_version": next(p["version"] for p in lock["package"] if p["name"] == "noq-proto"),
            "rust_toolchain": tomllib.loads((ROOT / "rust-toolchain.toml").read_text()),
            "attribution_scope": "experiment/source/reference policies, lockfiles, native/Rust selected-profile manifests; complete source is identified by clean Git revision",
            "attribution_sha256": attribution,
            "artifact_sha256": {"results.json": digest(summary_data), "results.traces.jsonl.gz": digest(trace_data)},
            "codec_constraints": {
                "shared": {"implementation": "libwebrtc built-in H264", "fps": 30,
                           "dimensions_q_h_f": [[320, 180], [640, 360], [1280, 720]],
                           "aggregate_start_bps": 300_000, "rung_caps_bps": [200_000, 600_000, 1_400_000]},
                "custom": {"aggregate_activation_minima_bps": minima,
                           "codec_min_bitrate_kbps": integer_field("min_bitrate_kbps"),
                           "qp_max": integer_field("qp_max"), "periodic_keyframe_interval_captures": integer_field("key_frame_interval"),
                           "additional_keyframes": "epoch boundary, resumed rung, genuine reverse-path repair",
                           "frame_dropping": "enabled, actual callback omissions counted"},
                "native": {"allocation_and_codec_omissions": "native defaults and negotiated constraints, not inferred from stats",
                           "minimum_aggregate_bps": 50_000, "frame_dropper": "native default enabled",
                           "negotiated_media_attributes": "recorded per run without ICE credentials"}},
            "premises": {"synthetic_sources": True, "browser_ecn_l4s_qualified": False, "physical_codec_cpu_latency_modeled": False,
                         "isolated_controller_comparison": False, "native_full_receive_playout_comparison": False},
            "variation": "source0: four conditions x three retained seeds x six arms; source1: clean seed1 sensitivity witness only"}
    (HERE / "results.json").write_bytes(summary_data)
    (HERE / "results.traces.jsonl.gz").write_bytes(trace_data)
    (HERE / "results.meta.json").write_bytes(encoded(meta))
    check()


if __name__ == "__main__":
    if len(sys.argv) == 5 and sys.argv[1] == "pack":
        pack(*sys.argv[2:])
    else:
        assert len(sys.argv) == 1 or sys.argv[1:] == ["check"]
        check()
