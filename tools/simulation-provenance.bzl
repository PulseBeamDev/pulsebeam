"""Declared candidate metadata for simulation launches and test actions."""

def _simulation_provenance_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [ctx.info_file],
        outputs = [output],
        arguments = [ctx.info_file.path, output.path],
        command = 'cp "$1" "$2"',
        mnemonic = "SimulationProvenance",
    )
    return [DefaultInfo(files = depset([output]))]

simulation_provenance = rule(implementation = _simulation_provenance_impl)
