use crate::dd::{DependencyDescriptorWriter, MAX_DD_LEN, test_utils::*};

use super::*;

fn encode(writer: &mut DependencyDescriptorWriter, dd: &DependencyDescriptor) -> Vec<u8> {
    let mut out = [0; MAX_DD_LEN];
    let len = writer.write(dd, &mut out).unwrap();
    out[..len].to_vec()
}

#[test]
fn reused_templates_after_a_gap_need_new_context() {
    let mut older = structure_minimal();
    older.template_id_offset = 5;
    let mut newer = structure_l1t3();
    newer.template_id_offset = 5;
    let mut writer = DependencyDescriptorWriter::new();
    let mut source = SourceContext::default();
    let initial = encode(&mut writer, &keyframe(&older));
    assert_eq!(
        source
            .observe(100, Some(&initial))
            .unwrap()
            .unwrap()
            .active_targets,
        1
    );
    let missing_structure = encode(&mut writer, &keyframe(&newer));
    let ambiguous = encode(&mut writer, &delta(&newer, 0, 1));
    assert!(source.observe(111, Some(&ambiguous)).unwrap().is_none());
    let late = source
        .observe(110, Some(&missing_structure))
        .unwrap()
        .unwrap();
    assert_eq!(late.active_targets, 7);
    assert!(source.observe(112, Some(&ambiguous)).unwrap().is_none());
    assert_eq!(
        source
            .observe(113, Some(&missing_structure))
            .unwrap()
            .unwrap()
            .active_targets,
        7
    );

    let mut update = delta(&newer, 0, 2);
    update.active_decode_targets = Some(3);
    let explicit_mask = encode(&mut writer, &update);
    assert_eq!(
        source
            .observe(114, Some(&explicit_mask))
            .unwrap()
            .unwrap()
            .active_targets,
        3
    );
    assert_eq!(
        source
            .observe(115, Some(&ambiguous))
            .unwrap()
            .unwrap()
            .active_targets,
        3
    );
    assert!(source.observe(116, None).unwrap().is_none());
    assert!(source.observe(117, Some(&ambiguous)).unwrap().is_none());
}

#[test]
fn repeated_structure_retains_origin_only_with_contiguous_evidence() {
    let structure = structure_minimal();
    let mut writer = DependencyDescriptorWriter::new();
    let repeated = encode(&mut writer, &keyframe(&structure));
    let mut changed = structure;
    changed.template_id_offset = 4;
    let changed = encode(&mut writer, &keyframe(&changed));
    let mut source = SourceContext::default();
    for (sequence, bytes, origin) in [
        (100, &repeated, 100),
        (101, &repeated, 100),
        (102, &changed, 102),
        (104, &changed, 104),
    ] {
        let qualified = source.observe(sequence, Some(bytes)).unwrap().unwrap();
        assert_eq!(
            (qualified.structure_sequence, qualified.structure_origin),
            (sequence, origin)
        );
    }
    let late = source.qualify(101, Some(&repeated)).unwrap().unwrap();
    assert_eq!((late.structure_sequence, late.structure_origin), (101, 100));
}

#[test]
fn authenticated_padding_preserves_context_but_does_not_bridge_an_unknown_position() {
    let structure = structure_minimal();
    let mut writer = DependencyDescriptorWriter::new();
    let seed = encode(&mut writer, &keyframe(&structure));
    let plain = encode(&mut writer, &delta(&structure, 0, 1));
    let mut source = SourceContext::default();
    source.observe(100, Some(&seed)).unwrap().unwrap();
    source.bridge_padding_before(103, |range| range == (101..103));
    assert_eq!(
        source
            .observe(103, Some(&plain))
            .unwrap()
            .unwrap()
            .structure_sequence,
        100
    );
    source.bridge_padding_before(106, |range| range == (105..106));
    assert!(source.observe(106, Some(&plain)).unwrap().is_none());
    source.bridge_padding_before(108, |range| range == (107..108));
    assert!(source.observe(108, Some(&plain)).unwrap().is_none());
}

