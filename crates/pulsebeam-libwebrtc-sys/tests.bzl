"""Existing non-device runtime proofs, compiled and linked through local Bazel owners."""

load("@rules_rust//rust:defs.bzl", "rust_test")

LIFETIME_TESTS = [
    "event_log_lifecycle",
    "managed_pcm_teardown",
    "packet_socket_lifecycle",
    "production_sdk_wake_drop",
    "provider_environment_retention",
    "provider_panic_boundaries",
    "runtime_alias_retirement",
    "session_runtime",
]

_CLANG_ROOT = Label("@webrtc_upstream//:clang++").workspace_root + "/checkout/src/third_party/llvm-build/Release+Asserts"

ASAN_FLAGS = [
    "-Clinker=$(location @webrtc_upstream//:clang++)",
    "-Clink-arg=-resource-dir=" + _CLANG_ROOT + "/lib/clang/23",
    "-Clink-arg=--ld-path=" + _CLANG_ROOT + "/bin/ld.lld",
    # Clang still discovers CRTs in the declared SDK; Nix linker scripts use absolute store paths.
    "-Clink-arg=-Wl,--sysroot=",
    "-Clink-arg=-fsanitize=address",
    "-Clink-arg=-fuse-ld=lld",
    "-Clink-arg=-nostdlib++",
]


def runtime_tests(sources, support, fixtures):
    names = []
    for source in sources:
        name = source[len("tests/"):-len(".rs")]
        is_fixture = name == "turn"
        if not is_fixture:
            names.append(":" + name)
        tools = name in ["turn", "controlled_turn_tls", "installed_packet_socket"]
        rust_test(
            name = name,
            crate_root = source,
            srcs = [source] + support,
            compile_data = fixtures + (["//crates/pulsebeam-testdata:quality_zero_video_inputs"] if name == "production_session" else []) + select({":asan_selected": ["@webrtc_upstream//:clang_tools", "@webrtc_upstream//:clang++"], "//conditions:default": []}),
            edition = "2024",
            args = ["--test-threads=1"],
            tags = ["manual", "external-network"] if is_fixture else [],
            data = ["@webrtc_test_tools//:tools"] if tools else [],
            env = {"PULSEBEAM_TEST_TOOLS": "../" + Label("@webrtc_test_tools//:bin/openssl").workspace_name + "/bin"} if tools else {},
            rustc_flags = select({":asan_selected": ASAN_FLAGS, "//conditions:default": []}),
            crate_features = select({":native_selected": ["native"], "//conditions:default": []}),
            deps = [":pulsebeam-webrtc-sys", "@crates//:serde", "@crates//:serde_json", "@crates//:rustls", "@crates//:oxitls-rustcrypto-provider"],
        )
    native.test_suite(name = "runtime_tests", tests = names)
    native.test_suite(name = "lifetime", tests = [":" + name for name in LIFETIME_TESTS])
    native.test_suite(name = "network_fixtures", tests = [":turn", ":controlled_turn_tls"], tags = ["manual"])
