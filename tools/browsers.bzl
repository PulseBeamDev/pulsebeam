"""Provision the consumer browser acceptance matrix with a pinned OS closure.

Nixpkgs' autoPatchelfHook supplies interpreter/RPATHs for all browser helpers,
without changing host-process libraries or entering an FHS namespace.
"""

load("@rules_nixpkgs_core//:nixpkgs.bzl", "nixpkgs_package")


def _browsers_impl(ctx):
    matrix = json.decode(ctx.read(Label("//tools:browser-matrix.json")))
    for artifact in matrix["platforms"]["linux-x86_64"]["artifacts"]:
        nixpkgs_package(
            name = "browser_" + artifact["name"],
            nix_file = Label("//tools/host:browser.nix"),
            repository = Label("@nixpkgs//:nixpkgs"),
            nixopts = [arg for key in ["name", "version", "url", "sha256", "archive"] for arg in ["--argstr", key, artifact[key]]],
            build_file_content = '''package(default_visibility = ["//visibility:public"])
filegroup(name = "files", srcs = glob(["**"]))
filegroup(name = "executable", srcs = [%s])
''' % repr(artifact["executable"]),
        )
    return ctx.extension_metadata(reproducible = True)


browsers = module_extension(implementation = _browsers_impl)
