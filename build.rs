fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_source_files = ["./pulsebeam-proto/v1/signaling.proto"];
    let msg_attr = "#[derive(Hash, Eq)]";
    tonic_build::configure()
        .build_server(true)
        // there's a lot of incorrect code generation with prost, so this is the best trade-off:
        // manual add vs manual delete. manual add is simpler.
        // https://github.com/tokio-rs/prost/issues/332
        .message_attribute(".", msg_attr)
        .type_attribute("MessagePayload.payload_type", msg_attr)
        .type_attribute("DataChannel.payload", msg_attr)
        .type_attribute("Signal.data", msg_attr)
        .type_attribute(
            ".",
            "#[derive(serde::Serialize, serde::Deserialize, valuable::Valuable)]",
        )
        .type_attribute(".", "#[serde(rename_all = \"camelCase\")]")
        .compile_protos(&proto_source_files, &["pulsebeam-proto"])
        .expect("error compiling protos");
    Ok(())
}
