"""Measure quiet video admission and membership churn in isolated simulator processes."""

import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile


CASE = "tests::native_runtime::application_profile::application_subscription_profile"
PREFIX = "application-profile "


def sample(args, root, mode, offered, run):
    log = root / f"{mode}-{offered}-{run}.log"
    costs = log.with_suffix(".cost")
    env = os.environ | {
        "PULSEBEAM_PROFILE_MODE": mode,
        "PULSEBEAM_PROFILE_OFFERED": str(offered),
        "RUST_LOG": os.environ.get("RUST_LOG", "error"),
    }
    with log.open("w") as output:
        subprocess.run(
            ["/usr/bin/time", "-f", "%U %S %M %e", "-o", str(costs),
             "/usr/bin/timeout", "--foreground", "--kill-after=5s", "70s",
             args.sim_binary, "--ignored", "--exact", CASE, "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, check=True,
        )
    lines = log.read_text().splitlines()
    rows = [json.loads(line[len(PREFIX):]) for line in lines if line.startswith(PREFIX)]
    if len(rows) != 1 or not any("1 passed" in line for line in lines):
        raise ValueError(f"missing profile evidence: {log}")
    user, system, rss, wall = map(float, costs.read_text().split())
    return rows[0] | {"run": run, "cpu_seconds": user + system,
                      "rss_kib": int(rss), "wall_seconds": wall}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sim-binary", required=True)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--offered", type=int, choices=(1, 16, 100, 101))
    parser.add_argument("--mode", choices=("exact", "set"))
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    root = Path(tempfile.mkdtemp(prefix="application-subscription-profile-"))
    print(f"Profile artifacts: {root}", flush=True)
    groups = defaultdict(list)
    try:
        for run in range(1, args.runs + 1):
            offered_counts = (args.offered,) if args.offered else (1, 16, 100, 101)
            modes = (args.mode,) if args.mode else (
                ("exact", "set") if run % 2 else ("set", "exact")
            )
            for offered in offered_counts:
                for mode in modes:
                    row = sample(args, root, mode, offered, run)
                    groups[offered, mode].append(row)
                    print(json.dumps(row, sort_keys=True), flush=True)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if logs := sorted(root.glob("*.log"), key=lambda path: path.stat().st_mtime):
            print("\n".join(logs[-1].read_text().splitlines()[-20:]), flush=True)
        parser.exit(1, f"Profile failed: {error}. See {root}\n")
    summary = [{"offered_publications": offered, "mode": mode, **{
        f"median_{metric}": statistics.median(row[metric] for row in samples)
        for metric in ("cpu_seconds", "rss_kib", "wall_seconds",
                       "initial_admission_us", "churn_us")
    }} for (offered, mode), samples in sorted(groups.items())]
    report = {"provenance": args.candidate.read_text().strip(), "runs": args.runs,
              "summary": summary, "samples": [r for rows in groups.values() for r in rows]}
    (root / "profile.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
