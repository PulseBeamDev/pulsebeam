"""Declared candidate metadata for simulation launches and test actions."""

def _simulation_provenance_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(
        executable = ctx.file._python,
        inputs = [ctx.info_file],
        outputs = [output],
        arguments = ["-c", "import shutil, sys; shutil.copyfile(sys.argv[1], sys.argv[2])", ctx.info_file.path, output.path],
        mnemonic = "SimulationProvenance",
    )
    return [DefaultInfo(files = depset([output]))]

simulation_provenance = rule(
    implementation = _simulation_provenance_impl,
    attrs = {"_python": attr.label(default = "@webrtc_host//:bin/python3", allow_single_file = True, cfg = "exec")},
)
