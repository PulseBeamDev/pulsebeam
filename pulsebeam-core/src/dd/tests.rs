#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing
)] // tests assert by panicking
use proptest::prelude::*;

use super::model::*;
use super::read::{DdReadError, DependencyDescriptorReader, read_mandatory};
use super::test_utils::*;
use super::write::{DdWriteError, DependencyDescriptorWriter};

use arrayvec::ArrayVec;

fn arb_dti() -> impl Strategy<Value = DecodeTargetIndication> {
    (0u32..4).prop_map(DecodeTargetIndication::from_bits)
}

prop_compose! {
    fn arb_structure_inner(
        spatial: usize,
        temporal: usize,
        dt_cnt: usize,
        chain_count: usize,
    )(
        template_id_offset in 0u8..64,
        dti_grid in prop::collection::vec(
            prop::collection::vec(arb_dti(), dt_cnt),
            spatial * temporal,
        ),
        fdiff_grid in prop::collection::vec(
            prop::collection::vec(1u16..=16, 0..3),
            spatial * temporal,
        ),
        chain_grid in prop::collection::vec(
            prop::collection::vec(0u8..16, chain_count),
            spatial * temporal,
        ),
        protected_by in prop::collection::vec(0usize..chain_count.max(1), dt_cnt),
        with_resolutions in any::<bool>(),
        // Up to the full field, not a plausible-looking 4096: the coded form is
        // `minus_1`, so the interesting values are at the ends.
        dims in prop::collection::vec((1u16..=u16::MAX, 1u16..=u16::MAX), spatial),
    ) -> TemplateDependencyStructure {
        let mut templates = ArrayVec::new();
        for s in 0..spatial {
            for t in 0..temporal {
                let i = s * temporal + t;
                templates.push(FrameDependencyTemplate {
                    spatial_id: u8::try_from(s).expect("spatial index fits a u8"),
                    temporal_id: u8::try_from(t).expect("temporal index fits a u8"),
                    dtis: dti_grid[i].iter().copied().collect(),
                    frame_diffs: fdiff_grid[i].iter().copied().collect(),
                    chain_diffs: chain_grid[i].iter().copied().collect(),
                });
            }
        }

        let resolutions = if with_resolutions {
            dims.iter()
                .map(|&(width, height)| RenderResolution { width, height })
                .collect()
        } else {
            ArrayVec::new()
        };

        TemplateDependencyStructure {
            template_id_offset,
            decode_target_count: u8::try_from(dt_cnt).expect("dt count fits a u8"),
            chain_count: u8::try_from(chain_count).expect("chain count fits a u8"),
            decode_target_protected_by: if chain_count == 0 {
                ArrayVec::new()
            } else {
                protected_by
                    .iter()
                    .map(|&v| u8::try_from(v).expect("chain index fits a u8"))
                    .collect()
            },
            templates,
            resolutions,
        }
    }
}

pub fn arb_structure() -> impl Strategy<Value = TemplateDependencyStructure> {
    (1usize..=3, 1usize..=3, 1usize..=4)
        .prop_flat_map(|(spatial, temporal, dt_cnt)| {
            (Just((spatial, temporal, dt_cnt)), 0usize..=dt_cnt)
        })
        .prop_flat_map(|((spatial, temporal, dt_cnt), chain_count)| {
            arb_structure_inner(spatial, temporal, dt_cnt, chain_count)
        })
}

