"""Run libtest simulations in separate processes, preserving nextest isolation."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path
import subprocess
import sys
import time


def names(binary, *flags):
    listed = subprocess.run([binary, "--list", "--format=terse", *flags], check=True, text=True, capture_output=True)
    return [line.removesuffix(": test") for line in listed.stdout.splitlines() if line.endswith(": test")]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--mode", choices=["fast", "slow", "all"], default="fast")
    parser.add_argument("--filter", default="")
    parser.add_argument("--skip", action="append", default=[], help="exact documented gate exception")
    parser.add_argument("--run-skipped", action="store_true", help="explicitly execute documented gate exceptions")
    parser.add_argument("--seeds", type=int, default=1)
    parser.add_argument("--from-seed", "--seed", dest="from_seed", type=int)
    parser.add_argument("--jobs", type=int, default=min(16, os.cpu_count() or 1))
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    ignored = set(names(binary, "--ignored"))
    selected = [name for name in names(binary) if name not in ignored and args.filter in name
                and (args.mode == "all" or ("::slow::" in name) == (args.mode == "slow"))]
    if not args.run_skipped:
        for name in args.skip:
            if name in selected:
                selected.remove(name)
                print(f"[simulation-result] SKIP {name}; documented gate exception in crates/pulsebeam-simulator/docs/native-dtls-exception.md", flush=True)
    if not selected:
        parser.error("no non-ignored simulations match the requested selection")
    if args.seeds < 1 or args.jobs < 1:
        parser.error("--seeds and --jobs must be positive")
    if args.from_seed is not None and not (0 <= args.from_seed <= args.from_seed + args.seeds - 1 < 2**64):
        parser.error("seed window must fit an unsigned 64-bit integer")

    print(f"[simulation-run] cases={len(selected)} seeds={args.seeds} jobs={args.jobs} cpus={os.cpu_count()}", flush=True)

    def run(name, seed):
        env = os.environ.copy()
        if seed is not None:
            env["PULSEBEAM_SIM_SEED"] = str(seed)
        # Measure in the runner, outside the child's virtualized clock.
        started = time.perf_counter()
        print(f"[simulation-timing] START {name} seed={seed if seed is not None else 'default'}", flush=True)
        result = subprocess.run([binary, "--exact", name, "--nocapture", "--test-threads=1"], env=env)
        print(f"[simulation-timing] END {name} wall_seconds={time.perf_counter() - started:.3f}", flush=True)
        outcome = "PASS" if result.returncode == 0 else "FAIL"
        print(f"[simulation-result] {outcome} {name}", flush=True)
        if result.returncode:
            seed_arg = "" if seed is None else f" --seed {seed}"
            exception_arg = " --run-skipped" if name in args.skip else ""
            print(f"Replay: ./bazel run //:replay --{exception_arg}{seed_arg} --filter {name}", flush=True)
        return result.returncode != 0

    failed = False
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        futures = [executor.submit(run, name, None if args.from_seed is None else args.from_seed + offset)
                   for offset in range(args.seeds) for name in selected]
        for future in as_completed(futures):
            failed |= future.result()
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
