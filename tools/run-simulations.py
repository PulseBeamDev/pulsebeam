"""Run libtest simulations in separate processes, preserving nextest isolation."""

import argparse
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
    args = parser.parse_args()
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
                print(f"[simulation-result] SKIP {name}; documented gate exception in crates/pulsebeam-simulator/docs/native-dtls-exception.md", flush=True)
    if not selected:
        parser.error("no non-ignored simulations match the requested selection")

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
            result = subprocess.run([binary, "--exact", name, "--nocapture", "--test-threads=1"],
                                    env=env, stdout=trace, stderr=subprocess.STDOUT)
        return name, seed, result.returncode, time.perf_counter() - started, log

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
