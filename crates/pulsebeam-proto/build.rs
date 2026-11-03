use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};
use std::collections::{BTreeMap, BTreeSet};

fn message_types<'a>(
    prefix: &str,
    messages: &'a [DescriptorProto],
    index: &mut BTreeMap<String, &'a DescriptorProto>,
) {
    for message in messages {
        let name = format!("{prefix}.{}", message.name());
        index.insert(name.clone(), message);
        message_types(&name, &message.nested_type, index);
    }
}

fn capability_types(descriptors: &FileDescriptorSet) -> BTreeSet<String> {
    let mut messages = BTreeMap::new();
    for file in &descriptors.file {
        message_types(
            &format!(".{}", file.package()),
            &file.message_type,
            &mut messages,
        );
    }
    let mut pending = vec![
        String::from(".pulsebeam.media.v1.RtpCapabilities"),
        String::from(".pulsebeam.media.v1.DataDelivery"),
    ];
    let mut selected = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if selected.insert(name.clone())
            && let Some(message) = messages.get(&name)
        {
            pending.extend(
                message
                    .field
                    .iter()
                    .filter_map(|field| field.type_name.clone()),
            );
        }
    }
    selected
}

// Generate only into OUT_DIR. Source-tree output can drift from the schema
// and makes builds fight rustfmt over tracked files.
#[allow(
    clippy::expect_used,
    reason = "a build script that cannot generate code must fail"
)]
fn main() {
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    prost_build::Config::new()
        .file_descriptor_set_path(out_dir.join("state-descriptors.bin"))
        .boxed(".pulsebeam.media.v1.DesiredState.value.present")
        .boxed(".pulsebeam.media.v1.ConfirmedState.value.owned")
        .compile_protos(&["proto/state.proto", "proto/topic.proto"], &["proto"])
        .expect("Failed to compile .proto files");

    let descriptor_bytes = std::fs::read(out_dir.join("state-descriptors.bin"))
        .expect("prost emitted the descriptor set");
    let descriptors = FileDescriptorSet::decode(descriptor_bytes.as_slice())
        .expect("prost emitted a valid descriptor set");
    let selected = capability_types(&descriptors);
    let prefixes = selected.iter().map(String::as_str).collect::<Vec<_>>();
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)
        .expect("capability descriptors are valid")
        .preserve_proto_field_names()
        .ignore_unknown_fields()
        .build(&prefixes)
        .expect("Failed to generate capability ProtoJSON");

    println!("cargo:rerun-if-changed=proto/");
}
