"""Bridge toolchains_llvm's path-only linker API to a declared Nix mold input."""

load("@toolchains_llvm//toolchain:rules.bzl", "llvm_toolchain")


def _native_impl(ctx):
    arch = {"amd64": "x86_64", "x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(ctx.os.arch)
    if ctx.os.name != "linux" or not arch:
        fail("Native builds require Linux x86_64, or Linux aarch64 for release assembly.")
    for mod in ctx.modules:
        for tag in mod.tags.toolchain:
            mold = ctx.read(Label("@nix_sdk//:mold-path")).strip()
            llvm_toolchain(
                name = "llvm_toolchain",
                llvm_version = tag.llvm_version,
                exec_os = "linux",
                sysroot = {"linux-" + arch: str(Label("@nix_sdk//:sysroot"))},
                linker = {"": mold},
                extra_linker_files = Label("@nix_sdk//:linker"),
            )
    return ctx.extension_metadata(reproducible = True)


native = module_extension(
    implementation = _native_impl,
    os_dependent = True,
    arch_dependent = True,
    tag_classes = {"toolchain": tag_class(attrs = {"llvm_version": attr.string(mandatory = True)})},
)
