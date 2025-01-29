fn main() {
    let proto_source_files = ["./pulsebeam-proto/v1/tunnel.proto"];
    for entry in &proto_source_files {
        println!("cargo:rerun-if-changed={}", entry);
    }

    prost_build::Config::new()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]") // enable support for JSON encoding
        .type_attribute(".", "#[serde(rename_all = \"camelCase\")]")
        .service_generator(twirp_build::service_generator())
        .compile_protos(&proto_source_files, &["./"])
        .expect("error compiling protos");
}
