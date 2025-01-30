fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let proto_source_files = ["./pulsebeam-proto/v1/tunnel.proto"];
    for entry in &proto_source_files {
        println!("cargo:rerun-if-changed={}", entry);
    }

    let msg_attr = "#[derive(Hash, Eq)]";
    prost_build::Config::new()
        // there's a lot of incorrect code generation with prost, so this is the best trade-off:
        // manual add vs manual delete. manual add is simpler.
        // https://github.com/tokio-rs/prost/issues/332
        .message_attribute(".", msg_attr)
        .type_attribute("MessagePayload.payload_type", msg_attr)
        .type_attribute("DataChannel.payload", msg_attr)
        .type_attribute("Signal.data", msg_attr)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .type_attribute(".", "#[serde(rename_all = \"camelCase\")]")
        .service_generator(twirp_build::service_generator())
        .compile_protos(&proto_source_files, &["./"])
        .expect("error compiling protos");
}
