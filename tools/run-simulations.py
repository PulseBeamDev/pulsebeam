"""Run libtest simulations in separate processes, preserving nextest isolation."""

import argparse
import hashlib
import json
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time


def names(binary, *flags):
    listed = subprocess.run([binary, "--list", "--format=terse", *flags], check=True, text=True, capture_output=True)
    return [line.removesuffix(": test") for line in listed.stdout.splitlines() if line.endswith(": test")]


BOUNDED_CASE = "tests::exploration::bounded_cross_shard"
BOUNDED_DOMAINS = {
    "application": {str(list(order)) for order in ((0, 1, 2), (0, 2, 1), (1, 0, 2), (1, 2, 0), (2, 0, 1), (2, 1, 0))},
    "fault": {"none", "udp-loss", "udp-duplicate", "controller-stall", "wrong-owner", "materialization-failure"},
    "host/declarations": {"canonical", "reverse"},
    "host/rejoin": {"canonical", "reverse"},
    "timing": {"nominal", "delay-3100ms"},
    "udp-pair": {"forward", "reverse"},
}


def choice_family(site):
    parts = site.split("/")
    return "/".join(parts[:2]) if parts[0] == "host" else parts[0]


def traverse(execute, max_paths, max_seconds, required):
    """Stateless DFS of exact decision prefixes, without state equivalence pruning."""
    pending = [[]]
    completed = 0
    coverage = {}
    disabled = {}
    consumed = {}
    scenario = None
    started = time.perf_counter()
    while pending:
        if completed >= max_paths or time.perf_counter() - started >= max_seconds:
            return {"status": "incomplete", "completed": completed, "pending": len(pending),
                    "coverage": coverage, "disabled": disabled, "consumed": consumed,
                    "scenario": scenario, "pruned": {}}
        prefix = pending.pop()
        artifact = execute(prefix, completed)
        if artifact["outcome"] != "satisfied":
            return {"status": artifact["outcome"], "completed": completed,
                    "pending": len(pending), "artifact": artifact, "coverage": coverage, "disabled": disabled}
        scenario = artifact.get("scenario", scenario)
        choices = artifact["choices"]
        if choices[:len(prefix)] != prefix:
            return {"status": "replay-divergence", "completed": completed, "diagnostic": "prefix not consumed exactly"}
        for index in range(len(prefix), len(choices)):
            choice = choices[index]
            enabled = choice["enabled"]
            if not enabled or len(enabled) != len(set(enabled)) or choice["selected"] != 0:
                return {"status": "invalid-model", "completed": completed, "diagnostic": "invalid unforced choice"}
            for selection in reversed(range(1, len(enabled))):
                sibling = dict(choice, selected=selection)
                pending.append(choices[:index] + [sibling])
        for choice in choices:
            # Occurrence identities remain in artifacts. Coverage is grouped by
            # the declared family, not packet bytes or scheduler quantum.
            family = choice_family(choice["site"])
            coverage.setdefault(family, {})
            selected = choice["enabled"][choice["selected"]]
            coverage[family][selected] = coverage[family].get(selected, 0) + 1
        for reason in artifact["disabled"]:
            disabled[reason] = disabled.get(reason, 0) + 1
        for fault in artifact.get("consumed", []):
            site = "/".join(fault.split("/")[:2])
            consumed[site] = consumed.get(site, 0) + 1
        completed += 1
    missing = {family: sorted(domain - set(coverage.get(family, {})))
               for family, domain in required.items() if set(coverage.get(family, {})) != domain}
    return {"status": "invalid-model" if missing else "complete", "completed": completed,
            "pending": 0, "pruned": {}, "coverage": coverage, "disabled": disabled,
            "consumed": consumed, "scenario": scenario, "missing": missing}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--mode", choices=["fast", "slow", "all"], default="fast")
    parser.add_argument("--filter", default="")
    parser.add_argument("--exact", action="store_true", help="match the complete filtered case name")
    parser.add_argument("--skip", action="append", default=[], help="exact documented gate exception")
    parser.add_argument("--run-skipped", action="store_true", help="explicitly execute documented gate exceptions")
    parser.add_argument("--seeds", type=int, default=1)
    parser.add_argument("--from-seed", "--seed", dest="from_seed", type=int, default=os.environ.get("PULSEBEAM_SIM_SEED"))
    # Bazel owner targets pass their reserved CPU budget explicitly. Standalone
    # replay can use the host, and an explicit --jobs always wins.
    parser.add_argument("--jobs", type=int, default=1 if "TEST_TMPDIR" in os.environ else min(16, os.cpu_count() or 1))
    parser.add_argument("--show-output", action="store_true", help="emit complete, grouped case traces for diagnostics or scoreboard metrics")
    parser.add_argument("--bounded-replay", type=Path, help="strict replay of one materialized bounded artifact")
    parser.add_argument("--bounded-max-paths", type=int, default=4096, help="operational cutoff, never a model bound")
    parser.add_argument("--bounded-max-seconds", type=float, default=600, help="operational traversal budget")
    args = parser.parse_args()
    if args.bounded_max_paths < 1 or args.bounded_max_seconds <= 0:
        parser.error("bounded operational budgets must be positive")
    if args.bounded_replay:
        args.bounded_replay = args.bounded_replay.resolve()
        args.filter = BOUNDED_CASE
        args.exact = True
    if args.seeds < 1 or args.jobs < 1:
        parser.error("--seeds and --jobs must be positive")
    if args.from_seed is not None and not (0 <= args.from_seed <= args.from_seed + args.seeds - 1 < 2**64):
        parser.error("seed window must fit an unsigned 64-bit integer")

    binary = str(Path(args.binary).resolve())
    ignored = set(names(binary, "--ignored"))
    selected = [name for name in names(binary) if name not in ignored
                and (name == args.filter if args.exact else args.filter in name)
                and (args.mode == "all" or ("::slow::" in name) == (args.mode == "slow"))]
    if not args.run_skipped:
        for name in args.skip:
            if name in selected:
                selected.remove(name)
                reason = ("documented gate exception in crates/pulsebeam-simulator/docs/native-dtls-exception.md"
                          if name == "tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close"
                          else "explicit --skip selection")
                print(f"[simulation-result] SKIP {name}; {reason}", flush=True)
    if not selected:
        parser.error("no non-ignored simulations match the requested selection")

    if BOUNDED_CASE in selected and (args.from_seed is not None or args.seeds != 1):
        if args.filter or args.bounded_replay:
            parser.error("bounded exploration fixes its materialized seed; select seeded cases for sweeps")
        selected.remove(BOUNDED_CASE)
        print("[bounded-model] seed sweep selects seeded cases only; bounded acceptance uses its fixed model", flush=True)
        if not selected:
            parser.error("no seeded simulations match the requested selection")
    build_identity = None
    if BOUNDED_CASE in selected:
        with open(binary, "rb") as executable:
            build_identity = hashlib.file_digest(executable, "sha256").hexdigest()
    print(f"[simulation-run] cases={len(selected)} seeds={args.seeds} jobs={args.jobs}", flush=True)
    output_root = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR")
    logs = Path(output_root) / "simulations" if output_root else Path(tempfile.mkdtemp(prefix="pulsebeam-simulations-"))
    logs.mkdir(parents=True, exist_ok=True)

    def run(index, name, seed):
        env = os.environ.copy()
        if seed is not None:
            env["PULSEBEAM_SIM_SEED"] = str(seed)
        seed_label = "committed" if seed is None else str(seed)
        log = logs / f"case-{index:04d}-seed-{seed_label}.log"
        # Keep full DEBUG output on disk, not in memory or interleaved across
        # processes. Measure outside the child's virtualized clock.
        started = time.perf_counter()
        with log.open("w") as trace:
            trace.write(f"case={name} seed={seed_label}\n")
            trace.flush()
            command = [binary, "--exact", name, "--nocapture", "--test-threads=1"]
            if name != BOUNDED_CASE:
                result = subprocess.run(command, env=env, stdout=trace, stderr=subprocess.STDOUT)
                status = result.returncode
            else:
                artifacts = logs / f"bounded-{index:04d}"
                artifacts.mkdir()
                for key in ("PREFIX", "REPLAY", "OUTPUT"):
                    env.pop("PULSEBEAM_BOUNDED_" + key, None)
                env["PULSEBEAM_BOUNDED_BUILD"] = build_identity

                def execute(prefix, branch, replay=None):
                    prefix_path = artifacts / "prefix.json"
                    output_path = artifacts / f"branch-{branch:04d}.json"
                    prefix_path.write_text(json.dumps(prefix))
                    child_env = dict(env, PULSEBEAM_BOUNDED_PREFIX=str(prefix_path),
                                     PULSEBEAM_BOUNDED_OUTPUT=str(output_path))
                    replay = replay or args.bounded_replay
                    if replay:
                        child_env["PULSEBEAM_BOUNDED_REPLAY"] = str(replay)
                    trace.write(f"bounded branch={branch} prefix_depth={len(prefix)}\n")
                    trace.flush()
                    try:
                        child = subprocess.run(command, env=child_env, stdout=trace,
                                               stderr=subprocess.STDOUT,
                                               timeout=min(180, max(0.001, args.bounded_max_seconds - (time.perf_counter() - started))))
                    except subprocess.TimeoutExpired:
                        return {"outcome": "incomplete", "diagnostic": "isolated execution operational deadline"}
                    if not output_path.exists():
                        return {"outcome": "infrastructure-failure", "diagnostic": "missing execution artifact"}
                    try:
                        artifact = json.loads(output_path.read_text())
                        assert isinstance(artifact["choices"], list)
                        assert isinstance(artifact["observations"], list)
                        assert isinstance(artifact["consumed"], list)
                        assert isinstance(artifact["disabled"], list)
                        assert isinstance(artifact["outcome"], str)
                        assert isinstance(artifact["scenario"], dict)
                        assert all(isinstance(artifact[key], str) for key in ("build", "dependencies", "fixture"))
                        assert artifact["outcome"] in {"satisfied", "counterexample", "incomplete", "invalid-model",
                                                       "replay-divergence", "infrastructure-failure"}
                        assert all(isinstance(value, str) for key in ("observations", "consumed", "disabled")
                                   for value in artifact[key])
                        assert all(isinstance(choice["site"], str)
                                   and isinstance(choice["enabled"], list) and choice["enabled"]
                                   and all(isinstance(value, str) for value in choice["enabled"])
                                   and len(choice["enabled"]) == len(set(choice["enabled"]))
                                   and type(choice["selected"]) is int
                                   and 0 <= choice["selected"] < len(choice["enabled"])
                                   for choice in artifact["choices"])
                    except (OSError, ValueError, KeyError, TypeError, AssertionError) as error:
                        return {"outcome": "infrastructure-failure", "diagnostic": f"invalid execution artifact: {error}"}
                    if artifact["build"] != build_identity:
                        return {"outcome": "replay-divergence", "diagnostic": "wrong build identity"}
                    if child.returncode and artifact["outcome"] == "satisfied":
                        return {"outcome": "infrastructure-failure", "diagnostic": "child failed after reporting success"}
                    artifact["path"] = str(output_path)
                    return artifact

                if args.bounded_replay:
                    artifact = execute([], 0)
                    report = {"status": "replayed" if artifact["outcome"] == "satisfied" else artifact["outcome"],
                              "artifact": artifact}
                    status = int(report["status"] != "replayed")
                else:
                    report = traverse(execute, args.bounded_max_paths, args.bounded_max_seconds,
                                      BOUNDED_DOMAINS)
                    if report["status"] == "complete":
                        # Reuse the first materialized leaf as the fresh-process
                        # replay witness rather than adding another scenario.
                        replayed = execute([], report["completed"], artifacts / "branch-0000.json")
                        report["replay"] = replayed
                        if replayed["outcome"] != "satisfied":
                            report["status"] = replayed["outcome"]
                            report["artifact"] = replayed
                    status = int(report["status"] != "complete")
                report["wall_seconds"] = time.perf_counter() - started
                report_path = artifacts / "report.json"
                report_path.write_text(json.dumps(report, indent=2) + "\n")
                print(f"[bounded-result] {report['status']} completed={report.get('completed', 0)} wall_seconds={report['wall_seconds']:.3f} report={report_path}", flush=True)
                if status and report.get("artifact", {}).get("path"):
                    print(f"Bounded replay: ./bazel run //:replay -- --bounded-replay {shlex.quote(report['artifact']['path'])}", flush=True)
        return name, seed, status, time.perf_counter() - started, log

    failed = False
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        cases = [(name, None if args.from_seed is None else args.from_seed + offset)
                 for offset in range(args.seeds) for name in selected]
        futures = [executor.submit(run, index, name, seed) for index, (name, seed) in enumerate(cases)]
        for future in as_completed(futures):
            name, seed, status, elapsed, log = future.result()
            if args.show_output:
                # Only this thread writes child traces, so scoreboard metrics
                # stay adjacent to their case result even with multiple workers.
                with log.open(errors="replace") as trace:
                    for line in trace:
                        sys.stdout.write(line)
            outcome = "FAIL" if status else "PASS"
            seed_label = "committed" if seed is None else str(seed)
            print(f"[simulation-result] {outcome} {name} seed={seed_label} wall_seconds={elapsed:.3f}", flush=True)
            replay = ["./bazel", "run", "//:replay", "--"]
            if name in args.skip:
                replay.append("--run-skipped")
            if seed is not None:
                replay += ["--seed", str(seed)]
            replay += ["--exact", "--filter", name]
            print(f"Replay: {shlex.join(replay)}", flush=True)
            if status:
                failed = True
                print(f"Failure trace: {log}", flush=True)
            else:
                log.unlink()
    if not any(logs.iterdir()):
        logs.rmdir()
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
