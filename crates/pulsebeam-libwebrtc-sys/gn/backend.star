# NativeLink runs on the same Linux host, not the Google Chromium worker image.
load("@builtin//struct.star", "module")

def _platforms(ctx):
    properties = {
        "OSFamily": "Linux",
        "pulsebeam-host-tools": "@PULSEBEAM_HOST_TOOLS@",
    }
    return {"default": properties, "large": properties}

def _configs(ctx):
    return []

backend = module("backend", platform_properties = _platforms, configs = _configs)
