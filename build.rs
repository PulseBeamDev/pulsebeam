fn main() {
    prost_build::Config::new()
        .out_dir("src/proto")
        .compile_protos(&["proto/sfu.proto"], &["proto"])
        .expect("Failed to compile .proto files");

    // Tell cargo to rerun if the proto files change
    println!("cargo:rerun-if-changed=proto/");
}