#[test]
fn late_packets_read_their_original_mask_without_rolling_back_current_context() {
    let structure = structure_l1t3();
    let mut writer = DependencyDescriptorWriter::new();
    let seed = encode(&mut writer, &keyframe(&structure));
    let plain = encode(&mut writer, &delta(&structure, 0, 1));
    let mut update = delta(&structure, 0, 2);
    update.active_decode_targets = Some(1);
    let update = encode(&mut writer, &update);
    let mut source = SourceContext::default();
    source.observe(20, Some(&seed)).unwrap();
    source.observe(21, Some(&plain)).unwrap();
    source.observe(22, Some(&update)).unwrap();
    assert_eq!(
        source
            .observe(21, Some(&plain))
            .unwrap()
            .unwrap()
            .active_targets,
        7
    );
    assert_eq!(
        source
            .observe(23, Some(&plain))
            .unwrap()
            .unwrap()
            .active_targets,
        1
    );
    source.observe(4117, None).unwrap();
    assert!(source.observe(21, Some(&plain)).unwrap().is_none());
}

#[test]
fn frame_conflict_is_sticky_and_history_is_bounded_across_wrap() {
    use crate::dd::DecodeTargetIndication;
    let mut frames = Frames::with_anchor(65_535);
    let structure = structure_minimal();
    let mut descriptor = delta(&structure, 0, u16::MAX);
    descriptor.end_of_frame = false;
    let mut evidence = Qualified {
        descriptor,
        active_targets: 1,
        structure_sequence: 0,
        structure_origin: 0,
        target_chains: structure.decode_target_protected_by,
    };
    assert_eq!(frames.observe(20, &evidence), FrameStatus::Resolved(65_535));
    evidence.descriptor.start_of_frame = false;
    evidence.descriptor.end_of_frame = true;
    assert_eq!(frames.observe(22, &evidence), FrameStatus::Resolved(65_535));
    evidence.descriptor.end_of_frame = false;
    evidence.descriptor.frame_dependencies.dtis[0] = DecodeTargetIndication::NotPresent;
    assert_eq!(
        frames.observe(21, &evidence),
        FrameStatus::Contradictory(65_535)
    );
    assert!(!frames.is_consistent(65_535));
    evidence.descriptor.frame_number = 0;
    assert_eq!(frames.observe(23, &evidence), FrameStatus::Resolved(65_536));
    evidence.descriptor.frame_number = 32_768;
    assert_eq!(frames.observe(24, &evidence), FrameStatus::Unresolved);
    evidence.descriptor.frame_number = 511;
    assert_eq!(
        frames.observe(535, &evidence),
        FrameStatus::Resolved(66_047)
    );
    evidence.descriptor.frame_number = u16::MAX;
    assert_eq!(frames.observe(20, &evidence), FrameStatus::Unresolved);
}

#[test]
fn filtered_entry_refusal_and_suspension_fence_are_transactional() {
    use crate::dd::DecodeTargetIndication;
    let structure = structure_l1t3();
    let mut evidence = Qualified {
        descriptor: delta(&structure, 0, 0),
        active_targets: 7,
        structure_sequence: 0,
        structure_origin: 0,
        target_chains: structure.decode_target_protected_by,
    };
    let mut branch = FilteredBranch::new(DecodeTarget::new(0).unwrap(), true);
    let never = |_| panic!("ineligible packet reached enqueue");
    let offer =
        |branch: &mut FilteredBranch, seq, evidence: &Qualified, action: fn(Enqueue) -> bool| {
            branch.offer(
                seq,
                FrameStatus::Resolved(u64::from(evidence.descriptor.frame_number)),
                Some(evidence),
                action,
            )
        };
    assert_eq!(
        offer(&mut branch, 100, &evidence, never),
        Decision::AwaitingEntry
    );
    evidence.descriptor.frame_dependencies.frame_diffs.clear();
    assert_eq!(
        offer(&mut branch, 101, &evidence, |kind| {
            assert_eq!(kind, Enqueue::Enter);
            false
        }),
        Decision::Refused
    );
    assert_eq!(
        offer(&mut branch, 101, &evidence, |kind| {
            assert_eq!(kind, Enqueue::Enter);
            true
        }),
        Decision::Entered
    );

    evidence.descriptor.frame_number = 1;
    evidence.descriptor.frame_dependencies.dtis[0] = DecodeTargetIndication::NotPresent;
    assert_eq!(offer(&mut branch, 102, &evidence, never), Decision::Omitted);
    evidence.active_targets = 6;
    assert_eq!(
        offer(&mut branch, 103, &evidence, never),
        Decision::Suspended
    );
    assert_eq!(offer(&mut branch, 102, &evidence, never), Decision::Fenced);
    evidence.descriptor.frame_number = 2;
    evidence.descriptor.frame_dependencies.dtis[0] = DecodeTargetIndication::Required;
    evidence.active_targets = 7;
    assert_eq!(
        offer(&mut branch, 104, &evidence, |_| true),
        Decision::Entered
    );
    assert_eq!(
        branch.offer(103, FrameStatus::Unresolved, None, never),
        Decision::Fenced
    );
    evidence.descriptor.frame_number = 3;
    evidence.descriptor.frame_dependencies.frame_diffs.push(1);
    assert_eq!(
        offer(&mut branch, 105, &evidence, |kind| {
            assert_eq!(kind, Enqueue::Continue);
            true
        }),
        Decision::Admitted
    );
}

