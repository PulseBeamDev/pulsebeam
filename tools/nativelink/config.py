#!/usr/bin/env python3
"""Render a NativeLink v1.8.3 local execution config as JSON on stdout."""

import argparse
import json
from pathlib import Path


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def port(value):
    number = positive(value)
    if number > 65535:
        raise argparse.ArgumentTypeError("must be at most 65535")
    return number


def make_config(state, cpu, memory_kb, action_cpu, action_memory_kb, reapi_port, worker_port):
    state = Path(state)
    if not state.is_absolute() or "$" in str(state):
        raise ValueError("state must be an absolute path without '$'")
    if min(cpu, memory_kb, action_cpu, action_memory_kb) <= 0:
        raise ValueError("capacities and reservations must be positive")
    if action_cpu > cpu or action_memory_kb > memory_kb:
        raise ValueError("default reservation exceeds worker capacity")
    if not (1 <= reapi_port <= 65535 and 1 <= worker_port <= 65535) or reapi_port == worker_port:
        raise ValueError("ports must be distinct and in 1..65535")

    def filesystem(name, limit):
        return {"filesystem": {
            "content_path": str(state / name / "content"),
            "temp_path": str(state / name / "tmp"),
            "eviction_policy": {"max_bytes": limit},
        }}

    # The temporary names are cleared first so clients cannot inject them.
    # Restore supplied reservations after adding defaults for omitted values.
    modifications = []
    for name, default in (("cpu_count", action_cpu), ("memory_kb", action_memory_kb)):
        temporary = "pulsebeam_saved_" + name
        modifications.extend([
            {"remove": temporary},
            {"replace": {"name": name, "new_name": temporary}},
            {"add": {"name": name, "value": str(default)}},
            {"replace": {"name": temporary, "new_name": name}},
        ])

    cas, ac, fast, scheduler = "CAS_MAIN_STORE", "AC_MAIN_STORE", "WORKER_FAST_SLOW_STORE", "MAIN_SCHEDULER"
    return {
        "stores": [
            {"name": cas, **filesystem("cas", 64 * 1024**3)},
            {"name": ac, "completeness_checking": {
                "backend": filesystem("ac", 1024**3),
                "cas_store": {"ref_store": {"name": cas}},
            }},
            {"name": fast, "fast_slow": {
                "fast": filesystem("worker-cas", 8 * 1024**3),
                "slow": {"ref_store": {"name": cas}},
            }},
        ],
        "schedulers": [{"name": scheduler, "property_modifier": {
            "modifications": modifications,
            "scheduler": {"cache_lookup": {
                "ac_store": ac,
                "scheduler": {"simple": {"supported_platform_properties": {
                    "cpu_count": "minimum", "memory_kb": "minimum",
                    "OSFamily": "priority", "container-image": "priority",
                }}},
            }},
        }}],
        "workers": [{"local": {
            "worker_api_endpoint": {"uri": f"grpc://127.0.0.1:{worker_port}"},
            "cas_fast_slow_store": fast,
            "upload_action_result": {"ac_store": ac},
            "work_directory": str(state / "work"),
            "platform_properties": {
                "cpu_count": {"values": [str(cpu)]},
                "memory_kb": {"values": [str(memory_kb)]},
                "OSFamily": {"values": [""]},
                "container-image": {"values": [""]},
            },
        }}],
        "servers": [
            {"name": "local", "listener": {"http": {"socket_address": f"127.0.0.1:{reapi_port}"}},
             "services": {
                 "cas": [{"cas_store": cas}], "ac": [{"ac_store": ac}],
                 "bytestream": [{"cas_store": cas}],
                 "execution": [{"cas_store": cas, "scheduler": scheduler}],
                 "capabilities": [{"remote_execution": {"scheduler": scheduler}}],
             }},
            {"name": "worker_api", "listener": {"http": {"socket_address": f"127.0.0.1:{worker_port}"}},
             "services": {"worker_api": {"scheduler": scheduler}, "health": {}}},
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", required=True, help="private absolute state directory")
    parser.add_argument("--cpu", type=positive, required=True, help="worker CPU capacity")
    parser.add_argument("--memory-kb", type=positive, required=True, help="worker memory capacity in KiB")
    parser.add_argument("--action-cpu", type=positive, default=1)
    parser.add_argument("--action-memory-kb", type=positive, default=1_048_576)
    parser.add_argument("--reapi-port", type=port, default=50051)
    parser.add_argument("--worker-port", type=port, default=50061)
    args = parser.parse_args()
    try:
        config = make_config(args.state, args.cpu, args.memory_kb, args.action_cpu,
                             args.action_memory_kb, args.reapi_port, args.worker_port)
    except ValueError as error:
        parser.error(str(error))
    print(json.dumps(config, indent=2))


if __name__ == "__main__":
    main()
