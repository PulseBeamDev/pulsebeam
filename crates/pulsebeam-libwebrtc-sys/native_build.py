"""Stage declared GN inputs and run the upstream native graph without network."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(command, *, cwd, env):
    subprocess.run([str(value) for value in command], cwd=cwd, env=env, check=True)


def writable_file(root, relative):
    path = root
    for part in Path(relative).parts[:-1]:
        path /= part
        if path.is_symlink():
            source = path.resolve()
            path.unlink()
            path.mkdir()
            for child in source.iterdir():
                (path / child.name).symlink_to(child, target_is_directory=child.is_dir())
    target = root / relative
    content = target.read_bytes()
    target.unlink()
    target.write_bytes(content)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source-root", "bridge", "overlay", "host-bin", "build", "flavor", "pulsebeam", "webrtc", "runtime"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--asan", action="store_true")
    args = parser.parse_args()
    source = Path(args.source_root).absolute()
    overlay = Path(args.overlay).absolute()
    bridge = Path(args.bridge).absolute()
    build = Path(args.build).absolute()
    host = Path(args.host_bin).absolute()
    build.mkdir(parents=True, exist_ok=True)
    env = {
        "PATH": str(host),
        "HOME": str(build / "home"),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PKG_CONFIG_PATH": "",
        "PKG_CONFIG_LIBDIR": "",
    }
    with tempfile.TemporaryDirectory(prefix="webrtc-gn-", dir=build.parent) as temporary:
        root = Path(temporary)
        for child in source.iterdir():
            (root / child.name).symlink_to(child, target_is_directory=child.is_dir())
        package = root / "pulsebeam"
        package.mkdir()
        for relative in ("BUILD.gn", "bridge.py", "strict-fp.py", "rust"):
            (package / relative).symlink_to(overlay / relative, target_is_directory=relative == "rust")
        owned = package / "pulsebeam-webrtc-sys"
        (owned / "src").mkdir(parents=True)
        (owned / "src/bridge.rs").symlink_to(bridge)
        (owned / "native").symlink_to(bridge.parent.parent / "native", target_is_directory=True)
        for relative in ("BUILD.gn", "buildtools/third_party/libc++/BUILD.gn", "buildtools/third_party/libc++abi/BUILD.gn"):
            writable_file(root, relative)
        run([host / "patch", "--batch", "--fuzz=0", "-p1", "-i", overlay / "integration.patch"], cwd=root, env=env)
        native = args.flavor == "native"
        config = (overlay / "args.gn").read_text()
        config += f"\npulsebeam_native = {str(native).lower()}\n"
        config += f"rtc_include_internal_audio_device = {str(native).lower()}\n"
        config += f"rtc_use_x11 = {str(native).lower()}\n"
        config += f"rtc_use_pipewire = {str(native).lower()}\n"
        if native:
            config += "rtc_link_pipewire = false\nrtc_include_pulse_audio = true\n"
        if args.asan:
            config += "is_asan = true\ndcheck_always_on = true\n"
        config += f'cc_wrapper = "{package / "strict-fp.py"}"\n'
        config += f'pkg_config = "{host / "pkg-config"}"\n'
        gn = root / "buildtools/linux64/gn"
        ninja = root / "third_party/ninja/ninja"
        run([gn, "gen", build, f"--root={root}", "--root-target=//pulsebeam:native", "--args=" + config], cwd=root, env=env)
        run([ninja, "-C", build, "-j8", "pulsebeam:native"], cwd=root, env=env)
        for name, relative in {
            "pulsebeam": "obj/pulsebeam/libpulsebeam.a",
            "webrtc": "obj/libwebrtc.a",
            "runtime": "obj/pulsebeam/libruntime.a",
        }.items():
            output = Path(getattr(args, name))
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(build / relative, output)


if __name__ == "__main__":
    main()
