"""Declared finite source variants. Only calibration targets consume these outputs."""

VARIANTS = ["local-fanout", "remote-fanout", "materialization"]

def _sources_impl(ctx):
    sources = []
    data = []
    entries = []
    root = None
    for source in ctx.files.srcs:
        relative = source.short_path.removeprefix(ctx.label.package + "/")
        output = ctx.actions.declare_file(ctx.label.name + "/" + relative)
        entries.append({"input": source.path, "output": output.path, "relative": relative})
        if relative.endswith(".rs"):
            sources.append(output)
        else:
            data.append(output)
        if relative == "src/lib.rs":
            root = output
    if root == None:
        fail("SFU crate root missing")
    manifest = ctx.actions.declare_file(ctx.label.name + ".inputs.json")
    provenance = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(manifest, json.encode(entries))
    ctx.actions.run(
        executable = ctx.executable._transform,
        arguments = ["--variant", ctx.attr.variant, "--manifest", manifest.path, "--provenance", provenance.path],
        inputs = ctx.files.srcs + [manifest],
        outputs = sources + data + [provenance],
        mnemonic = "CalibrationVariant",
    )
    return [
        DefaultInfo(files = depset(sources + data)),
        OutputGroupInfo(sources = depset(sources), compile_data = depset(data), root = depset([root]), provenance = depset([provenance])),
    ]

calibration_sources = rule(
    implementation = _sources_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "variant": attr.string(values = VARIANTS, mandatory = True),
        "_transform": attr.label(default = "//tools:calibration_transform", executable = True, cfg = "exec"),
    },
)

def sfu_variant_sources(srcs):
    for variant in VARIANTS:
        name = "calibration-" + variant
        calibration_sources(name = name, variant = variant, srcs = srcs, testonly = True, tags = ["manual"])
        for group in ["sources", "compile_data", "root", "provenance"]:
            native.filegroup(name = name + "-" + group, srcs = [":" + name], output_group = group, testonly = True, tags = ["manual"])
