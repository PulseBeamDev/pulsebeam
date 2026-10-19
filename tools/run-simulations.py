"""Run libtest simulations in separate processes, preserving nextest isolation."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import math
import os
from pathlib import Path
import signal
import shlex
import subprocess
import sys
import tempfile
import time


def run_process(command, trace, *, timeout, env=None):
    trace.write(f"command={shlex.join(command)}\n")
    trace.flush()
    try:
        process = subprocess.Popen(command, env=env, stdout=trace, stderr=subprocess.STDOUT,
                                   start_new_session=True)
    except OSError as error:
        trace.write(f"Unable to start child: {error}\n")
        return 127, False
    try:
        return process.wait(timeout=timeout), False
    except subprocess.TimeoutExpired:
        # Kill the session's process group, not just libtest: native helpers and
        # descendants must not outlive a failed discovery, owner or replay.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        trace.write(f"TIMEOUT after {timeout:g} host seconds; process group terminated\n")
        return process.returncode, True


def names(binary, logs, seed_label, timeout, *flags):
    command = [binary, "--list", "--format=terse", *flags]
    log = logs / ("discovery-ignored.log" if "--ignored" in flags else "discovery.log")
    with log.open("w") as trace:
        trace.write(f"case=discovery seed={seed_label}\n")
        status, timed_out = run_process(command, trace, timeout=timeout)
    if status or timed_out:
        print(f"[simulation-discovery] FAIL seed={seed_label} timeout={timed_out}; "
              f"command={shlex.join(command)}\nFailure trace: {log}", file=sys.stderr, flush=True)
        return None
    selected = [line.removesuffix(": test") for line in log.read_text().splitlines()
                if line.endswith(": test")]
    log.unlink()
    return selected




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
    parser.add_argument("--discovery-timeout", type=float, default=30, help="host-second deadline per discovery process")
    parser.add_argument("--case-timeout", type=float, default=600, help="host-second deadline per isolated case, including replay")
    args = parser.parse_args()
    if any(not math.isfinite(value) or value <= 0 for value in [args.discovery_timeout, args.case_timeout]):
        parser.error("host deadlines must be finite and positive")
    if args.seeds < 1 or args.jobs < 1:
        parser.error("--seeds and --jobs must be positive")
    if args.from_seed is not None and not (0 <= args.from_seed <= args.from_seed + args.seeds - 1 < 2**64):
        parser.error("seed window must fit an unsigned 64-bit integer")

    binary = str(Path(args.binary).resolve())
    output_root = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR")
    logs = Path(output_root) / "simulations" if output_root else Path(tempfile.mkdtemp(prefix="pulsebeam-simulations-"))
    logs.mkdir(parents=True, exist_ok=True)
    seed_label = "committed" if args.from_seed is None else str(args.from_seed)
    ignored = names(binary, logs, seed_label, args.discovery_timeout, "--ignored")
    if ignored is None:
        return 1
    listed = names(binary, logs, seed_label, args.discovery_timeout)
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
        parser.error("no non-ignored simulations match the requested selection")

    print(f"[simulation-run] cases={len(selected)} seeds={args.seeds} jobs={args.jobs}", flush=True)

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
            status, timed_out = run_process([binary, "--exact", name, "--nocapture", "--test-threads=1"],
                                            trace, env=env, timeout=args.case_timeout)
        return name, seed, status, timed_out, time.perf_counter() - started, log

    failed = False
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        cases = [(name, None if args.from_seed is None else args.from_seed + offset)
                 for offset in range(args.seeds) for name in selected]
        futures = [executor.submit(run, index, name, seed) for index, (name, seed) in enumerate(cases)]
        for future in as_completed(futures):
            name, seed, status, timed_out, elapsed, log = future.result()
            # Only this thread writes child evidence, keeping generated coverage
            # and explicit scoreboard traces adjacent to their case result.
            with log.open(errors="replace") as trace:
                for line in trace:
                    if args.show_output or line.startswith(("[simulation-generated]", "[simulation-premise]")):
                        sys.stdout.write(line)
            outcome = "FAIL" if status or timed_out else "PASS"
            seed_label = "committed" if seed is None else str(seed)
            print(f"[simulation-result] {outcome} {name} seed={seed_label} wall_seconds={elapsed:.3f}", flush=True)
            replay = ["./bazel", "run", "//:replay", "--"]
            if name in args.skip:
                replay.append("--run-skipped")
            if seed is not None:
                replay += ["--seed", str(seed)]
            replay += ["--exact", "--filter", name]
            print(f"Replay: {shlex.join(replay)}", flush=True)
            if status or timed_out:
                failed = True
                if timed_out:
                    print(f"Host timeout: {args.case_timeout:g}s; child process group terminated", flush=True)
                print(f"Failure trace: {log}", flush=True)
            else:
                log.unlink()
    if not any(logs.iterdir()):
        logs.rmdir()
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
