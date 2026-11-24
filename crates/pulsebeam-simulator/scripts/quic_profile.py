"""Finite manual qualification using the same replay and kernel witnesses as QA."""

import argparse
import json
import math
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time


SAMPLES = 5
SAMPLE_PREFIX = "[simulation-generated] quic-profile "
FILTERS = {
    "baseline": "quic_media_opus_exchanges_with_webrtc_encoded_endpoints",
    "mixed": "quic_mixed_keeps_control_media_and_reliable_data_progressing",
    "recovery": "quic_mobility_replaces_failed_path_using_authenticated_placement",
}


def run(command, log, timeout, env=None):
    start = time.monotonic()
    with log.open("w") as output:
        subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, env=env,
                       timeout=timeout, check=True)
    return time.monotonic() - start


def quantiles(values):
    values = sorted(values)
    return {f"p{p}": values[math.ceil(len(values) * p / 100) - 1] for p in (50, 95, 99)}


def profile(args, root):
    # Run first so the children's high-water RSS belongs to this process alone.
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    kernel_seconds = run([args.kernel, "--exact", "node::transport_conformance::quic::"
                          "native_quic_kernel_v4_v6_preserve_association_on_rebind"],
                         root / "linux.log", 30)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if "1 passed" not in (root / "linux.log").read_text():
        raise ValueError("Linux kernel witness did not run")

    samples = {}
    env = os.environ | {"PULSEBEAM_QUIC_PROFILE": "1"}
    for name, selected in FILTERS.items():
        log = root / f"{name}.log"
        run([sys.executable, args.runner, args.sim_binary, "--candidate", args.candidate,
             "--filter", selected, "--jobs", "1", "--seeds", str(SAMPLES), "--from-seed", "1",
             "--discovery-timeout", "20", "--case-timeout", "15", "--artifacts-dir", str(root)],
            log, 110, env)
        read = [json.loads(line.removeprefix(SAMPLE_PREFIX)) for line in log.read_text().splitlines()
                if line.startswith(SAMPLE_PREFIX)]
        if len(read) != SAMPLES or any(sample["case"] != name for sample in read):
            raise ValueError(f"expected {SAMPLES} {name} samples, got {len(read)}")
        samples[name] = read

    return {
        "provenance": Path(args.candidate).read_text().strip(),
        "sample_count_per_case": SAMPLES,
        "virtual_time_microseconds": {
            name: {field: quantiles([sample[field] for sample in rows])
                   for field in ("media_us", "data_observed_us", "control_observed_us", "recovery_us")
                   if rows[0].get(field) is not None}
            for name, rows in samples.items()
        },
        "virtual_samples": samples,
        "linux_kernel_rebind_witness": {
            "wall_seconds": kernel_seconds,
            "user_cpu_seconds": after.ru_utime - before.ru_utime,
            "system_cpu_seconds": after.ru_stime - before.ru_stime,
            "max_rss_kib": after.ru_maxrss,
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualification", action="store_true", required=True)
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--sim-binary", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="quic-profile-", dir=args.output_dir)).resolve()
    print(f"QUIC qualification artifacts: {root}", flush=True)
    try:
        report = profile(args, root)
    except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
        parser.exit(1, f"QUIC qualification failed: {error}\n")
    output = json.dumps(report, indent=2, sort_keys=True)
    (root / "qualification.json").write_text(output + "\n")
    print(output)


if __name__ == "__main__":
    main()