/// Descriptors are drawn against a concrete structure so their template ids and
/// field widths are always consistent with it; independently drawn descriptors
/// would be rejected before exercising anything.
fn arb_descriptor(
    structure: TemplateDependencyStructure,
) -> impl Strategy<Value = DependencyDescriptor> {
    let dt_cnt = usize::from(structure.decode_target_count);
    let chain_count = usize::from(structure.chain_count);
    let template_count = structure.templates.len();

    (
        0..template_count,
        any::<u16>(),
        any::<bool>(),
        any::<bool>(),
        prop::option::of(0u32..(1u32 << dt_cnt)),
        prop::collection::vec(arb_dti(), dt_cnt),
        prop::option::of(prop::collection::vec(1u16..=4096, 0..MAX_FRAME_DIFFS)),
        prop::option::of(prop::collection::vec(0u8..=255, chain_count)),
    )
        .prop_map(
            move |(
                index,
                frame_number,
                start_of_frame,
                end_of_frame,
                active_decode_targets,
                custom_dtis,
                custom_fdiffs,
                custom_chains,
            )| {
                let mut dd = delta(&structure, index, frame_number);
                dd.start_of_frame = start_of_frame;
                dd.end_of_frame = end_of_frame;
                dd.active_decode_targets = active_decode_targets;

                if !custom_dtis.is_empty() && custom_dtis != dd.frame_dependencies.dtis[..] {
                    dd.fields.set_custom_dtis(true);
                    dd.frame_dependencies.dtis = custom_dtis.iter().copied().collect();
                }
                if let Some(fdiffs) = custom_fdiffs {
                    dd.fields.set_custom_fdiffs(true);
                    dd.frame_dependencies.frame_diffs = fdiffs.iter().copied().collect();
                }
                if let Some(chains) = custom_chains {
                    dd.fields.set_custom_chains(true);
                    dd.frame_dependencies.chain_diffs = chains.iter().copied().collect();
                }
                dd
            },
        )
}

/// A keyframe carrying the structure, followed by delta frames against it, with
/// occasional mid-sequence structure changes.
pub fn arb_descriptor_sequence() -> impl Strategy<Value = Vec<DependencyDescriptor>> {
    prop::collection::vec(arb_structure(), 1..3).prop_flat_map(|structures| {
        let per_structure: Vec<_> = structures
            .into_iter()
            .map(|s| {
                let key = keyframe(&s);
                prop::collection::vec(arb_descriptor(s), 0..6).prop_map(move |deltas| {
                    let mut out = vec![key.clone()];
                    out.extend(deltas);
                    out
                })
            })
            .collect();
        per_structure.prop_map(|groups| groups.into_iter().flatten().collect())
    })
}

fn encode(dd: &DependencyDescriptor) -> Vec<u8> {
    let mut w = DependencyDescriptorWriter::new();
    let mut buf = [0u8; MAX_DD_LEN];
    let len = w.write(dd, &mut buf).expect("encode");
    buf[..len].to_vec()
}

/// Hand-derived from the wire format, independent of this crate's encoder:
/// mandatory `1|1|000000|0x0000`, then flags `10000`, then the minimal
/// structure `000000|00000|11|10|0|0|0`.
const MINIMAL_KEYFRAME_BYTES: [u8; 6] = [0xC0, 0x00, 0x00, 0x80, 0x00, 0xE0];

#[test]
fn decodes_hand_derived_minimal_keyframe() {
    let mut r = DependencyDescriptorReader::new();
    let dd = r.read(&MINIMAL_KEYFRAME_BYTES).unwrap();

    assert!(dd.start_of_frame);
    assert!(dd.end_of_frame);
    assert_eq!(dd.template_id, 0);
    assert_eq!(dd.frame_number, 0);
    assert_eq!(dd.attached_structure.as_deref(), Some(&structure_minimal()));
    assert_eq!(dd.frame_dependencies.dtis[..], dtis("S")[..]);
    assert!(dd.fields.is_empty());
}

#[test]
fn encodes_hand_derived_minimal_keyframe() {
    assert_eq!(
        encode(&keyframe(&structure_minimal())),
        MINIMAL_KEYFRAME_BYTES
    );
}

#[test]
fn mandatory_fields_decode_without_any_structure() {
    let m = read_mandatory(&[0x80, 0x01, 0x02]).unwrap();
    assert!(m.start_of_frame);
    assert!(!m.end_of_frame);
    assert_eq!(m.template_id, 0);
    assert_eq!(m.frame_number, 258);

    let m = read_mandatory(&[0x7F, 0xFF, 0xFF]).unwrap();
    assert!(!m.start_of_frame);
    assert!(m.end_of_frame);
    assert_eq!(m.template_id, 63);
    assert_eq!(m.frame_number, 65535);
}

