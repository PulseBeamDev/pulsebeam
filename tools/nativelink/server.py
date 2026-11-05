"""Run the pinned local REAPI cache, scheduler and sandboxed worker in foreground."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import shlex
import stat

from config import make_config, port, positive


def sandbox_script(host, work):
    command = [
        str(host / "bwrap"), "--unshare-net", "--unshare-pid", "--die-with-parent",
        "--ro-bind", "/nix/store", "/nix/store",
        "--ro-bind", "/usr/lib", "/usr/lib",
        "--ro-bind", "/lib", "/lib", "--ro-bind", "/lib64", "/lib64",
        "--tmpfs", "/tmp",
        "--bind", str(work), str(work), "--proc", "/proc", "--dev", "/dev",
    ]
    return f'#!{host / "bash"}\nexec {shlex.join(command)} --chdir "$PWD" -- "$@"\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--host-bin-file", required=True)
    parser.add_argument("--state", type=Path, default=Path.home() / ".cache/pulsebeam/nativelink/v1")
    parser.add_argument("--cpu", type=positive, default=min(8, os.cpu_count() or 1))
    parser.add_argument("--memory-kb", type=positive, default=8_388_608)
    parser.add_argument("--reapi-port", type=port, default=50051)
    parser.add_argument("--worker-port", type=port, default=50061)
    args = parser.parse_args()
    state = args.state.absolute()
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = state.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        parser.error("state must be an owned private directory (mode 0700), not a symlink")
    lock = os.open(state / "server.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        parser.error("NativeLink is already using this state directory")
    host = Path(Path(args.host_bin_file).read_text().strip())
    work = state / "work"
    work.mkdir(exist_ok=True)
    entrypoint = state / "sandbox.sh"
    entrypoint.write_text(sandbox_script(host, work))
    entrypoint.chmod(0o700)
    config = make_config(str(state), args.cpu, args.memory_kb, 1, 1_048_576,
                         args.reapi_port, args.worker_port)
    worker = config["workers"][0]["local"]
    worker["entrypoint"] = str(entrypoint)
    worker["platform_properties"]["OSFamily"] = {"values": ["Linux"]}
    worker["platform_properties"]["pulsebeam-host-tools"] = {"values": [str(host)]}
    properties = config["schedulers"][0]["property_modifier"]["scheduler"]["cache_lookup"]["scheduler"]["simple"]["supported_platform_properties"]
    properties.update({"OSFamily": "exact", "pulsebeam-host-tools": "exact"})
    config_path = state / "config.json"
    config_path.write_text(json.dumps(config, indent=2))
    os.set_inheritable(lock, True)
    print(f"NativeLink REAPI grpc://127.0.0.1:{args.reapi_port}, capacity {args.cpu} CPU / {args.memory_kb} KiB", flush=True)
    print(f"State: {state}. Stop with Ctrl-C. Trusted local clients only.", flush=True)
    os.execve(os.path.abspath(args.binary), [args.binary, str(config_path)],
              {**os.environ, "PATH": str(host)})


if __name__ == "__main__":
    main()
