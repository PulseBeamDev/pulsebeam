"""Import the locked OS-only SDK using maintained Nix repository rules."""

load("@rules_nixpkgs_core//:nixpkgs.bzl", "nixpkgs_local_repository", "nixpkgs_package")


def _os_impl(ctx):
    nixpkgs_local_repository(
        name = "nixpkgs",
        nix_flake_lock_file = Label("//tools/host:flake.lock"),
    )
    nixpkgs_package(
        name = "nix_sdk",
        nix_file = Label("//tools/host:sdk.nix"),
        repository = "@nixpkgs",
        build_file_content = 'package(default_visibility = ["//visibility:public"])\nfilegroup(name = "sysroot", srcs = ["."])\nfilegroup(name = "linker", srcs = ["bin/mold", "mold-path"])\nexports_files(["mold-path"])\n',
    )
    nixpkgs_package(
        name = "host_runtime",
        nix_file = Label("//tools/host:runtime.nix"),
        repository = "@nixpkgs",
        build_file_content = 'package(default_visibility = ["//visibility:public"])\nfilegroup(name = "tools", srcs = glob(["bin/**", "etc/**"]) + ["runtime-root"])\nexports_files(["runtime-root"])\n',
    )
    return ctx.extension_metadata(reproducible = True)


os_inputs = module_extension(implementation = _os_impl)
