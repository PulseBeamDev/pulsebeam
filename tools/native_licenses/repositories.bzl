"""Expose the pinned Rust toolchain's standard-library copyright inventory."""

def _rust_std_notices_impl(ctx):
    toolchain = ctx.path(ctx.attr.host_build).dirname
    config = ctx.read(ctx.attr.version_file)
    channels = [line.split("=", 1)[1].strip().strip('"') for line in config.splitlines() if line.strip().startswith("channel =")]
    if len(channels) != 1:
        fail("expected one pinned Rust channel in rust-toolchain.toml")
    result = ctx.execute([str(toolchain.get_child("bin/rustc")), "--version"])
    if result.return_code or not result.stdout.startswith("rustc " + channels[0] + " "):
        fail("Rust host toolchain version differs from rust-toolchain.toml: " + result.stdout + result.stderr)
    notice = ctx.read(toolchain.get_child("share/doc/rust/COPYRIGHT-library.html"))
    if "Copyright notices for The Rust Standard Library" not in notice:
        fail("pinned Rust toolchain is missing its standard-library notice inventory")
    ctx.file("COPYRIGHT-library.html", notice)
    ctx.file("BUILD.bazel", 'exports_files(["COPYRIGHT-library.html"], visibility = ["//visibility:public"])\n')
    return ctx.repo_metadata(reproducible = True)

rust_std_notices = repository_rule(
    implementation = _rust_std_notices_impl,
    attrs = {
        "host_build": attr.label(mandatory = True, allow_single_file = True),
        "version_file": attr.label(mandatory = True, allow_single_file = True),
    },
)
