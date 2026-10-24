"""Whole-native-build action; ordinary Rust sources are deliberately not inputs."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")


def _resources(_os, _inputs):
    return {"cpu": 8, "memory": 8192}


def _native_build_impl(ctx):
    if not ctx.target_platform_has_constraint(ctx.attr._linux[platform_common.ConstraintValueInfo]) or not ctx.target_platform_has_constraint(ctx.attr._x86_64[platform_common.ConstraintValueInfo]):
        fail("pulsebeam-webrtc-sys source builds support Linux x86_64 only")
    flavor = ctx.attr._flavor[BuildSettingInfo].value
    asan = ctx.attr._asan[BuildSettingInfo].value
    identity = flavor + ("-asan" if asan else "")
    directory = ctx.actions.declare_directory(ctx.label.name + "/" + identity + "/build")
    libraries = {name: ctx.actions.declare_file(ctx.label.name + "/" + identity + "/lib" + name + ".a") for name in ["pulsebeam", "webrtc", "runtime"]}
    args = ctx.actions.args()
    args.add("--source-root", ctx.file._dotgn.dirname)
    args.add("--bridge", ctx.file.bridge)
    args.add("--overlay", ctx.file._overlay.dirname)
    args.add("--host-bin", ctx.file._python.dirname)
    args.add("--build", directory.path)
    args.add("--flavor", flavor)
    if asan:
        args.add("--asan")
    for name, library in libraries.items():
        args.add("--" + name, library)
    ctx.actions.run(
        executable = ctx.file._python,
        arguments = [ctx.file._runner.path, args],
        inputs = depset(
            direct = [ctx.file.bridge, ctx.file._runner, ctx.file._dotgn] + ctx.files.adapters + ctx.files._overlay_files,
            transitive = [ctx.attr._sources[DefaultInfo].files, ctx.attr._host_tools[DefaultInfo].files],
        ),
        outputs = [directory] + libraries.values(),
        env = {"PYTHONDONTWRITEBYTECODE": "1"},
        resource_set = _resources,
        mnemonic = "WebRtcGn",
        progress_message = "GN/Ninja WebRTC " + identity,
        execution_requirements = {"no-remote-exec": "1"},
    )
    return [
        DefaultInfo(files = depset([directory] + libraries.values())),
        OutputGroupInfo(**{name: depset([library]) for name, library in libraries.items()}),
    ]

native_build = rule(
    implementation = _native_build_impl,
    attrs = {
        "bridge": attr.label(allow_single_file = [".rs"], mandatory = True),
        "adapters": attr.label_list(allow_files = True),
        "_flavor": attr.label(default = ":flavor"),
        "_asan": attr.label(default = ":asan"),
        "_linux": attr.label(default = "@platforms//os:linux"),
        "_x86_64": attr.label(default = "@platforms//cpu:x86_64"),
        "_runner": attr.label(default = ":native_build.py", allow_single_file = True),
        "_overlay": attr.label(default = ":gn/BUILD.gn", allow_single_file = True),
        "_overlay_files": attr.label_list(default = [":gn/BUILD.gn", ":gn/args.gn", ":gn/integration.patch", ":gn/bridge.py", ":gn/strict-fp.py", ":gn/rust/cxx.h"], allow_files = True),
        "_sources": attr.label(default = "@webrtc_upstream//:sources"),
        "_dotgn": attr.label(default = "@webrtc_upstream//:checkout/src/.gn", allow_single_file = True),
        "_host_tools": attr.label(default = "@webrtc_host//:tools", cfg = "exec"),
        "_python": attr.label(default = "@webrtc_host//:bin/python3", allow_single_file = True, cfg = "exec"),
    },
)
