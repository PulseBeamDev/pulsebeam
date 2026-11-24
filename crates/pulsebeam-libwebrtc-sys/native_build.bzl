"""Bazel owns the native result; Siso owns fine-grained GN execution."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")


def _resources(_os, _inputs):
    return {"cpu": 8, "memory": 8192}


def _client_resources(_os, _inputs):
    return {"cpu": 2, "memory": 1024}


def _native_build_impl(ctx):
    if not ctx.target_platform_has_constraint(ctx.attr._linux[platform_common.ConstraintValueInfo]) or not ctx.target_platform_has_constraint(ctx.attr._x86_64[platform_common.ConstraintValueInfo]):
        fail("pulsebeam-webrtc-sys source builds support Linux x86_64 only")
    flavor = ctx.attr._flavor[BuildSettingInfo].value
    asan = ctx.attr._asan[BuildSettingInfo].value
    executor = ctx.attr._executor[BuildSettingInfo].value
    endpoint = ctx.attr._reapi[BuildSettingInfo].value
    identity = flavor + ("-asan" if asan else "")
    capture = ctx.file.capture
    prefix = ctx.label.name + "/" + identity + "/"
    args = ctx.actions.args()
    args.add("--source-manifest", ctx.file._source_manifest)
    args.add("--bridge", ctx.file.bridge)
    args.add("--owned-cargo-manifest", ctx.file._owned_cargo_manifest)
    args.add("--overlay", ctx.file._overlay.dirname)
    args.add("--host-bin", ctx.file._python.dirname)
    args.add("--flavor", flavor)
    args.add("--executor", executor)
    args.add("--reapi", endpoint)
    if asan:
        args.add("--asan")
    if capture:
        executable = ctx.actions.declare_file(prefix + "vp9_capture")
        args.add("--build", executable.dirname + "/build")
        args.add("--capture-source", capture)
        args.add("--capture-output", executable)
        inputs = [capture, ctx.file._capture_gn]
        outputs = [executable]
    else:
        directory = ctx.actions.declare_directory(prefix + "build")
        libraries = {name: ctx.actions.declare_file(prefix + "lib" + name + ".a") for name in ["pulsebeam", "webrtc", "runtime"]}
        notices = ctx.actions.declare_file(prefix + "THIRD_PARTY_WEBRTC.md")
        manifest = ctx.actions.declare_file(prefix + "webrtc_license_manifest.json")
        args.add("--build", directory.path)
        args.add("--notices", notices)
        args.add("--license-manifest", manifest)
        for name, library in libraries.items():
            args.add("--" + name, library)
        inputs = []
        outputs = [directory, notices, manifest] + libraries.values()
    requirements = {"no-remote-exec": "1"}
    if executor != "local":
        # Only the inner Siso client uses REAPI. Keep the outer action local.
        requirements["requires-network"] = "1"
    ctx.actions.run(
        executable = ctx.file._python,
        arguments = [ctx.file._runner.path, args],
        inputs = depset(
            direct = [ctx.file.bridge, ctx.file._runner, ctx.file._source_manifest, ctx.file._owned_cargo_manifest] + ctx.files.adapters + ctx.files._overlay_files + inputs,
            transitive = [ctx.attr._host_tools[DefaultInfo].files],
        ),
        outputs = outputs,
        env = {"PYTHONDONTWRITEBYTECODE": "1"},
        resource_set = _client_resources if executor == "reapi" else _resources,
        mnemonic = "WebRtcGn",
        progress_message = "GN/Siso WebRTC " + identity + " (" + executor + ")",
        execution_requirements = requirements,
    )
    if capture:
        return [DefaultInfo(files = depset(outputs))]
    groups = {name: depset([library]) for name, library in libraries.items()}
    groups["notices"] = depset([notices])
    groups["license_manifest"] = depset([manifest])
    return [
        DefaultInfo(files = depset([directory] + libraries.values())),
        OutputGroupInfo(**groups),
    ]

native_build = rule(
    implementation = _native_build_impl,
    attrs = {
        "bridge": attr.label(allow_single_file = [".rs"], mandatory = True),
        "adapters": attr.label_list(allow_files = True),
        "capture": attr.label(allow_single_file = [".cc"]),
        "_capture_gn": attr.label(default = ":gn/capture/BUILD.gn", allow_single_file = True),
        "_flavor": attr.label(default = ":flavor"),
        "_asan": attr.label(default = ":asan"),
        "_executor": attr.label(default = ":executor"),
        "_reapi": attr.label(default = ":reapi"),
        "_linux": attr.label(default = "@platforms//os:linux"),
        "_x86_64": attr.label(default = "@platforms//cpu:x86_64"),
        "_runner": attr.label(default = ":native_build.py", allow_single_file = True),
        "_owned_cargo_manifest": attr.label(default = ":Cargo.toml", allow_single_file = True),
        "_overlay": attr.label(default = ":gn/BUILD.gn", allow_single_file = True),
        "_overlay_files": attr.label_list(default = [":gn/BUILD.gn", ":gn/args.gn", ":gn/bridge.py", ":gn/runtime.cc", ":gn/remote.sh.in", ":gn/siso.star", ":gn/toolchain/BUILD.gn", ":gn/rust/cxx.h"], allow_files = True),
        "_source_manifest": attr.label(default = "@webrtc_upstream//:sources.sha256", allow_single_file = True),
        "_host_tools": attr.label(default = "@webrtc_host//:tools", cfg = "exec"),
        "_python": attr.label(default = "@webrtc_host//:bin/python3", allow_single_file = True, cfg = "exec"),
    },
)
