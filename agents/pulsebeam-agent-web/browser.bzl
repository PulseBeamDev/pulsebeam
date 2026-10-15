"""Shared pinned browser runner configuration, with owner-specific fixture inputs."""

load("@crates//:defs.bzl", "aliases", "crate_deps", "crate_edition")
load("@rules_rust//rust:defs.bzl", "rust_test")

def browser_test(name, source, assets, env, contracts = [], live = False, features = []):
    rust_test(
        name = name,
        crate_root = source,
        srcs = [source] + native.glob(["tests/support/**/*.rs"], exclude = [] if live else ["tests/support/destination.rs"]),
        compile_data = contracts,
        crate_features = features,
        edition = crate_edition(),
        rustc_env_files = [":cargo_env"],
        lint_config = ":lints",
        aliases = aliases(normal = True, normal_dev = True, proc_macro = True, proc_macro_dev = True),
        deps = crate_deps(["serde", "serde_json", "thirtyfour", "tokio"] + (["tokio-util"] if live else [])) + [
            "//crates/pulsebeam-core:pulsebeam-core",
            "//crates/pulsebeam-proto:pulsebeam-proto",
            "//server/rust:pulsebeam-server",
        ] + (["//crates/pulsebeam:pulsebeam", "//crates/pulsebeam-runtime:pulsebeam-runtime"] if live else []),
        data = assets + [
            "@browser_chrome//:files",
            "@browser_chrome//:executable",
            "@browser_chromedriver//:files",
            "@browser_chromedriver//:executable",
        ],
        env = dict(env, **{
            "PULSEBEAM_BROWSER_BINARY": "$(rootpath @browser_chrome//:executable)",
            "PULSEBEAM_DRIVER_BINARY": "$(rootpath @browser_chromedriver//:executable)",
        }),
        tags = ["slow", "cpu:4"],
        size = "large",
    )
