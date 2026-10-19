"""Run libtest simulations in separate processes with owned replay evidence."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


# Only replay-affecting settings, never an environment dump. Seed and corpus
# are captured separately. Keep unset values absent on replay as well.
SETTINGS = ("RUST_LOG", "PROPTEST_CASES", "PROPTEST_MAX_LOCAL_REJECTS",
            "PROPTEST_MAX_GLOBAL_REJECTS", "PROPTEST_MAX_FLAT_MAP_REGENS",
            "PROPTEST_MAX_SHRINK_TIME", "PROPTEST_MAX_SHRINK_ITERS",
            "PROPTEST_MAX_DEFAULT_SIZE_RANGE", "PROPTEST_RNG_ALGORITHM",
            "PROPTEST_RNG_SEED", "PROPTEST_VERBOSE", "PROPTEST_FORK",
            "PROPTEST_TIMEOUT", "PROPTEST_DISABLE_FAILURE_PERSISTENCE")
SELECTION = ("mode", "filter", "exact", "skip", "run_skipped", "seeds", "from_seed",
             "jobs", "discovery_timeout", "case_timeout")
REPLAY_LIMITS = ("Requires the same actual source candidate and supported toolchain. "
                 "Dirty fingerprints attribute sources but do not package them. "
                 "Regression RNG states use the current strategy, not historical scenarios.")


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def candidate(path):
    if not path:
        raise ValueError("required candidate provenance is missing; use the supported Bazel target or --candidate")
    values = dict(line.split(" ", 1) for line in Path(path).read_text().splitlines() if " " in line)
    revision = values.get("STABLE_SIM_REVISION", "")
    fingerprint = values.get("STABLE_SIM_FINGERPRINT", "")
    dirty = values.get("STABLE_SIM_DIRTY")
    if not re.fullmatch(r"[0-9a-f]{40,64}", revision) or not re.fullmatch(r"[0-9a-f]{64}", fingerprint) or dirty not in ("true", "false"):
        raise ValueError("invalid or incomplete candidate provenance")
    return {"revision": revision, "dirty": dirty == "true", "fingerprint": fingerprint}


def run_process(command, trace, *, timeout, env=None):
    trace.write(f"command={shlex.join(command)}\n")
    trace.flush()
    try:
        process = subprocess.Popen(command, env=env, stdout=trace, stderr=subprocess.STDOUT,
                                   start_new_session=True)
    except OSError as error:
        trace.write(f"Unable to start child: {error}\n")
        return 127, "launch"
    try:
        status = process.wait(timeout=timeout)
        return status, "exit" if status else None
    except subprocess.TimeoutExpired:
        # Native helpers and descendants must not outlive discovery or a case.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        trace.write(f"TIMEOUT after {timeout:g} host seconds; process group terminated\n")
        return process.returncode, "timeout"


class Bundle:
    def __init__(self, args):
        # Even an explicit destination is a parent, never a shared mutable run.
        base = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR")
        base = Path(base) / "simulations" if base else Path(args.artifacts_dir or tempfile.gettempdir())
        base.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="run-", dir=base)).resolve()
        self.manifest = {"version": 1, "status": "running", "candidate": None,
                         "replay_limits": REPLAY_LIMITS, "cases": [], "discovery": []}
        self.save()
        print(f"[simulation-artifacts] {self.root}", flush=True)

    def save(self):
        write_json(self.root / "manifest.json", self.manifest)

    def prepare(self, args):
        self.manifest["candidate"] = candidate(args.candidate)
        settings = {key: os.environ[key] for key in SETTINGS if key in os.environ}
        corpus = os.environ.get("PULSEBEAM_SIM_REGRESSIONS")
        history = os.environ.get("PULSEBEAM_SIM_HISTORY")
        history_checksum = None
        expected_checksum = None
        self.manifest.update(selection={key: getattr(args, key) for key in SELECTION}, settings=settings, input=None)
        self.save()
        if args.replay_bundle:
            source = Path(args.replay_bundle).resolve()
            replay = json.loads((source / "replay.json").read_text())
            if replay["version"] != 1 or replay["candidate"] != self.manifest["candidate"]:
                raise ValueError("replay requires the recorded actual source candidate (revision, dirty state and fingerprint)")
            for key in SELECTION:
                setattr(args, key, replay["selection"][key])
            settings = replay["settings"]
            corpus = None
            history = None
            if replay.get("history"):
                history = source / replay["history"]["path"]
                history_checksum = replay["history"]["sha256"]
            if replay["input"]:
                corpus = source / replay["input"]["path"]
                expected_checksum = replay["input"]["sha256"]
        captured = None
        if corpus is not None:
            data = Path(corpus).read_bytes()
            if expected_checksum and hashlib.sha256(data).hexdigest() != expected_checksum:
                raise ValueError("retained regression input checksum mismatch")
            (self.root / "inputs").mkdir()
            (self.root / "inputs" / "supplied.regressions").write_bytes(data)
            captured = {"path": "inputs/supplied.regressions", "sha256": hashlib.sha256(data).hexdigest()}
        captured_history = None
        if history is not None:
            data = Path(history).read_bytes()
            checksum = hashlib.sha256(data).hexdigest()
            if history_checksum and checksum != history_checksum:
                raise ValueError("retained history input checksum mismatch")
            (self.root / "inputs").mkdir(exist_ok=True)
            (self.root / "inputs" / "supplied.history").write_bytes(data)
            captured_history = {"path": "inputs/supplied.history", "sha256": checksum}
        self.manifest.update(selection={key: getattr(args, key) for key in SELECTION},
                             settings=settings, input=captured, history=captured_history)
        self.save()
        self.replay(self.root, self.manifest["selection"])
        self.env = os.environ.copy()
        for key in (*SETTINGS, "PULSEBEAM_SIM_SEED", "PULSEBEAM_SIM_REGRESSIONS", "PULSEBEAM_SIM_HISTORY"):
            self.env.pop(key, None)
        self.env.update(settings)
        if captured:
            self.env["PULSEBEAM_SIM_REGRESSIONS"] = str(self.root / captured["path"])
        if captured_history:
            self.env["PULSEBEAM_SIM_HISTORY"] = str(self.root / captured_history["path"])
        if args.from_seed is not None:
            self.env["PULSEBEAM_SIM_SEED"] = str(args.from_seed)

    def replay(self, directory, selection, history=None):
        captured = self.manifest["input"]
        if captured:
            captured = captured | {"path": os.path.relpath(self.root / captured["path"], directory)}
        history = self.manifest["history"] if history is None else history
        if history:
            history = history | {"path": os.path.relpath(self.root / history["path"], directory)}
        write_json(directory / "replay.json", {"version": 1, "candidate": self.manifest["candidate"],
                   "selection": selection, "settings": self.manifest["settings"], "input": captured, "history": history})
        return shlex.join(["./bazel", "run", "//:replay", "--", "--replay-bundle", str(directory)])

    def infrastructure_error(self, error):
        self.manifest.update(status="infrastructure", error=str(error))
        try:
            self.save()
            (self.root / "infrastructure.log").write_text(str(error) + "\n")
        except OSError as write_error:
            print(f"Unable to retain infrastructure diagnostics: {write_error}", file=sys.stderr)
        print(f"[simulation-infrastructure] FAIL: {error}; evidence: {self.root}", file=sys.stderr, flush=True)


def names(binary, bundle, seed_label, timeout, *flags):
    command = [binary, "--list", "--format=terse", *flags]
    log = bundle.root / ("discovery-ignored.log" if "--ignored" in flags else "discovery.log")
    outputs = bundle.root / (log.stem + "-outputs")
    outputs.mkdir()
    with log.open("w") as trace:
        trace.write(f"case=discovery seed={seed_label}\n")
        status, category = run_process(command, trace, timeout=timeout,
                                       env=bundle.env | {"TEST_UNDECLARED_OUTPUTS_DIR": str(outputs)})
    bundle.manifest["discovery"].append({"command": command, "status": status,
                                        "failure": category or None, "trace": log.name if category else None})
    bundle.save()
    if category:
        bundle.manifest["status"] = "failed"
        bundle.manifest["failure"] = "discovery-" + category
        bundle.save()
        print(f"[simulation-discovery] FAIL seed={seed_label} category={category}; "
              f"command={shlex.join(command)}\nFailure trace: {log}", file=sys.stderr, flush=True)
        print(f"Replay: ./bazel run //:replay -- --replay-bundle {shlex.quote(str(bundle.root))}", flush=True)
        return None
    selected = [line.removesuffix(": test") for line in log.read_text().splitlines()
                if line.endswith(": test")]
    log.unlink()
    shutil.rmtree(outputs)
    return selected


def execute(args, bundle):
    binary = str(Path(args.binary).resolve())
    seed_label = "committed" if args.from_seed is None else str(args.from_seed)
    ignored = names(binary, bundle, seed_label, args.discovery_timeout, "--ignored")
    if ignored is None:
        return 1
    listed = names(binary, bundle, seed_label, args.discovery_timeout)
    if listed is None:
        return 1
    selected = [name for name in listed if name not in ignored
                and (name == args.filter if args.exact else args.filter in name)
                and (args.mode == "all" or ("::slow::" in name) == (args.mode == "slow"))]
    if not args.run_skipped:
        for name in args.skip:
            if name in selected:
                selected.remove(name)
                print(f"[simulation-result] SKIP {name}; documented gate exception in crates/pulsebeam-simulator/docs/native-dtls-exception.md", flush=True)
    if not selected:
        bundle.manifest.update(status="failed", failure="selection")
        bundle.save()
        print("no non-ignored simulations match the requested selection", file=sys.stderr)
        return 2

    print(f"[simulation-run] cases={len(selected)} seeds={args.seeds} jobs={args.jobs}", flush=True)
    cases = [(name, None if args.from_seed is None else args.from_seed + offset)
             for offset in range(args.seeds) for name in selected]
    for index, (name, seed) in enumerate(cases):
        directory = bundle.root / f"case-{index:04d}"
        directory.mkdir()
        replay_selection = bundle.manifest["selection"] | {"filter": name, "exact": True,
                           "from_seed": seed, "seeds": 1, "jobs": 1}
        replay = bundle.replay(directory, replay_selection)
        bundle.manifest["cases"].append({"case": name, "seed": seed, "seed_selection": "authored-default" if seed is None else "explicit",
                                         "directory": directory.name, "status": "pending", "replay": replay})
    bundle.save()

    def run(index):
        record = bundle.manifest["cases"][index].copy()
        directory = bundle.root / record["directory"]
        outputs = directory / "outputs"
        outputs.mkdir()
        scratch = outputs / "scratch"
        scratch.mkdir()
        env = bundle.env | {"TEST_UNDECLARED_OUTPUTS_DIR": str(outputs), "TEST_TMPDIR": str(scratch)}
        if record["seed"] is not None:
            env["PULSEBEAM_SIM_SEED"] = str(record["seed"])
        seed_label = "committed" if record["seed"] is None else str(record["seed"])
        log = directory / "trace.log"
        record["status"] = "running"
        write_json(directory / "result.json", record)
        started = time.perf_counter()
        with log.open("w") as trace:
            trace.write(f"case={record['case']} seed={seed_label}\n")
            status, category = run_process([binary, "--exact", record["case"], "--nocapture", "--test-threads=1"],
                                            trace, env=env, timeout=args.case_timeout)
        record.update(status="failed" if category else "passed", exit_status=status, failure=category,
                      wall_seconds=time.perf_counter() - started)
        # Preserve all files emitted under the owned case output boundary on
        # failure, including corpora and concrete scenarios if a case emits them.
        record["artifacts"] = [str(path.relative_to(directory)) for path in sorted(outputs.rglob("*")) if path.is_file()] if category else []
        if category:
            histories = sorted(outputs.rglob("*.reduced.history"))
            if len(histories) > 1:
                raise ValueError("ambiguous concrete history replay inputs")
            if histories:
                history = histories[0]
                captured = {"path": str(history.relative_to(bundle.root)),
                            "sha256": hashlib.sha256(history.read_bytes()).hexdigest()}
                selection = json.loads((directory / "replay.json").read_text())["selection"]
                record["replay"] = bundle.replay(directory, selection, history=captured)
                record["history"] = captured
        write_json(directory / "result.json", record)
        return index, record, log

    failed = False
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        futures = [executor.submit(run, index) for index in range(len(cases))]
        for future in as_completed(futures):
            index, record, log = future.result()
            bundle.manifest["cases"][index] = record
            # Only this thread emits traces, so metrics stay adjacent to results.
            with log.open(errors="replace") as trace:
                for line in trace:
                    if args.show_output or line.startswith(("[simulation-generated]", "[simulation-premise]")):
                        sys.stdout.write(line)
            seed_label = "committed" if record["seed"] is None else str(record["seed"])
            outcome = "FAIL" if record["failure"] else "PASS"
            print(f"[simulation-result] {outcome} {record['case']} seed={seed_label} wall_seconds={record['wall_seconds']:.3f}", flush=True)
            print(f"Replay: {record['replay']}", flush=True)
            if record["failure"]:
                failed = True
                if record["failure"] == "timeout":
                    print(f"Host timeout: {args.case_timeout:g}s; child process group terminated", flush=True)
                print(f"Failure trace: {log}", flush=True)
            else:
                log.unlink()
                shutil.rmtree(log.parent / "outputs")
    bundle.manifest["status"] = "failed" if failed else "passed"
    bundle.save()
    return int(failed)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--candidate", help="declared Bazel workspace-status metadata (required)")
    parser.add_argument("--artifacts-dir", help="persistent parent for a unique owned run; Bazel test outputs take precedence")
    parser.add_argument("--replay-bundle", help="retained run or case directory containing replay.json")
    parser.add_argument("--mode", choices=["fast", "slow", "all"], default="fast")
    parser.add_argument("--filter", default="")
    parser.add_argument("--exact", action="store_true", help="match the complete filtered case name")
    parser.add_argument("--skip", action="append", default=[], help="exact documented gate exception")
    parser.add_argument("--run-skipped", action="store_true", help="explicitly execute documented gate exceptions")
    parser.add_argument("--seeds", type=int, default=1)
    parser.add_argument("--from-seed", "--seed", dest="from_seed", type=int, default=os.environ.get("PULSEBEAM_SIM_SEED"))
    parser.add_argument("--jobs", type=int, default=1 if "TEST_TMPDIR" in os.environ else min(16, os.cpu_count() or 1))
    parser.add_argument("--show-output", action="store_true", help="emit complete grouped traces, including successes")
    parser.add_argument("--discovery-timeout", type=float, default=30, help="host-second deadline per discovery process")
    parser.add_argument("--case-timeout", type=float, default=600, help="host-second deadline per isolated case")
    args = parser.parse_args()
    # Validate ordinary invocations before creating evidence, as before.
    def validate():
        if any(not math.isfinite(value) or value <= 0 for value in [args.discovery_timeout, args.case_timeout]):
            parser.error("host deadlines must be finite and positive")
        if args.seeds < 1 or args.jobs < 1:
            parser.error("--seeds and --jobs must be positive")
        if args.from_seed is not None and not (0 <= args.from_seed <= args.from_seed + args.seeds - 1 < 2**64):
            parser.error("seed window must fit an unsigned 64-bit integer")
    validate()
    bundle = None
    try:
        bundle = Bundle(args)
        bundle.prepare(args)
        validate()
        return execute(args, bundle)
    except (OSError, ValueError, KeyError, TypeError) as error:
        if bundle:
            bundle.infrastructure_error(error)
        else:
            print(f"[simulation-infrastructure] FAIL: unable to create required evidence root: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
