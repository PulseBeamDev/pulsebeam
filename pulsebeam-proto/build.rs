fn main() {
    prost_build::Config::new()
        .out_dir("src/")
        .compile_protos(&["proto/signaling.proto"], &["proto"])
        .expect("Failed to compile .proto files");

    // Tell cargo to rerun if the proto files change
    println!("cargo:rerun-if-changed=proto/");
}
