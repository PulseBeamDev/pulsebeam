"""Expose the locked OS runtime identity as a declared native toolchain input."""


def _environment_impl(ctx):
    identity = ctx.getenv("PULSEBEAM_HOST_ENV")
    if not identity:
        fail("Use ./bazel to enter the locked PulseBeam OS environment.")
    marker = ctx.path("/etc/pulsebeam-host")
    if not marker.exists or ctx.read(marker, watch = "yes").strip() != identity:
        fail("PULSEBEAM_HOST_ENV does not match the locked OS marker; use ./bazel without overriding the reserved identity.")
    ctx.file("identity", identity + "\n")
    ctx.file("BUILD.bazel", 'exports_files(["identity"], visibility = ["//visibility:public"])\n')

host_environment = repository_rule(
    implementation = _environment_impl,
    environ = ["PULSEBEAM_HOST_ENV"],
)
