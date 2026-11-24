"""Stage declared GN inputs and execute the upstream GN graph."""

import argparse
from contextlib import contextmanager
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import types
from urllib.parse import urlparse


@contextmanager
def native_build_lock(path):
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit(
                "WebRTC native build blocked: another WebRTC build is running on this machine. "
                "Do not retry in a loop; continue independent work and retry after it finishes. "
                "If this is an intentional native change, check that the rebuild is necessary."
            ) from None
        yield fd
    finally:
        os.close(fd)


def run(command, *, cwd, env, lock_fd=None):
    descriptors = () if lock_fd is None else (lock_fd,)
    subprocess.run([str(value) for value in command], cwd=cwd, env=env, check=True, pass_fds=descriptors)


def reapi_address(value):
    endpoint = urlparse(value)
    try:
        port = endpoint.port
    except ValueError as error:
        raise argparse.ArgumentTypeError(str(error)) from error
    if endpoint.scheme != "grpc" or endpoint.hostname not in ("localhost", "127.0.0.1", "::1") or not port:
        raise argparse.ArgumentTypeError("WebRTC REAPI requires grpc://<loopback-host>:<port>")
    if endpoint.path or endpoint.query or endpoint.fragment or endpoint.username or endpoint.password:
        raise argparse.ArgumentTypeError("WebRTC REAPI endpoint must contain only a loopback host and port")
    return endpoint.netloc


def siso_command(siso, build, executor, endpoint, root_target="pulsebeam:native"):
    local_jobs = 2 if executor == "reapi" else 8
    command = [siso, "ninja", "-C", build, f"-local_jobs={local_jobs}", "-remote_jobs=32", "-k=8", "-load=@config//pulsebeam.star"]
    if executor == "local":
        command += ["-offline"]
    else:
        command += ["-reapi_address=" + endpoint, "-reapi_instance=", "-reapi_insecure"]
        if executor == "cache":
            command += ["-re_exec_enable=false", "-re_cache_enable_write"]
        else:
            command += ["-strict_remote", "-config=remote-link"]
    return command + [root_target]


def stage_file(root, relative, content=None):
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
    if content is None:
        content = target.read_bytes()
        target.unlink()
    elif target.exists() or target.is_symlink():
        raise ValueError(f"Owned build input shadows upstream file: {relative}")
    target.write_bytes(content)


def check_cxx_abi(owned_manifest, source_root):
    with Path(owned_manifest).open("rb") as manifest_file:
        owned = tomllib.load(manifest_file)
    dependency = owned.get("dependencies", {}).get("cxx")
    owned_pin = dependency.get("version") if isinstance(dependency, dict) else dependency
    if not isinstance(owned_pin, str) or not owned_pin.startswith("="):
        raise ValueError("owned Cargo manifest must declare cxx with an exact =version pin")

    vendor = Path(source_root) / "third_party/rust/chromium_crates_io/vendor"
    for crate, role in (("cxx", "runtime"), ("cxxbridge-cmd", "generator")):
        sdk_manifest = vendor / (crate + "-v1") / "Cargo.toml"
        with sdk_manifest.open("rb") as manifest_file:
            sdk_version = tomllib.load(manifest_file).get("package", {}).get("version")
        if not isinstance(sdk_version, str):
            raise ValueError(f"SDK CXX {role} manifest has no package.version: {sdk_manifest}")
        if owned_pin[1:] != sdk_version:
            raise SystemExit(
                "WebRTC CXX ABI preflight failed: owned Rust cxx pin "
                f"{owned_pin[1:]} does not match SDK CXX {role} version {sdk_version}"
            )


