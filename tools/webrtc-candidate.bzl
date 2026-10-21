"""Checksum-pinned, unpublished sys source and native artifact inputs."""

def _candidate_impl(ctx):
    lock = json.decode(ctx.read(ctx.attr.lock))
    item = lock[ctx.attr.kind]
    variable = "PULSEBEAM_WEBRTC_" + ctx.attr.kind.upper() + "_ARCHIVE"
    archive = ctx.os.environ.get(variable)
    if not archive:
        fail("Set --repo_env=%s=/absolute/path/to/%s for the local WebRTC candidate" % (variable, item["filename"]))
    path = ctx.path(archive)
    if not path.exists:
        fail("Missing candidate archive: %s" % path)
    ctx.download_and_extract(url = "file://" + str(path), sha256 = item["sha256"], type = "tar.gz")
    ctx.template("BUILD.bazel", ctx.attr.build_file, executable = False)

webrtc_candidate = repository_rule(
    implementation = _candidate_impl,
    attrs = {
        "kind": attr.string(mandatory = True, values = ["source", "native"]),
        "lock": attr.label(mandatory = True, allow_single_file = True),
        "build_file": attr.label(mandatory = True, allow_single_file = True),
    },
    environ = ["PULSEBEAM_WEBRTC_SOURCE_ARCHIVE", "PULSEBEAM_WEBRTC_NATIVE_ARCHIVE"],
)
