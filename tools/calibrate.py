"""Bounded detector calibration, using the ordinary isolated simulator runner."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import shlex
import sys

spec = importlib.util.spec_from_file_location("simulation_runner", Path(__file__).with_name("run-simulations.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)

CASES = {"local-fanout": "local_fanout", "remote-fanout": "remote_fanout", "materialization": "materialization"}
OWNER = "//crates/pulsebeam-simulator:calibration_replay"


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def classify(claim, variant, record, events):
    """Only test-owned public evidence attributes detection, never panic wording."""
    infrastructure = {"classification": "infrastructure-failure", "status": "failed", "failure": "calibration-infrastructure"}
    if record["failure"] not in (None, "exit") or not events:
        return infrastructure
    if any(e.get("claim") != claim or not isinstance(e.get("details"), dict) for e in events):
        return infrastructure
    premises = [e["details"] for e in events if e.get("event") == "premise"]
    if len(premises) != 1:
        return infrastructure
    if claim == "materialization":
        if premises[0] != {"fault": "materialization_failed", "consumed": 1}:
            return infrastructure
    elif premises[0] != {"shards": 1 if claim == "local-fanout" else 3,
                          "sequential_participants": 3, "intended_recipients": 2}:
        return infrastructure
    requests = {}
    receipts = {}
    http = []
    violation = None
    passed = False
    retry = None
    premise_seen = False
    for event in events:
        kind, details = event["event"], event["details"]
        if violation is not None:
            return infrastructure
        if kind == "request":
            phase = details.get("phase")
            if phase in requests or details.get("topic") != "calibration-origin" or details.get("mode") != "lossy" or not details.get("origin"):
                return infrastructure
            ids = details.get("recipient_ids", [])
            if not ids or len(ids) != details.get("recipients") or len(set(ids)) != len(ids) or details["origin"] in ids:
                return infrastructure
            if details.get("payload") != list(f"{claim}/{phase}/source-input".encode()):
                return infrastructure
            requests[phase] = details
            receipts[phase] = set()
        elif kind == "receipt":
            phase = details.get("phase")
            request = requests.get(phase)
            if not request or details.get("origin") != request["origin"] or details.get("payload") != request["payload"]:
                return infrastructure
            recipient = details.get("recipient")
            if not isinstance(recipient, int) or not 0 <= recipient < request["recipients"] or recipient in receipts[phase]:
                return infrastructure
            if details.get("recipient_id") != request["recipient_ids"][recipient]:
                return infrastructure
            receipts[phase].add(recipient)
        elif kind == "http":
            if not premise_seen or details.get("method") != "POST" or not details.get("uri", "").endswith("/api/v1/native"):
                return infrastructure
            http.append(details)
        elif kind == "retry":
            if claim != "materialization" or retry is not None or details != {"statuses": [503, 201]}:
                return infrastructure
            retry = details
        elif kind == "pass":
            passed = True
        elif kind == "violation":
            if not premise_seen or passed or details.get("boundary") != "public":
                return infrastructure
            reason = details.get("reason")
            if reason == "delivery-deadline":
                phase = details.get("phase")
                request = requests.get(phase)
                received = details.get("received")
                if not request or not isinstance(received, list) or len(received) != request["recipients"]:
                    return infrastructure
                if received != [i in receipts[phase] for i in range(request["recipients"])] or all(received):
                    return infrastructure
                # A partial fanout must positively serve one recipient, not fail setup.
                if claim != "materialization" and (phase != "fanout" or request["recipients"] != 2 or not any(received)):
                    return infrastructure
            elif reason == "admission-status":
                if claim != "materialization" or details.get("expected") != 503 or details.get("actual") != 201 or len(http) != 1 or http[0].get("status") != 201:
                    return infrastructure
                # False 201 rejects immediately, but only after genuine unrelated
                # service was established before the controlled admission fault.
                before = requests.get("before-fault")
                if not before or before["recipients"] != 1 or receipts["before-fault"] != {0}:
                    return infrastructure
            else:
                return infrastructure
            violation = event
        elif kind == "premise":
            premise_seen = True
        else:
            return infrastructure
    expected_phases = {"before-fault": 1, "unrelated-service": 1, "after-retry": 2} if claim == "materialization" else {"fanout": 2}
    complete = set(requests) == set(expected_phases) and all(
        requests[phase]["recipients"] == count and len(receipts[phase]) == count for phase, count in expected_phases.items())
    if claim == "materialization":
        complete = complete and len(http) == 1 and http[0].get("status") == 503 and retry is not None
    if record["exit_status"] == 0 and passed and complete and violation is None:
        classification = "control-pass" if variant == "control" else "survived-defect"
        return {"classification": classification, "status": "passed" if variant == "control" else "failed",
                "failure": None if variant == "control" else classification}
    if record["exit_status"] != 0 and violation is not None:
        # Only the declared defect's violation counts. Recovery failures in an
        # admission mutant do not establish the false-success HTTP contract.
        intended = violation["details"]["reason"] == ("admission-status" if claim == "materialization" else "delivery-deadline")
        if variant != "control" and not intended:
            return infrastructure
        classification = "control-failure" if variant == "control" else "detected-public-defect"
        return {"classification": classification, "first_violation": violation,
                "status": "failed" if variant == "control" else "passed",
                "failure": classification if variant == "control" else None}
    return infrastructure


class CalibrationBundle(runner.Bundle):
    replay_version = 2

    def __init__(self, args, identity):
        self.identity = identity
        super().__init__(args)
        self.manifest.update(version=2, variant=identity)

    def prepare(self, args):
        if args.replay_bundle:
            replay = json.loads((Path(args.replay_bundle) / "replay.json").read_text())
            if replay.get("version") != 2 or replay.get("variant") != self.identity:
                raise ValueError("calibration replay requires the same finite target, transform and executable checksum")
        super().prepare(args)

    def replay(self, directory, selection, history=None):
        super().replay(directory, selection, history)
        path = directory / "replay.json"
        replay = json.loads(path.read_text())
        runner.write_json(path, replay | {"version": 2, "variant": self.identity})
        return shlex.join(["./bazel", "run", OWNER, "--", "--replay-bundle", str(directory)])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--control", required=True)
    parser.add_argument("--variant", action="append", nargs=3, metavar=("ID", "BINARY", "PROVENANCE"), required=True)
    parser.add_argument("--replay-bundle")
    parser.add_argument("--artifacts-dir")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--case-timeout", type=float, default=120)
    options = parser.parse_args()
    if not 0 <= options.seed < 2**64 or not math.isfinite(options.case_timeout) or options.case_timeout <= 0:
        parser.error("seed must fit u64 and host deadline must be finite and positive")
    variants = {name: (binary, provenance) for name, binary, provenance in options.variant}
    if len(options.variant) != 3 or set(variants) != set(CASES):
        parser.error("exactly the three finite SFU variants are required")
    selections = [("control", claim) for claim in CASES] + [(claim, claim) for claim in CASES]
    if options.replay_bundle:
        try:
            replay = json.loads((Path(options.replay_bundle) / "replay.json").read_text())
            identity = replay.get("variant", {})
            selection = (identity.get("id"), identity.get("claim"))
            if selection not in selections:
                raise ValueError("bundle does not identify a supported calibration case")
            selections = [selection]
        except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
            runner.Bundle(options).infrastructure_error(error)
            return 1
    failed = False
    for variant, claim in selections:
        bundle = None
        try:
            binary = options.control if variant == "control" else variants[variant][0]
            provenance = None if variant == "control" else json.loads(Path(variants[variant][1]).read_text())
            if provenance is not None and provenance.get("variant") != variant:
                raise ValueError("wrong variant provenance")
            identity = {"id": variant, "claim": claim,
                        "target": "//crates/pulsebeam-simulator:sim_binary" + ("" if variant == "control" else "_" + variant),
                        "binary_sha256": sha256(binary), "transform": provenance}
            args = argparse.Namespace(binary=binary, candidate=options.candidate, artifacts_dir=options.artifacts_dir,
                                      replay_bundle=options.replay_bundle, mode="all", filter="tests::calibration::" + CASES[claim],
                                      exact=True, skip=[], run_skipped=False, seeds=1, from_seed=options.seed, jobs=1,
                                      discovery_timeout=30, case_timeout=options.case_timeout, show_output=False,
                                      history_search="off")
            bundle = CalibrationBundle(args, identity)
            bundle.prepare(args)
            if args.filter != "tests::calibration::" + CASES[claim] or not args.exact or args.seeds != 1 or args.jobs != 1:
                raise ValueError("calibration replay selection must be one exact finite case")

            def assess(record, outputs):
                path = outputs / "calibration.jsonl"
                events = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
                result = classify(claim, variant, record, events)
                print(f"[calibration] {variant}/{claim}: {result['classification']}", flush=True)
                return result

            failed |= bool(runner.execute(args, bundle, assess=assess))
        except (OSError, ValueError, KeyError, TypeError) as error:
            failed = True
            if bundle:
                bundle.infrastructure_error(error)
            else:
                print(f"[calibration-infrastructure] {variant}/{claim}: {error}", file=sys.stderr)
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