def runtime_objects(targets, root="//pulsebeam:runtime"):
    objects = set()
    visited = set()

    def visit(label):
        if label in visited:
            return
        visited.add(label)
        target = targets[label]
        kind = target["type"]
        if kind not in ("group", "source_set", "static_library"):
            if kind in ("shared_library", "loadable_module"):
                raise ValueError(f"C++ runtime is not statically packageable: {label}")
            return
        for outputs in target.get("source_outputs", {}).values():
            for output in outputs:
                if output.endswith(".o"):
                    path = Path(output)
                    if path.is_absolute() or ".." in path.parts:
                        raise ValueError(f"Runtime object escapes build directory: {output}")
                    objects.add(output)
        for dependency in target.get("deps", []):
            visit(dependency)

    if targets[root]["type"] == "executable":
        for dependency in targets[root].get("deps", []):
            visit(dependency)
    else:
        visit(root)
    if not objects:
        raise ValueError("GN reported no C++ runtime objects")
    return sorted(objects)


def repository_sources(value):
    # Linux sandbox inputs retain the source-file symlink into the immutable
    # repository cache. Its content manifest identifies the sibling checkout.
    try:
        source = Path(value).resolve(strict=True).parent / "checkout/src"
    except OSError as error:
        raise argparse.ArgumentTypeError(str(error)) from error
    if not source.is_dir():
        raise argparse.ArgumentTypeError(
            "WebRTC sources are missing beside their repository manifest; "
            "copied/hardlinked sandbox input staging is unsupported: " + str(source)
        )
    return source