#[test]
fn delta_frames_are_exactly_three_bytes() {
    for s in all_structures() {
        let mut w = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];
        w.write(&keyframe(&s), &mut buf).unwrap();

        for i in 0..s.templates.len() {
            let len = w
                .write(
                    &delta(&s, i, u16::try_from(i).expect("loop index fits a u16")),
                    &mut buf,
                )
                .unwrap();
            assert_eq!(len, MANDATORY_LEN, "template {i} of {s:?}");
        }
    }
}

#[test]
fn structures_survive_a_keyframe_then_delta_exchange() {
    for s in all_structures() {
        let mut writer = DependencyDescriptorWriter::new();
        let mut reader = DependencyDescriptorReader::new();
        let mut buf = [0u8; MAX_DD_LEN];

        let len = writer.write(&keyframe(&s), &mut buf).unwrap();
        let decoded = reader.read(&buf[..len]).unwrap();
        assert_eq!(decoded.attached_structure.as_deref(), Some(&s));

        for i in 0..s.templates.len() {
            let sent = delta(
                &s,
                i,
                100 + u16::try_from(i).expect("loop index fits a u16"),
            );
            let len = writer.write(&sent, &mut buf).unwrap();
            let got = reader.read(&buf[..len]).unwrap();
            assert_eq!(got, sent, "template {i}");
        }
    }
}

#[test]
fn resolutions_are_reported_per_spatial_layer() {
    let s = structure_l3t1_with_resolutions();
    let mut reader = DependencyDescriptorReader::new();
    let mut buf = [0u8; MAX_DD_LEN];
    let mut writer = DependencyDescriptorWriter::new();

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();

    for (i, expected) in s.resolutions.iter().enumerate() {
        let len = writer
            .write(
                &delta(&s, i, u16::try_from(i).expect("loop index fits a u16")),
                &mut buf,
            )
            .unwrap();
        let got = reader.read(&buf[..len]).unwrap();
        assert_eq!(got.resolution.as_ref(), Some(expected));
        assert_eq!(
            got.spatial_id(),
            u8::try_from(i).expect("loop index fits a u8")
        );
    }
}

#[test]
fn reader_resolves_template_across_id_offset_wraparound() {
    let mut s = structure_l1t3();
    s.template_id_offset = 62;

    let mut writer = DependencyDescriptorWriter::new();
    let mut reader = DependencyDescriptorReader::new();
    let mut buf = [0u8; MAX_DD_LEN];

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();

    // Templates 0,1,2 map to wire ids 62, 63, 0.
    for (index, wire_id) in [(0usize, 62u8), (1, 63), (2, 0)] {
        let sent = delta(&s, index, u16::try_from(index).expect("index fits a u16"));
        assert_eq!(sent.template_id, wire_id);
        let len = writer.write(&sent, &mut buf).unwrap();
        let got = reader.read(&buf[..len]).unwrap();
        assert_eq!(
            got.temporal_id(),
            u8::try_from(index).expect("index fits a u8")
        );
    }
}

#[test]
fn reader_requires_prior_structure_for_mandatory_only_descriptor() {
    let mut r = DependencyDescriptorReader::new();
    assert_eq!(r.read(&[0x80, 0x00, 0x00]), Err(DdReadError::NoStructure));
}

#[test]
fn reader_rejects_unknown_template_id() {
    let s = structure_l1t3();
    let mut writer = DependencyDescriptorWriter::new();
    let mut reader = DependencyDescriptorReader::new();
    let mut buf = [0u8; MAX_DD_LEN];

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();

    // Structure has 3 templates; id 40 resolves past the end.
    assert_eq!(
        reader.read(&[0x80 | 40, 0x00, 0x00]),
        Err(DdReadError::UnknownTemplateId(40))
    );
}

