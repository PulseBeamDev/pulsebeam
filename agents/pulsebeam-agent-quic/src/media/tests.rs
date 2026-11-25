use super::{
    H264, Opus, PacketBudget, PacketizeError,
    descriptor::{
        DdReadError, DdWriteError, DecodeTargetIndication, DependencyDescriptor,
        FrameDependencyTemplate, Reader, TemplateDependencyStructure, WriteError, Writer,
    },
};
use pulsebeam_core::fabric::{Extension, ExtensionKind};

fn keyframe() -> DependencyDescriptor {
    let template = FrameDependencyTemplate {
        dtis: [DecodeTargetIndication::Switch].into_iter().collect(),
        ..Default::default()
    };
    DependencyDescriptor {
        frame_dependencies: template.clone(),
        attached_structure: Some(Box::new(TemplateDependencyStructure {
            decode_target_count: 1,
            templates: [template].into_iter().collect(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[test]
fn h264_and_descriptor_stages_use_logical_boundaries_across_frames() {
    let h264 = H264::new(PacketBudget::new(29, 12).unwrap()).unwrap();
    let mut writer = Writer::default();
    let mut reader = Reader::new();
    let first = keyframe();
    let annex_b = vec![0, 0, 1, 0x65, 1, 2, 3, 4, 5, 6, 7, 8];
    let packets: Vec<_> = h264.packetize(u32::MAX, 9000, annex_b).unwrap().collect();
    // FU-A bytes and DD mandatory fields are independently specified.
    let expected = [
        (
            u32::MAX,
            &[0x7c, 0x85, 1, 2, 3][..],
            &[0x80, 0, 0, 0x80, 0, 0xe0][..],
        ),
        (0, &[0x7c, 0x05, 4, 5, 6][..], &[0x00, 0, 0][..]),
        (1, &[0x7c, 0x45, 7, 8][..], &[0x40, 0, 0][..]),
    ];
    assert_eq!(packets.len(), expected.len());
    for (packet, (sequence, payload, descriptor)) in packets.iter().zip(expected) {
        let encoded = writer
            .write(packet, packet.first.then_some(&first))
            .unwrap();
        let extensions = [encoded.extension()];
        let send = packet.as_send_packet(&extensions);
        assert_eq!(send.sequence, sequence);
        assert_eq!(send.timestamp, 9000);
        assert_eq!(send.payload, payload);
        assert_eq!(send.extensions[0].value, descriptor);
        let parsed = reader.read(send.extensions[0].value).unwrap();
        assert_eq!(parsed.start_of_frame, packet.first);
        assert_eq!(parsed.end_of_frame, send.marker);
        assert_eq!(parsed.frame_number, 0);
        assert_eq!(parsed.attached_structure.is_some(), packet.first);
    }
    let packet = h264
        .packetize(2, 12000, vec![0, 0, 1, 0x41, 0x55])
        .unwrap()
        .next()
        .unwrap();
    let mut dependent = DependencyDescriptor {
        frame_number: 1,
        attached_structure: None,
        ..first
    };
    dependent.frame_dependencies.frame_diffs.push(1);
    assert!(matches!(
        writer.write(&packet, Some(&dependent)),
        Err(WriteError::Encode(DdWriteError::Inconsistent))
    ));
    dependent.fields.set_custom_fdiffs(true);
    let encoded = writer.write(&packet, Some(&dependent)).unwrap();
    assert_eq!(encoded.extension().value, &[0xc0, 0, 1, 0x12, 0]);
    assert_eq!(
        reader
            .read(encoded.extension().value)
            .unwrap()
            .frame_dependencies
            .frame_diffs
            .as_slice(),
        &[1]
    );

    let mut updated = keyframe();
    updated.frame_number = 2;
    updated.template_id = 3;
    updated
        .attached_structure
        .as_mut()
        .unwrap()
        .template_id_offset = 3;
    let packet = h264
        .packetize(3, 15000, vec![0, 0, 1, 0x65, 0x56])
        .unwrap()
        .next()
        .unwrap();
    let encoded = writer.write(&packet, Some(&updated)).unwrap();
    assert_eq!(
        reader.read(encoded.extension().value).unwrap(),
        DependencyDescriptor {
            start_of_frame: true,
            end_of_frame: true,
            ..updated
        }
    );
}

#[test]
fn publisher_retries_same_time_layer_updates_without_advancing_state() {
    use super::{Packet, descriptor::Publisher};
    let mut base = keyframe();
    base.frame_number = u16::MAX;
    let upper_dependencies = FrameDependencyTemplate {
        spatial_id: 1,
        frame_diffs: [1].into_iter().collect(),
        ..base.frame_dependencies.clone()
    };
    base.attached_structure
        .as_mut()
        .unwrap()
        .templates
        .push(upper_dependencies.clone());
    let upper = DependencyDescriptor {
        frame_number: 0,
        template_id: 1,
        frame_dependencies: upper_dependencies,
        ..Default::default()
    };
    let mut publisher = Publisher::new(u16::MAX);
    let packet = |sequence, first, last, marker| Packet {
        sequence,
        timestamp: 41,
        first,
        last,
        marker,
        payload: [0x82],
    };
    let start = packet(u32::MAX, true, false, false);
    let mut retained = Vec::new();
    assert!(
        !publisher
            .write_if(&start, Some(&base), |ext| {
                retained.extend_from_slice(ext.value);
                false
            })
            .unwrap()
    );
    assert_eq!(publisher.next_frame(), u16::MAX);
    let mut no_structure = base.clone();
    no_structure.attached_structure = None;
    assert_eq!(
        publisher.write_if(&start, Some(&no_structure), |_| panic!("missing structure")),
        Err(WriteError::Encode(DdWriteError::NoStructure))
    );
    let mut reader = Reader::new();
    assert!(
        publisher
            .write_if(&start, Some(&base), |ext| {
                assert_eq!(ext.value, retained);
                assert_eq!(reader.read(ext.value).unwrap().spatial_id(), 0);
                true
            })
            .unwrap()
    );
    let end = packet(0, false, true, false);
    assert!(
        !publisher
            .write_if(&end, None, |ext| {
                assert_eq!(ext.value, &[0x40, 0xff, 0xff]);
                false
            })
            .unwrap()
    );
    assert_eq!(publisher.next_frame(), u16::MAX);
    assert!(
        publisher
            .write_if(&end, None, |ext| {
                assert_eq!(ext.value, &[0x40, 0xff, 0xff]);
                assert!(reader.read(ext.value).unwrap().end_of_frame);
                true
            })
            .unwrap()
    );
    assert_eq!(publisher.next_frame(), 0);
    let upper_packet = packet(1, true, true, true);
    let mut missing = upper.clone();
    missing.frame_dependencies.frame_diffs[0] = 2;
    missing.fields.set_custom_fdiffs(true);
    assert_eq!(
        publisher.write_if(&upper_packet, Some(&missing), |_| panic!(
            "unseen reference"
        )),
        Err(WriteError::MissingReference)
    );
    assert!(
        publisher
            .write_if(&upper_packet, Some(&upper), |ext| {
                assert_eq!(ext.value, &[0xc1, 0, 0]);
                let parsed = reader.read(ext.value).unwrap();
                assert_eq!(parsed.spatial_id(), 1);
                assert_eq!(parsed.frame_dependencies.frame_diffs.as_slice(), &[1]);
                true
            })
            .unwrap()
    );
    assert_eq!(publisher.next_frame(), 1);

    // The next phase uses the same template but references the frame before
    // the previous one. The custom fdiff follows the five presence bits.
    let mut shifted = upper;
    shifted.frame_number = 1;
    shifted.frame_dependencies.frame_diffs[0] = 2;
    assert_eq!(
        publisher.write_if(&packet(2, true, true, true), Some(&shifted), |_| {
            panic!("a new reference distance needs an override")
        }),
        Err(WriteError::Encode(DdWriteError::Inconsistent))
    );
    shifted.fields.set_custom_fdiffs(true);
    assert!(
        publisher
            .write_if(&packet(2, true, true, true), Some(&shifted), |ext| {
                assert_eq!(ext.value, &[0xc1, 0, 1, 0x12, 0x20]);
                let parsed = reader.read(ext.value).unwrap();
                assert_eq!(parsed.frame_dependencies.frame_diffs.as_slice(), &[2]);
                true
            })
            .unwrap()
    );
}

#[test]
fn source_reset_and_malformed_encoder_facts_do_not_guess_a_template() {
    let opus = Opus::new(PacketBudget::new(16, 0).unwrap());
    let packet = opus.packetize(0, 960, false, &[0xf8, 1]).unwrap();
    let mut writer = Writer::default();
    let mut reader = Reader::new();
    let mut facts = keyframe();
    let encoded = writer.write(&packet, Some(&facts)).unwrap();
    reader.read(encoded.extension().value).unwrap();
    writer.reset();
    reader.reset();
    facts.attached_structure = None;
    assert!(matches!(
        writer.write(&packet, Some(&facts)),
        Err(WriteError::Encode(DdWriteError::NoStructure))
    ));
    assert!(matches!(
        reader.read(&[0xc0, 0, 0]),
        Err(DdReadError::NoStructure)
    ));
    let mut invalid = keyframe();
    let structure = invalid.attached_structure.as_mut().unwrap();
    structure.templates.push(FrameDependencyTemplate {
        temporal_id: 2,
        ..invalid.frame_dependencies.clone()
    });
    assert!(matches!(
        writer.write(&packet, Some(&invalid)),
        Err(WriteError::Encode(DdWriteError::Inconsistent))
    ));
    assert!(writer.write(&packet, Some(&keyframe())).is_ok());
}

#[test]
fn opus_is_one_borrowed_packet_and_passes_supplied_descriptor_through() {
    assert!(PacketBudget::new(12, 0).is_none());
    assert!(H264::new(PacketBudget::new(14, 0).unwrap()).is_none());
    let opus = Opus::new(PacketBudget::new(28, 12).unwrap());
    assert_eq!(
        opus.packetize(4, 960, false, &[]),
        Err(PacketizeError::Empty)
    );
    assert_eq!(
        opus.packetize(4, 960, false, &[0; 5]),
        Err(PacketizeError::TooLarge)
    );
    let supplied = [0xc0, 0, 0, 0x80, 0, 0xe0];
    let extensions = [Extension {
        kind: ExtensionKind::DependencyDescriptor,
        value: &supplied,
    }];
    let unit = [0xf8, 0xff, 0xfe, 0x01];
    let packet = opus.packetize(4, 960, false, &unit).unwrap();
    assert!(packet.first && packet.last && !packet.marker);
    let send = packet.as_send_packet(&extensions);
    assert_eq!(
        (send.sequence, send.timestamp, send.marker),
        (4, 960, false)
    );
    assert_eq!(send.payload, unit);
    let parsed = Reader::new().read(send.extensions[0].value).unwrap();
    assert!(parsed.start_of_frame && parsed.end_of_frame);
}
