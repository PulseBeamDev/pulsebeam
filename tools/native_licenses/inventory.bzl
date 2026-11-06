"""Configured crate-universe Rust license inventory.

Load native_license_inventory and pass roots = [":configured_rust_target"].
The rule produces <name>.manifest.json and <name>.notices.txt. Supplemental
files use {"//path:license": "canonical-repository|crate-name|version"};
all three fields must match exactly. Supply pinned, reviewed source files.
Only cargo-bazel tagged rust_* targets in rules_rust crate extension repositories
are inventoried. Other repositories, WebRTC, and system libraries are excluded.
Host proc macros are deliberately retained even if not shipped in the binary.
"""

InventoryInfo = provider(fields = ["records", "files"])

def _aspect_impl(target, ctx):
    transitive_records = []
    transitive_files = []
    for attr in ["deps", "proc_macro_deps", "actual"]:
        if hasattr(ctx.rule.attr, attr):
            value = getattr(ctx.rule.attr, attr)
            for dep in value if type(value) == "list" else [value]:
                if dep and InventoryInfo in dep:
                    transitive_records.append(dep[InventoryInfo].records)
                    transitive_files.append(dep[InventoryInfo].files)

    records = []
    files = []
    repo = target.label.workspace_name
    if (repo.startswith("rules_rust+") and "+crate+" in repo and "__" in repo.split("+crate+")[-1] and
        ctx.rule.kind.startswith("rust_") and hasattr(ctx.rule.attr, "tags") and "cargo-bazel" in ctx.rule.attr.tags):
        if not hasattr(ctx.rule.files, "compile_data"):
            fail("crate-universe Rust target has no compile_data: %s" % target.label)
        files = ctx.rule.files.compile_data
        paths = []
        for f in files:
            prefix = "../" + repo + "/"
            if not f.short_path.startswith(prefix):
                fail("unexpected compile_data outside crate repository: %s in %s" % (f.short_path, target.label))
            paths.append((f.short_path[len(prefix):], f.path))
        records.append(json.encode({"repository": repo, "target": str(target.label), "files": sorted(paths)}))
    return [InventoryInfo(
        records = depset(records, transitive = transitive_records),
        files = depset(files, transitive = transitive_files),
    )]

_inventory_aspect = aspect(
    implementation = _aspect_impl,
    attr_aspects = ["deps", "proc_macro_deps", "actual"],
)

def _inventory_impl(ctx):
    records = depset(transitive = [root[InventoryInfo].records for root in ctx.attr.roots]).to_list()
    files = depset(transitive = [root[InventoryInfo].files for root in ctx.attr.roots])
    supplemental = []
    for target, specs in ctx.attr.supplemental.items():
        target_files = target[DefaultInfo].files.to_list()
        if len(target_files) != 1:
            fail("each supplemental label must provide exactly one file: %s" % target.label)
        for spec in specs.split("\n"):
            supplemental.append({"spec": spec, "path": target_files[0].path, "label": str(target.label)})
    index = ctx.actions.declare_file(ctx.label.name + ".inputs.json")
    manifest = ctx.actions.declare_file(ctx.label.name + ".manifest.json")
    notices = ctx.actions.declare_file(ctx.label.name + ".notices.txt")
    ctx.actions.write(index, json.encode({"records": sorted(records), "supplemental": supplemental}))
    ctx.actions.run(
        executable = ctx.executable._generator,
        arguments = [index.path, manifest.path, notices.path],
        inputs = depset([index] + [f for t in ctx.attr.supplemental for f in t[DefaultInfo].files.to_list()], transitive = [files]),
        outputs = [manifest, notices],
        mnemonic = "RustLicenseInventory",
        progress_message = "Collecting configured Rust licenses for %{label}",
    )
    return [
        DefaultInfo(files = depset([manifest, notices])),
        OutputGroupInfo(manifest = depset([manifest]), notices = depset([notices])),
    ]

native_license_inventory = rule(
    implementation = _inventory_impl,
    attrs = {
        "roots": attr.label_list(mandatory = True, aspects = [_inventory_aspect]),
        "supplemental": attr.label_keyed_string_dict(allow_files = True),
        "_generator": attr.label(default = Label("//tools/native_licenses:generator"), executable = True, cfg = "exec"),
    },
)
