"""Pinned executable inputs for repository tool contracts."""

load("@rules_nixpkgs_core//:nixpkgs.bzl", "nixpkgs_package")


def _test_tools_impl(ctx):
    nixpkgs_package(
        name = "contract_test_tools",
        nix_file = Label("//tools/host:test-tools.nix"),
        repository = "@nixpkgs",
        build_file_content = 'package(default_visibility = ["//visibility:public"])\nexports_files(["bin/git", "bin/bash", "bin/readlink", "bin/stat", "bin/sha256sum", "bin/uname", "bin/mkdir", "bin/dirname", "bin/flock"])\n',
    )
    return ctx.extension_metadata(reproducible = True)


test_tools = module_extension(implementation = _test_tools_impl, os_dependent = True, arch_dependent = True)
