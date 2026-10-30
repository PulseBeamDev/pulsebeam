"""Cheap finite-transform, semantic-classification and replay contracts."""
import argparse
from contextlib import redirect_stdout, redirect_stderr
import importlib.util
from io import StringIO
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parents[1]
ROOT = TOOLS.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CALIBRATE = load("calibrate", TOOLS / "calibrate.py")
TRANSFORM = load("transform", TOOLS / "calibration-transform.py")


def event(kind, **details):
    return {"claim": "local-fanout", "event": kind, "details": details}


def fanout_events(delivered):
    payload = list(b"local-fanout/fanout/source-input")
    events = [event("premise", shards=1, sequential_participants=3, intended_recipients=2),
              event("request", origin="authenticated-source", topic="calibration-origin", mode="lossy", payload=payload, recipients=2, recipient_ids=["first", "second"], phase="fanout")]
    events += [event("receipt", recipient=i, recipient_id=["first", "second"][i], origin="authenticated-source", payload=payload, phase="fanout") for i in delivered]
    if delivered == [0, 1]:
        events.append(event("pass"))
    else:
        events.append(event("violation", boundary="public", reason="delivery-deadline", phase="fanout", received=[i in delivered for i in range(2)]))
    return events


class CalibrationContract(unittest.TestCase):
    def classify(self, events, variant="local-fanout", failure="exit", status=101):
        return CALIBRATE.classify("local-fanout", variant, {"failure": failure, "exit_status": status}, events)["classification"]

    def test_control_detected_survived_and_infrastructure_are_distinct(self):
        complete = fanout_events([0, 1])
        partial = fanout_events([0])
        self.assertEqual(self.classify(complete, "control", None, 0), "control-pass")
        self.assertEqual(self.classify(partial, "control"), "control-failure")
        self.assertEqual(self.classify(partial), "detected-public-defect")
        # No-op/already repaired variants have all receipts, so calibration fails.
        self.assertEqual(self.classify(complete, failure=None, status=0), "survived-defect")
        for events, failure in [(partial, "timeout"), ([], "exit"), (partial[1:], "exit"),
                                (partial[:-1], "exit"), (fanout_events([]), "exit")]:
            with self.subTest(events=events, failure=failure):
                self.assertEqual(self.classify(events, failure=failure), "infrastructure-failure")

    def test_missing_or_forged_receipts_cannot_detect_a_defect(self):
        for mutate in [lambda e: e[2]["details"].update(origin="wrong-source"),
                       lambda e: e[-1]["details"].update(received=[False, True]),
                       lambda e: e[-1]["details"].update(boundary="inline"),
                       lambda e: e[1]["details"].update(payload=[0])]:
            events = fanout_events([0])
            mutate(events)
            self.assertEqual(self.classify(events), "infrastructure-failure")

    def test_false_201_is_immediate_public_violation(self):
        events = [{"claim": "materialization", "event": "request", "details": {"origin": "source", "topic": "calibration-origin", "mode": "lossy", "payload": list(b"materialization/before-fault/source-input"), "recipients": 1, "recipient_ids": ["first"], "phase": "before-fault"}},
                  {"claim": "materialization", "event": "receipt", "details": {"recipient": 0, "recipient_id": "first", "origin": "source", "payload": list(b"materialization/before-fault/source-input"), "phase": "before-fault"}},
                  {"claim": "materialization", "event": "premise", "details": {"fault": "materialization_failed", "consumed": 1}},
                  {"claim": "materialization", "event": "http", "details": {"method": "POST", "uri": "http://sfu:7070/api/v1/native", "status": 201}},
                  {"claim": "materialization", "event": "violation", "details": {"boundary": "public", "reason": "admission-status", "expected": 503, "actual": 201}}]
        result = CALIBRATE.classify("materialization", "materialization", {"failure": "exit", "exit_status": 101}, events)
        self.assertEqual(result["classification"], "detected-public-defect")
        self.assertEqual(CALIBRATE.classify("materialization", "materialization", {"failure": "exit", "exit_status": 101}, events[2:])["classification"], "infrastructure-failure")
        events[2]["details"]["consumed"] = 0
        self.assertEqual(CALIBRATE.classify("materialization", "materialization", {"failure": "exit", "exit_status": 101}, events)["classification"], "infrastructure-failure")

    def test_finite_transforms_match_once_and_preserve_other_sources(self):
        owner = ROOT / "crates/pulsebeam"
        for variant, (path, before, after) in TRANSFORM.TRANSFORMS.items():
            original = (owner / path).read_bytes()
            changed = TRANSFORM.transform(variant, path, original)
            self.assertNotEqual(original, changed)
            self.assertEqual(changed, original.replace(before.encode(), after.encode(), 1))
            self.assertEqual(TRANSFORM.transform(variant, "src/other.rs", b"unchanged"), b"unchanged")
            for stale in [changed, b"", original + before.encode()]:
                with self.assertRaisesRegex(ValueError, "stale"):
                    TRANSFORM.transform(variant, path, stale)

    def test_generated_layout_compile_data_and_transform_identity(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            variant = "local-fanout"
            site = TRANSFORM.TRANSFORMS[variant][0]
            entries = []
            for relative, data in [(site, (ROOT / "crates/pulsebeam" / site).read_bytes()), ("src/lib.rs", b"mod shard;"), ("README.md", b"compile data")]:
                source = root / "original" / relative
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_bytes(data)
                entries.append({"input": str(source), "output": str(root / "generated" / relative), "relative": relative})
            manifest, provenance = root / "manifest.json", root / "provenance.json"
            manifest.write_text(json.dumps(entries))
            with patch("sys.argv", ["transform", "--variant", variant, "--manifest", str(manifest), "--provenance", str(provenance)]):
                TRANSFORM.main()
            self.assertEqual((root / "generated/README.md").read_bytes(), b"compile data")
            identity = json.loads(provenance.read_text())
            self.assertEqual(identity["variant"], variant)
            self.assertEqual(len(identity["transform_sha256"]), 64)
            self.assertNotEqual(identity["files"][site]["input"], identity["files"][site]["output"])

    def test_exact_executables_retention_and_survivor_exit_status(self):
        # A bounded process fixture exercises orchestration without system matrices.
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": temp}), redirect_stdout(StringIO()), redirect_stderr(StringIO()):
            root = Path(temp)
            candidate = root / "candidate"
            candidate.write_text("STABLE_SIM_REVISION " + "a" * 40 + "\nSTABLE_SIM_DIRTY false\nSTABLE_SIM_FINGERPRINT " + "b" * 64 + "\n")
            control = root / "control"
            control.write_text("ordinary executable")
            argv = ["calibrate", "--candidate", str(candidate), "--control", str(control)]
            binaries = {}
            for variant in CALIBRATE.CASES:
                binary, provenance = root / variant, root / (variant + ".json")
                binary.write_text("unique executable: " + variant)
                provenance.write_text(json.dumps({"variant": variant, "transform_sha256": "unique-" + variant}))
                argv += ["--variant", variant, str(binary), str(provenance)]
                binaries[variant] = str(binary)
            invoked = []

            def process(command, trace, **kwargs):
                binary = command[0]
                if "--list" in command:
                    if "--ignored" not in command:
                        trace.write("".join("tests::calibration::" + case + ": test\n" for case in CALIBRATE.CASES.values()))
                    return 0, None
                invoked.append((binary, command[2]))
                claim = next(claim for claim, case in CALIBRATE.CASES.items() if command[2].endswith("::" + case))
                # All binaries are no-ops in this fixture. They must survive,
                # and a binary with no evidence must be infrastructure failure.
                if claim == "local-fanout":
                    path = Path(kwargs["env"]["TEST_UNDECLARED_OUTPUTS_DIR"]) / "calibration.jsonl"
                    path.write_text("".join(json.dumps(e) + "\n" for e in fanout_events([0, 1])))
                return 0, None

            with patch("sys.argv", argv), patch.object(CALIBRATE.runner, "run_process", side_effect=process):
                self.assertEqual(CALIBRATE.main(), 1)
            self.assertEqual(len(invoked), 6)
            self.assertEqual([binary for binary, _ in invoked], [str(control)] * 3 + list(binaries.values()))
            manifests = {path.parent: json.loads(path.read_text()) for path in root.glob("simulations/run-*/manifest.json")}
            mutant_root, mutant = next((directory, manifest) for directory, manifest in manifests.items()
                                       if manifest["variant"]["id"] == "local-fanout")
            self.assertEqual(mutant["cases"][0]["classification"], "survived-defect")
            self.assertEqual(mutant["variant"]["binary_sha256"], CALIBRATE.sha256(binaries["local-fanout"]))
            self.assertEqual(mutant["cases"][0]["artifact_checksums"]["outputs/calibration.jsonl"],
                             CALIBRATE.sha256(mutant_root / "case-0000/outputs/calibration.jsonl"))
            self.assertEqual(len(list(root.glob("simulations/run-*/case-*/trace.log"))), 6)
            self.assertEqual(len(list(root.glob("simulations/run-*/case-*/outputs/calibration.jsonl"))), 2)

    def test_detected_defect_cannot_hide_an_evidence_retention_failure(self):
        runner = CALIBRATE.runner
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": temp}), redirect_stdout(StringIO()):
            root = Path(temp)
            candidate = root / "candidate"
            candidate.write_text("STABLE_SIM_REVISION " + "a" * 40 + "\nSTABLE_SIM_DIRTY false\nSTABLE_SIM_FINGERPRINT " + "b" * 64 + "\n")
            external = root / "external"
            external.write_text("not an owned artifact")
            args = argparse.Namespace(binary="fake-libtest", candidate=str(candidate), artifacts_dir=None, show_output=False,
                replay_bundle=None, mode="all", filter="tests::calibration::local_fanout", exact=True,
                skip=[], run_skipped=False, seeds=1, from_seed=1, jobs=1, discovery_timeout=30, case_timeout=120, history_search="off")
            bundle = runner.Bundle(args)
            bundle.prepare(args)

            def process(command, trace, **kwargs):
                if "--list" in command:
                    if "--ignored" not in command:
                        trace.write(args.filter + ": test\n")
                    return 0, None
                (Path(kwargs["env"]["TEST_UNDECLARED_OUTPUTS_DIR"]) / "linked-artifact").symlink_to(external)
                trace.write("complete child failure trace\n")
                return 101, "exit"

            def assess(record, outputs):
                return CALIBRATE.classify("local-fanout", "local-fanout", record, fanout_events([0]))

            with patch.object(runner, "run_process", side_effect=process):
                self.assertEqual(runner.execute(args, bundle, assess=assess), 1)
            record = bundle.manifest["cases"][0]
            self.assertEqual(record["classification"], "detected-public-defect")
            self.assertEqual(record["child_failure"], "exit")
            self.assertEqual(record["status"], "failed")
            self.assertEqual(bundle.manifest["status"], "infrastructure")
            self.assertIn("inside owned outputs", record["evidence_error"])
            self.assertIn("complete child failure trace", (bundle.root / record["directory"] / "trace.log").read_text())

    def test_variant_replay_is_owned_and_ordinary_v1_still_has_meaning(self):
        runner = CALIBRATE.runner
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": temp}), redirect_stdout(StringIO()), redirect_stderr(StringIO()):
            root = Path(temp)
            candidate = root / "candidate"
            candidate.write_text("STABLE_SIM_REVISION " + "a" * 40 + "\nSTABLE_SIM_DIRTY false\nSTABLE_SIM_FINGERPRINT " + "b" * 64 + "\n")
            args = argparse.Namespace(candidate=str(candidate), artifacts_dir=None, replay_bundle=None, mode="all", filter="tests::calibration::local_fanout", exact=True, skip=[], run_skipped=False, seeds=1, from_seed=1, jobs=1, discovery_timeout=30, case_timeout=120, history_search="off")
            identity = {"id": "local-fanout", "binary_sha256": "exact-binary", "transform": "exact-transform"}
            bundle = CALIBRATE.CalibrationBundle(args, identity)
            bundle.prepare(args)
            args.replay_bundle = str(bundle.root)
            ordinary = runner.Bundle(args)
            with self.assertRaisesRegex(ValueError, "calibration_replay"):
                ordinary.prepare(args)
            replay = CALIBRATE.CalibrationBundle(args, identity)
            replay.prepare(args)
            changed = CALIBRATE.CalibrationBundle(args, identity | {"binary_sha256": "different"})
            with self.assertRaisesRegex(ValueError, "same finite target"):
                changed.prepare(args)
            command = replay.replay(replay.root, replay.manifest["selection"])
            self.assertIn(CALIBRATE.OWNER, command)
            args.replay_bundle = None
            ordinary = runner.Bundle(args)
            ordinary.prepare(args)
            ordinary_replay = json.loads((ordinary.root / "replay.json").read_text())
            self.assertEqual(ordinary_replay["version"], 1)
            self.assertEqual(ordinary_replay["selection"]["history_search"], "off")


if __name__ == "__main__":
    unittest.main()
