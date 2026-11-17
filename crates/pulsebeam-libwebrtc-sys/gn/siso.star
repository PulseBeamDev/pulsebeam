load("@builtin//encoding.star", "json")
load("@builtin//struct.star", "module")
load("./main.star", upstream_init = "init")

def init(ctx):
    upstream = upstream_init(ctx)
    steps = json.decode(upstream.step_config)
    properties = {
        "OSFamily": "Linux",
        "pulsebeam-host-tools": "@PULSEBEAM_HOST_TOOLS@",
    }
    steps["platforms"] = {"default": properties, "large": properties}
    for rule in steps["rules"]:
        # NativeLink uses the declared Nix tools, not Chromium's worker image.
        if rule.get("remote_wrapper"):
            fail("upstream Siso remote wrapper needs explicit composition: " + rule["name"])
        rule["remote_wrapper"] = "../../pulsebeam/remote.sh"
        rule.setdefault("inputs", []).append("pulsebeam/remote.sh")
        # Preserve upstream's explicitly local generators, including bindgen.
        if rule["name"].startswith("nasm/") or rule["name"].startswith("rust_"):
            rule["remote"] = True
    return module(
        "config",
        step_config = json.encode(steps),
        filegroups = upstream.filegroups,
        handlers = upstream.handlers,
    )