#[test]
fn new_structure_resets_active_decode_targets_to_all() {
    let mut reader = DependencyDescriptorReader::new();
    let mut writer = DependencyDescriptorWriter::new();
    let mut buf = [0u8; MAX_DD_LEN];

    let s = structure_l1t3();
    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();
    assert_eq!(reader.active_decode_targets(), 0b111);

    let mut narrowed = delta(&s, 0, 1);
    narrowed.active_decode_targets = Some(0b001);
    let len = writer.write(&narrowed, &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();
    assert_eq!(reader.active_decode_targets(), 0b001);

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();
    assert_eq!(reader.active_decode_targets(), 0b111);
}

#[test]
fn active_decode_targets_persist_until_a_new_bitmask_arrives() {
    let s = structure_l1t3();
    let mut reader = DependencyDescriptorReader::new();
    let mut writer = DependencyDescriptorWriter::new();
    let mut buf = [0u8; MAX_DD_LEN];

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();

    let mut narrowed = delta(&s, 0, 1);
    narrowed.active_decode_targets = Some(0b011);
    let len = writer.write(&narrowed, &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();

    for i in 0..3 {
        let len = writer
            .write(
                &delta(&s, i, 10 + u16::try_from(i).expect("loop index fits a u16")),
                &mut buf,
            )
            .unwrap();
        reader.read(&buf[..len]).unwrap();
        assert_eq!(reader.active_decode_targets(), 0b011);
    }
}

#[test]
fn failed_parse_leaves_reader_state_unchanged() {
    let s = structure_l1t3();
    let mut writer = DependencyDescriptorWriter::new();
    let mut reader = DependencyDescriptorReader::new();
    let mut buf = [0u8; MAX_DD_LEN];

    let len = writer.write(&keyframe(&s), &mut buf).unwrap();
    reader.read(&buf[..len]).unwrap();
    let before = reader.structure().cloned();
    let mask_before = reader.active_decode_targets();

    // A truncated keyframe carrying a different structure must not be adopted.
    let other = structure_l2t1_key();
    let mut w2 = DependencyDescriptorWriter::new();
    let len = w2.write(&keyframe(&other), &mut buf).unwrap();
    for cut in MANDATORY_LEN..len {
        let _ = reader.read(&buf[..cut]);
        assert_eq!(reader.structure().cloned(), before, "cut at {cut}");
        assert_eq!(reader.active_decode_targets(), mask_before);
    }
}

#[test]
fn oversized_extension_is_rejected() {
    let mut r = DependencyDescriptorReader::new();
    let buf = vec![0u8; MAX_DD_LEN + 1];
    assert_eq!(r.read(&buf), Err(DdReadError::TooLong(MAX_DD_LEN + 1)));
}

#[test]
fn writer_rejects_descriptor_inconsistent_with_structure() {
    let s = structure_l1t3();
    let mut writer = DependencyDescriptorWriter::new();
    let mut buf = [0u8; MAX_DD_LEN];
    writer.write(&keyframe(&s), &mut buf).unwrap();

    let mut wrong_dtis = delta(&s, 0, 1);
    wrong_dtis.frame_dependencies.dtis.pop();
    assert_eq!(
        writer.write(&wrong_dtis, &mut buf),
        Err(DdWriteError::Inconsistent)
    );

    let mut wrong_chains = delta(&s, 0, 1);
    wrong_chains.frame_dependencies.chain_diffs.clear();
    assert_eq!(
        writer.write(&wrong_chains, &mut buf),
        Err(DdWriteError::Inconsistent)
    );
}

#[test]
fn writer_requires_a_structure() {
    let mut writer = DependencyDescriptorWriter::new();
    let mut buf = [0u8; MAX_DD_LEN];
    let dd = delta(&structure_l1t3(), 0, 0);
    assert_eq!(writer.write(&dd, &mut buf), Err(DdWriteError::NoStructure));
}

#[test]
fn writer_reports_overflow_for_undersized_buffer() {
    let mut writer = DependencyDescriptorWriter::new();
    let mut buf = [0u8; 2];
    assert_eq!(
        writer.write(&keyframe(&structure_l1t3()), &mut buf),
        Err(DdWriteError::Overflow)
    );
}

#[cfg(feature = "str0m")]
mod serializer {
    use super::*;
    use crate::dd::{RawDependencyDescriptor, Serializer};
    use str0m::rtp::{ExtensionSerializer, ExtensionValues};

    #[test]
    fn roundtrips_raw_bytes_through_extension_values() {
        let bytes = encode(&keyframe(&structure_l1t3()));
        let mut ev = ExtensionValues::default();
        assert!(Serializer.parse_value(&bytes, &mut ev));

        let mut out = [0u8; MAX_DD_LEN];
        let n = Serializer.write_to(&mut out, &ev);
        assert_eq!(&out[..n], &bytes[..]);
    }

    #[test]
    fn rejects_under_and_oversized_buffers() {
        let mut ev = ExtensionValues::default();
        assert!(!Serializer.parse_value(&[0x80, 0x00], &mut ev));
        assert!(!Serializer.parse_value(&vec![0u8; MAX_DD_LEN + 1], &mut ev));
        assert!(ev.user_values.get::<RawDependencyDescriptor>().is_none());
    }

    #[test]
    fn requires_two_byte_form_only_when_value_present() {
        let mut ev = ExtensionValues::default();
        assert!(!Serializer.requires_two_byte_form(&ev));

        let bytes = encode(&keyframe(&structure_l1t3()));
        assert!(Serializer.parse_value(&bytes, &mut ev));
        assert!(Serializer.requires_two_byte_form(&ev));
    }
}

proptest! {
    #[test]
    fn roundtrip_preserves_descriptors(sequence in arb_descriptor_sequence()) {
        let mut writer = DependencyDescriptorWriter::new();
        let mut reader = DependencyDescriptorReader::new();
        let mut buf = [0u8; MAX_DD_LEN];

        for sent in &sequence {
            let len = writer.write(sent, &mut buf).unwrap();
            let got = reader.read(&buf[..len]).unwrap();
            prop_assert_eq!(&got, sent);
            prop_assert_eq!(reader.structure(), writer.structure());
        }
    }

    #[test]
    fn reencoding_a_decoded_descriptor_is_byte_identical(sequence in arb_descriptor_sequence()) {
        let mut writer = DependencyDescriptorWriter::new();
        let mut reader = DependencyDescriptorReader::new();
        let mut reencoder = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];
        let mut again = [0u8; MAX_DD_LEN];

        for sent in &sequence {
            let len = writer.write(sent, &mut buf).unwrap();
            let got = reader.read(&buf[..len]).unwrap();
            let len2 = reencoder.write(&got, &mut again).unwrap();
            prop_assert_eq!(&buf[..len], &again[..len2]);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        raw in prop::collection::vec(any::<u8>(), 0..300),
        seed in prop::option::of(0usize..4),
    ) {
        let mut reader = DependencyDescriptorReader::new();
        if let Some(i) = seed {
            let s = all_structures()[i].clone();
            let mut writer = DependencyDescriptorWriter::new();
            let mut buf = [0u8; MAX_DD_LEN];
            let len = writer.write(&keyframe(&s), &mut buf).unwrap();
            reader.read(&buf[..len]).unwrap();
        }
        let _ = reader.read(&raw);
    }

    #[test]
    fn every_prefix_of_a_valid_descriptor_errors_without_panicking(
        sequence in arb_descriptor_sequence(),
    ) {
        let mut writer = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];

        for sent in &sequence {
            let len = writer.write(sent, &mut buf).unwrap();
            for cut in 0..len {
                let mut reader = DependencyDescriptorReader::new();
                let _ = reader.read(&buf[..cut]);
            }
        }
    }

    #[test]
    fn bit_flips_never_panic(
        sequence in arb_descriptor_sequence(),
        bit in 0usize..2048,
    ) {
        let mut writer = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];

        for sent in &sequence {
            let len = writer.write(sent, &mut buf).unwrap();
            let mut corrupted = buf[..len].to_vec();
            let byte = (bit / 8) % len;
            corrupted[byte] ^= 1 << (bit % 8);

            let mut reader = DependencyDescriptorReader::new();
            let _ = reader.read(&corrupted);
        }
    }
}