def write_notices(gn, root, build, source, env, notices, manifest):
    # Reuse the pinned upstream mapping, but invoke the exact GN binary used for
    # this build instead of its depot_tools wrapper.
    sys.modules["find_depot_tools"] = types.ModuleType("find_depot_tools")
    script = source / "tools_webrtc/libs/generate_licenses.py"
    spec = importlib.util.spec_from_file_location("webrtc_license_generator", script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    builder = module.LicenseBuilder([str(build)], ["//pulsebeam:native"])
    def describe(target):
        result = subprocess.run(
            [str(gn), "desc", "--all", "--format=json", "--root-target=//pulsebeam:native", str(build), target],
            cwd=root, env=env, text=True, stdout=subprocess.PIPE,
        )
        if result.returncode:
            raise RuntimeError(f"GN license graph query failed:\n{result.stdout}")
        return json.loads(result.stdout)

    graph = describe("//pulsebeam:native")
    libraries = {}
    for target in graph.values():
        for dependency in target["deps"]:
            name = builder._parse_library(dependency)
            if name:
                libraries.setdefault(name, set()).add(dependency)
    if not libraries:
        raise RuntimeError("WebRTC GN license closure is empty")
    # GN's cxxbridge and its Rust dependencies are host-only build tools.
    host_toolchain = "(//build/toolchain/linux:clang_x64_for_rust_host_build_tools)"
    if "rust" in libraries:
        if any(not label.endswith(host_toolchain) for label in libraries["rust"]):
            raise RuntimeError(f"unmapped GN Rust runtime dependency: {sorted(libraries['rust'])}")
        builder.common_licenses_dict["rust"] = []
    direct_sources = {item for target in describe("//pulsebeam:pulsebeam").values()
                      for item in target.get("sources", []) if item.startswith("//third_party/")}
    if direct_sources:
        raise RuntimeError(f"unmapped direct third-party GN sources: {sorted(direct_sources)}")
    # The C++-only SDK target uses the Rust crate\'s dual-license mapping.
    libraries.setdefault("rust/cxx/v1", set()).add("//build/rust:cxx_cppdeps_lib")
    builder.common_licenses_dict["rust/cxx/v1"] = sorted(set(
        builder.common_licenses_dict["rust/cxx/v1"]
        + ["third_party/rust/chromium_crates_io/vendor/cxx-v1/LICENSE-MIT"]
    ))
    # These configured stock codecs are omitted from WebRTC's license table;
    # use their pinned Chromium license declarations, not a downstream mapping.
    for name in ("ffmpeg", "openh264"):
        if name in libraries and name not in builder.common_licenses_dict:
            package = source / "third_party" / name
            fields = [line.removeprefix("License File:").strip()
                      for line in (package / "README.chromium").read_text().splitlines()
                      if line.startswith("License File:")]
            if len(fields) != 1 or not fields[0]:
                raise RuntimeError(f"Missing Chromium license declaration for {name}")
            paths = [Path(value.strip()) for value in fields[0].split(",")]
            if any(path.is_absolute() or ".." in path.parts for path in paths):
                raise RuntimeError(f"Chromium license path escapes package: {name}")
            builder.common_licenses_dict[name] = [str(Path("third_party") / name / path) for path in paths]
    missing = sorted(set(libraries) - set(builder.common_licenses_dict))
    if missing:
        raise RuntimeError(f"WebRTC GN libraries without upstream license mapping: {[(name, sorted(libraries[name])) for name in missing]}")
    entries = [{"library": name, "targets": sorted(targets), "licenses": builder.common_licenses_dict[name]}
               for name, targets in sorted(libraries.items())]
    Path(manifest).write_text(json.dumps(entries, indent=2, sort_keys=True) + "\n")
    pieces = ["# WebRTC and configured GN dependencies\n"]
    license_paths = [("WebRTC", "LICENSE"), ("WebRTC patent grant", "PATENTS")]
    license_paths.extend((entry["library"], path) for entry in entries for path in entry["licenses"])
    for name, relative in license_paths:
        text = (source / relative).read_text()
        if not text.strip():
            raise RuntimeError(f"empty WebRTC license: {relative}")
        pieces.append(f"\n## {name}: {relative}\n\n{text.rstrip()}\n")
    Path(notices).write_text("".join(pieces))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sources = parser.add_mutually_exclusive_group(required=True)
    sources.add_argument("--source-root")
    sources.add_argument("--source-manifest", dest="source_root", type=repository_sources)
    for name in ("bridge", "overlay", "host-bin", "build", "flavor", "owned-cargo-manifest"):
        parser.add_argument("--" + name, required=True)
    for name in ("pulsebeam", "webrtc", "runtime", "notices", "license-manifest", "capture-source", "capture-output"):
        parser.add_argument("--" + name)
    parser.add_argument("--asan", action="store_true")
    parser.add_argument("--executor", choices=("local", "cache", "reapi"), default="local")
    parser.add_argument("--reapi", type=reapi_address, default="grpc://127.0.0.1:50051")
    args = parser.parse_args()
    if args.capture_source or args.capture_output:
        if not (args.capture_source and args.capture_output):
            parser.error("capture requires both --capture-source and --capture-output")
    elif not (args.pulsebeam and args.webrtc and args.runtime and args.notices and args.license_manifest):
        parser.error("native build requires --pulsebeam, --webrtc, --runtime, --notices and --license-manifest")
    if args.executor == "reapi":
        build(args)
    else:
        with native_build_lock(f"/dev/shm/pulsebeam-webrtc-build-{os.getuid()}.lock") as lock_fd:
            build(args, lock_fd)


def build(args, lock_fd=None):
    source = Path(args.source_root).absolute()
    check_cxx_abi(args.owned_cargo_manifest, source)
    overlay = Path(args.overlay).absolute()
    bridge = Path(args.bridge).absolute()
    output_build = Path(args.build).absolute()
    host = Path((Path(args.host_bin).absolute().parent / "host-bin").read_text().strip())
    output_build.parent.mkdir(parents=True, exist_ok=True)
    env = {
        "PATH": str(host),
        "HOME": str(output_build.parent / "home"),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PKG_CONFIG_PATH": "",
        "PKG_CONFIG_LIBDIR": "",
        "SISO_TELEMETRY": "0",
    }
    with tempfile.TemporaryDirectory(prefix="webrtc-gn-", dir=output_build.parent) as temporary:
        root = Path(temporary)
        env["HOME"] = str(root / ".home")
        build = root / "out" / "Default"
        for child in source.iterdir():
            if child.name != "out":
                (root / child.name).symlink_to(child, target_is_directory=child.is_dir())
        package = root / "pulsebeam"
        package.mkdir()
        for relative in ("BUILD.gn", "bridge.py", "rust", "toolchain", "runtime.cc"):
            (package / relative).symlink_to(overlay / relative, target_is_directory=relative in ("rust", "toolchain"))
        target = "//pulsebeam:native"
        if args.capture_source:
            capture = package / "capture"
            capture.mkdir()
            (capture / "BUILD.gn").symlink_to(overlay / "capture/BUILD.gn")
            (capture / "vp9_capture.cc").symlink_to(Path(args.capture_source).absolute())
            target = "//pulsebeam/capture:vp9_capture"
        owned = package / "pulsebeam-webrtc-sys"
        (owned / "src").mkdir(parents=True)
        (owned / "src/bridge.rs").symlink_to(bridge)
        (owned / "native").symlink_to(bridge.parent.parent / "native", target_is_directory=True)
        # rustfmt resolves ignore patterns relative to the real config path; a
        # symlink into the upstream checkout puts generated files outside it.
        stage_file(root, ".rustfmt.toml")
        stage_file(root, "build/config/siso/pulsebeam.star", (overlay / "siso.star").read_bytes().replace(b"@PULSEBEAM_HOST_TOOLS@", os.fsencode(host)))
        stage_file(root, "pulsebeam/remote.sh", (overlay / "remote.sh.in").read_bytes().replace(b"@PULSEBEAM_HOST_TOOLS@", os.fsencode(host)))
        (package / "remote.sh").chmod(0o755)
        native = args.flavor == "native"
        config = (overlay / "args.gn").read_text()
        if not args.capture_source:
            config += f"\npulsebeam_native = {str(native).lower()}\n"
        config += f"rtc_include_internal_audio_device = {str(native).lower()}\n"
        config += f"rtc_use_x11 = {str(native).lower()}\n"
        config += f"rtc_use_pipewire = {str(native).lower()}\n"
        if native:
            config += "rtc_link_pipewire = false\nrtc_include_pulse_audio = true\n"
        if args.asan:
            config += "is_asan = true\ndcheck_always_on = true\n"
        config += f'pkg_config = "{host / "pkg-config"}"\n'
        config += 'custom_toolchain = "//pulsebeam/toolchain:clang_x64"\n'
        config += f"use_remoteexec = {str(args.executor != 'local').lower()}\n"
        gn = root / "buildtools/linux64/gn"
        siso = root / "third_party/siso/cipd/siso"
        run([gn, "gen", build, f"--root={root}", f"--root-target={target}", "--ide=json", "--args=" + config], cwd=root, env=env, lock_fd=lock_fd)
        if args.capture_output:
            run(siso_command(siso, build, args.executor, args.reapi, "vp9_capture"), cwd=root, env=env, lock_fd=lock_fd)
            shutil.copyfile(build / "vp9_capture", args.capture_output)
            Path(args.capture_output).chmod(0o755)
            return
        graph = json.loads((build / "project.json").read_text())
        objects = runtime_objects(graph["targets"])
        write_notices(gn, root, build, source, env, args.notices, args.license_manifest)
        targets = ["obj/pulsebeam/libpulsebeam.a", "obj/libwebrtc.a", *objects]
        run(siso_command(siso, build, args.executor, args.reapi) + targets, cwd=root, env=env, lock_fd=lock_fd)
        runtime = Path(args.runtime).absolute()
        runtime.parent.mkdir(parents=True, exist_ok=True)
        runtime.unlink(missing_ok=True)
        response = build / "runtime.rsp"
        response.write_text("\n".join(json.dumps(value) for value in objects) + "\n")
        run([root / "third_party/llvm-build/Release+Asserts/bin/llvm-ar", "rcsD", runtime, "@" + str(response)], cwd=build, env=env, lock_fd=lock_fd)
        for name, relative in {
            "pulsebeam": "obj/pulsebeam/libpulsebeam.a",
            "webrtc": "obj/libwebrtc.a",
        }.items():
            output = Path(getattr(args, name))
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(build / relative, output)
        output_build.mkdir(exist_ok=True)
        shutil.copyfile(build / "project.json", output_build / "project.json")
        shutil.copyfile(build / "args.gn", output_build / "args.gn")


if __name__ == "__main__":
    main()
