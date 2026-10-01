# Generated from normal owner dependencies by //tools:update-rust-pins. Do not edit.
load("@crates//:defs.bzl", native_deps = "all_crate_deps")

WASM_DEPS = {
    "agents/pulsebeam-agent-web": [
        "@wasm_crates//:futures-channel",
        "@wasm_crates//:js-sys",
        "@wasm_crates//:log",
        "@wasm_crates//:spin",
        "@wasm_crates//:serde",
        "@wasm_crates//:serde-wasm-bindgen",
        "@wasm_crates//:thiserror",
        "@wasm_crates//:uniffi",
        "@wasm_crates//:wasm-bindgen",
        "@wasm_crates//:wasm-bindgen-futures",
        "@wasm_crates//:web-sys"
    ],
    "agents/pulsebeam-agent-core": [
        "@wasm_crates//:log",
        "@wasm_crates//:serde",
        "@wasm_crates//:serde_json",
        "@wasm_crates//:thiserror",
        "@wasm_crates//:uniffi"
    ],
    "crates/pulsebeam-proto": [
        "@wasm_crates//:lz4_flex",
        "@wasm_crates//:prost",
        "@wasm_crates//:prost-types"
    ]
}

def all_crate_deps(**kwargs):
    if kwargs == {"normal": True}:
        return select({
            "@platforms//cpu:wasm32": WASM_DEPS[native.package_name()],
            "//conditions:default": native_deps(**kwargs),
        })
    return native_deps(**kwargs)