#[test]
fn in_source_switch_requires_complete_transitive_frame_and_chain_ancestry() {
    use crate::dd::DecodeTargetIndication::{Required, Switch};
    for (missing_fragment, bad_chain, wanted) in [
        (false, false, Enqueue::Enter),
        (true, false, Enqueue::Continue),
        (false, true, Enqueue::Continue),
    ] {
        let structure = structure_l1t3();
        let mut root = delta(&structure, 0, 10);
        root.frame_dependencies.dtis.fill(Required);
        root.frame_dependencies.frame_diffs.clear();
        let mut evidence = Qualified {
            descriptor: root,
            active_targets: 7,
            structure_sequence: 0,
            structure_origin: 0,
            target_chains: structure.decode_target_protected_by,
        };
        let mut branch = FilteredBranch::new(DecodeTarget::new(0).unwrap(), true);
        let offer = |branch: &mut FilteredBranch, seq, evidence: &Qualified, expected, accept| {
            branch.offer(
                seq,
                FrameStatus::Resolved(u64::from(evidence.descriptor.frame_number)),
                Some(evidence),
                |kind| {
                    assert_eq!(kind, expected);
                    accept
                },
            )
        };
        evidence.descriptor.end_of_frame = false;
        assert_eq!(
            offer(&mut branch, 100, &evidence, Enqueue::Enter, true),
            Decision::Entered
        );
        evidence.descriptor.start_of_frame = false;
        if !missing_fragment {
            assert_eq!(
                offer(&mut branch, 101, &evidence, Enqueue::Continue, true),
                Decision::Admitted
            );
        }
        evidence.descriptor.end_of_frame = true;
        assert_eq!(
            offer(&mut branch, 102, &evidence, Enqueue::Continue, true),
            Decision::Admitted
        );
        evidence.descriptor.start_of_frame = true;
        evidence.descriptor.frame_number = 11;
        evidence.descriptor.frame_dependencies.frame_diffs.push(1);
        evidence.descriptor.frame_dependencies.chain_diffs[0] = 1;
        assert_eq!(
            offer(&mut branch, 103, &evidence, Enqueue::Continue, true),
            Decision::Admitted
        );
        branch.select(DecodeTarget::new(1).unwrap());
        evidence.descriptor.frame_number = 12;
        evidence.descriptor.frame_dependencies.dtis[1] = Switch;
        if bad_chain {
            evidence.descriptor.frame_dependencies.chain_diffs[0] = 3;
        }
        assert_eq!(
            offer(&mut branch, 104, &evidence, wanted, false),
            Decision::Refused
        );
        let expected = if wanted == Enqueue::Enter {
            Decision::Entered
        } else {
            Decision::Admitted
        };
        assert_eq!(offer(&mut branch, 104, &evidence, wanted, true), expected);
    }
}

#[test]
fn unresolved_is_not_invalid_and_invalid_does_not_change_context() {
    let structure = structure_minimal();
    let mut writer = DependencyDescriptorWriter::new();
    let seed = encode(&mut writer, &keyframe(&structure));
    let delta = encode(&mut writer, &delta(&structure, 0, 2));
    assert!(parse_independent(&delta).unwrap().is_none());
    assert_eq!(
        parse_independent(&[0xc0, 0, 1, 0x80]).unwrap_err(),
        DdReadError::Truncated
    );
    let mut source = SourceContext::default();
    source.observe(10, Some(&seed)).unwrap();
    assert_eq!(
        source.observe(11, Some(&[0xc0])).unwrap_err(),
        DdReadError::Truncated
    );
    assert!(source.observe(11, Some(&delta)).unwrap().is_some());
}
