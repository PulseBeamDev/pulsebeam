"""Run the pinned local REAPI cache, scheduler and sandboxed worker in foreground."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import shlex
import stat

from config import make_config, port, positive
from resources import cache_budget, machine_budget


def sandbox_script(host, work, runtime):
    command = [
        str(host / "bwrap"), "--unshare-net", "--unshare-pid", "--die-with-parent",
        "--ro-bind", "/nix/store", "/nix/store",
        "--ro-bind", "/usr/lib", "/usr/lib",
        "--ro-bind", "/lib", "/lib", "--ro-bind", "/lib64", "/lib64",
        "--ro-bind", str(host), "/bin",
        "--ro-bind", str(runtime / "bin"), "/usr/bin",
        "--ro-bind", str(runtime / "etc"), "/etc",
        "--tmpfs", "/tmp",
        "--bind", str(work), str(work), "--proc", "/proc", "--dev", "/dev",
    ]
    # Bazel may set PWD=/proc/self/cwd; that resolves differently after /proc
    # is remounted in the worker's PID namespace.
    return (f'#!{host / "bash"}\n'
            'export SSL_CERT_FILE="${SSL_CERT_FILE-/etc/ssl/certs/ca-certificates.crt}"\n'
            f'exec {shlex.join(command)} --chdir "$(pwd -P)" -- "$@"\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--host-bin-file", required=True)
    parser.add_argument("--runtime-root-file", required=True)
    parser.add_argument("--state", type=Path, default=Path.home() / ".cache/pulsebeam/nativelink/v1")
    parser.add_argument("--cpu", type=positive)
    parser.add_argument("--memory-kb", type=positive)
    parser.add_argument("--cache-bytes", type=positive, help="eviction target per CAS tier; default: 10%% of the state filesystem")
    parser.add_argument("--reapi-port", type=port, default=50051)
    parser.add_argument("--worker-port", type=port, default=50061)
    args = parser.parse_args()
    state = args.state.absolute()
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = state.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        parser.error("state must be an owned private directory (mode 0700), not a symlink")
    try:
        if args.cpu is None or args.memory_kb is None:
            cpu, memory_kb = machine_budget()
            args.cpu = cpu if args.cpu is None else args.cpu
            args.memory_kb = memory_kb if args.memory_kb is None else args.memory_kb
        if args.cache_bytes is None:
            args.cache_bytes = cache_budget(state)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    lock = os.open(state / "server.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        parser.error("NativeLink is already using this state directory")
    host = Path(Path(args.host_bin_file).read_text().strip())
    runtime = Path(Path(args.runtime_root_file).read_text().strip())
    work = state / "work"
    work.mkdir(exist_ok=True)
    entrypoint = state / "sandbox.sh"
    entrypoint.write_text(sandbox_script(host, work, runtime))
    entrypoint.chmod(0o700)
    config = make_config(str(state), args.cpu, args.memory_kb, 1, 1_048_576,
                         args.reapi_port, args.worker_port, cache_bytes=args.cache_bytes)
    worker = config["workers"][0]["local"]
    worker["entrypoint"] = str(entrypoint)
    worker["platform_properties"]["OSFamily"] = {"values": ["Linux"]}
    worker["platform_properties"]["pulsebeam-host-tools"] = {"values": [str(host)]}
    worker["platform_properties"]["pulsebeam-runtime"] = {"values": [str(runtime)]}
    properties = config["schedulers"][0]["property_modifier"]["scheduler"]["cache_lookup"]["scheduler"]["simple"]["supported_platform_properties"]
    properties.update({"OSFamily": "exact", "pulsebeam-host-tools": "exact", "pulsebeam-runtime": "exact"})
    config_path = state / "config.json"
    config_path.write_text(json.dumps(config, indent=2))
    os.set_inheritable(lock, True)
    print(f"NativeLink REAPI grpc://127.0.0.1:{args.reapi_port}, scheduler capacity {args.cpu} CPU / {args.memory_kb} KiB", flush=True)
    print(f"Cache eviction targets: {args.cache_bytes} bytes per CAS tier, {max(1, args.cache_bytes // 100)} bytes for action results", flush=True)
    print(f"State: {state}. Stop with Ctrl-C. Trusted local clients only.", flush=True)
    os.execve(os.path.abspath(args.binary), [args.binary, str(config_path)],
              {**os.environ, "PATH": str(host)})


if __name__ == "__main__":
    main()
