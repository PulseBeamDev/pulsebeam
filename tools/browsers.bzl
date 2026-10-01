"""Provision the existing RTC acceptance matrix, without a mutable browser cache.

Upstream browser defaults do not implement this repository's ESR/version/hash
matrix. Bazel's http_archive owns downloads and extraction; this extension only
translates the existing matrix into repositories.
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")


def _browsers_impl(ctx):
    matrix = json.decode(ctx.read(Label("//crates/pulsebeam-rtc:browser/browser-matrix.json")))
    for artifact in matrix["platforms"]["linux-x86_64"]["artifacts"]:
        http_archive(
            name = "browser_" + artifact["name"],
            urls = [artifact["url"]],
            sha256 = artifact["sha256"],
            type = artifact["archive"],
            build_file_content = '''package(default_visibility = ["//visibility:public"])
filegroup(name = "files", srcs = glob(["**"]))
filegroup(name = "executable", srcs = [%s])
''' % repr(artifact["executable"]),
        )
    return ctx.extension_metadata(reproducible = True)


browsers = module_extension(implementation = _browsers_impl)
