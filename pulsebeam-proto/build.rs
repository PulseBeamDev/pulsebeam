// Emits into `OUT_DIR`, not `src/`.
//
// Generating into the source tree makes every build dirty the working copy:
// prost's layout is not rustfmt's, so `cargo build` and `cargo fmt` rewrite
// the same file back and forth and `cargo fmt --check` fails depending on
// which ran last. It also meant generated code was committed and could drift
// from the `.proto` it came from.
#[allow(
    clippy::expect_used,
    reason = "a build script that cannot generate code must fail"
)]
fn main() {
    prost_build::Config::new()
        .compile_protos(
            &["proto/signaling.proto", "proto/reliable.proto"],
            &["proto"],
        )
        .expect("Failed to compile .proto files");

    println!("cargo:rerun-if-changed=proto/");
}
