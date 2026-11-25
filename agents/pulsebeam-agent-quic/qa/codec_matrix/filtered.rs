use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use pulsebeam_agent_quic::media::{
    jitter::GapReason,
    units::{Depacketizer, Event, OrderedPacket, Recovery, VideoRecovery},
};
use pulsebeam_core::dd::{
    DecodeTargetIndication::NotPresent,
    DependencyDescriptorReader,
    evidence::{DecodeTarget, Enqueue, FilteredBranch, FrameStatus, Frames, SourceContext},
    output::{Evidence, Mode, OutputMapper, Packet as MappingPacket},
};
use serde_json::Value;
use wire_capture::{Cursor, PacketKind};

use super::{CASES, Case, captured_payload, pilots, transitive_ancestors};

#[derive(Clone, Copy)]
pub(super) enum Fault<'a> {
    None,
    Refused(&'a [usize]),
    Lost(usize),
}

fn replay(
    case: &Case,
    observed: &Value,
    initial_target: u8,
    switch: Option<(usize, u8)>,
    fault: Fault<'_>,
) -> Vec<usize> {
    replay_output(
        case,
        observed,
        initial_target,
        switch,
        fault,
        |_, _, _, _, _| {},
    )
}

pub(super) fn replay_output(
    case: &Case,
    observed: &Value,
    initial_target: u8,
    switch: Option<(usize, u8)>,
    fault: Fault<'_>,
    mut observe_rtp: impl FnMut(u64, u32, bool, &[u8], &[u8]),
) -> Vec<usize> {
    let units = observed["wire_units"].as_array().unwrap();
    let mut by_packet = BTreeMap::new();
    for (i, unit) in units.iter().enumerate() {
        for packet in unit["packets"].as_array().unwrap() {
            assert!(
                by_packet
                    .insert(packet.as_u64().unwrap() as usize, i)
                    .is_none()
            );
        }
    }
    let encoded: Vec<_> = case.encoded_index.lines().skip(1).collect();
    let at = Instant::now();
    let mut desired = initial_target;
    let mut branch = FilteredBranch::new(DecodeTarget::new(desired).unwrap(), true);
    let mut mapper = OutputMapper::new(u16::MAX - 2);
    let mut context = SourceContext::default();
    let mut frames = Frames::default();
    let mut cursor = Cursor::new(case.wire, case.index).unwrap();
    let mut receiver = Depacketizer::new(0, case.codec());
    let mut recovery = VideoRecovery::new(0);
    let mut outgoing = u64::from(u16::MAX) - 3;
    let mut selected = Vec::new();
    let mut lost = false;
    let mut sequence_high = None::<u64>;
    while let Some(packet) = cursor.next_due(Duration::from_secs(2)) {
        if packet.kind != PacketKind::Rtp {
            continue;
        }
        let unit = by_packet[&packet.index];
        if let Some((first, target)) = switch
            && first == packet.index
        {
            desired = target;
            branch.select(DecodeTarget::new(target).unwrap());
        }
        let bytes = packet.bytes;
        let raw = u16::from_be_bytes([bytes[2], bytes[3]]);
        let sequence = sequence_high.map_or(u64::from(raw), |old| {
            old + u64::from(raw.wrapping_sub(old as u16))
        });
        sequence_high = Some(sequence);
        let (payload, dd) = captured_payload(bytes);
        let evidence = context.observe(sequence, Some(dd)).unwrap().unwrap();
        let frame = frames.observe(sequence, &evidence);
        let FrameStatus::Resolved(frame_number) = frame else {
            panic!("{} packet {}: {frame:?}", case.mode, packet.index);
        };
        let now = at + packet.send_after;
        let timestamp = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let current_target = branch.selected_target().index();
        branch.offer(sequence, frame, Some(&evidence), |enqueue| {
            if matches!(fault, Fault::Refused(exclude) if exclude.contains(&packet.index)) {
                return false;
            }
            let entry = enqueue == Enqueue::Enter;
            let target = if entry { desired } else { current_target };
            let result = mapper.offer(
                now,
                now + Duration::from_millis(100),
                MappingPacket {
                    source: 0,
                    output_sequence: outgoing,
                    mode: Mode::Filtered(target),
                    entry,
                    dd: Some(Evidence {
                        qualified: &evidence,
                        frame: frame_number,
                    }),
                },
                |mapped| {
                    if matches!(fault, Fault::Lost(index) if index == packet.index) {
                        let gap = receiver
                            .gap(0, outgoing..outgoing + 1, GapReason::Deadline)
                            .unwrap();
                        assert!(matches!(
                            recovery.apply(Event::Discontinuity(gap)),
                            Recovery::Deliver(Event::Discontinuity(_))
                        ));
                        assert_eq!(recovery.take_refresh(), Some(0));
                        assert_eq!(recovery.take_refresh(), None);
                        lost = true;
                        outgoing += 1;
                        return true;
                    }
                    let mapped = mapped.unwrap();
                    let marker = bytes[1] & 128 != 0;
                    observe_rtp(outgoing, timestamp, marker, payload, mapped);
                    let event = receiver
                        .push(
                            0,
                            OrderedPacket {
                                sequence: outgoing,
                                timestamp,
                                rtp_time: Some(u64::from(timestamp)),
                                arrived_at: now,
                                marker,
                                payload,
                                descriptor: Some(mapped),
                            },
                        )
                        .unwrap_or_else(|err| {
                            panic!(
                                "{} target {target} packet {}: {err:?}",
                                case.mode, packet.index
                            )
                        });
                    if let Some(event) = event {
                        let output = match recovery.apply(event) {
                            Recovery::Deliver(Event::Unit(output)) => output,
                            Recovery::Withheld(_) | Recovery::Deliver(Event::Discontinuity(_))
                                if lost =>
                            {
                                outgoing += 1;
                                return true;
                            }
                            Recovery::Withheld(unit_info) => panic!(
                                "{} {} target {target} unit {unit}: withheld {:?}, {:?}",
                                case.codec, case.mode, unit_info.access, unit_info.framing,
                            ),
                            _ => panic!(
                                "{} {} target {target} unit {unit}: discontinuity",
                                case.codec, case.mode
                            ),
                        };
                        assert_eq!(
                            output.rtp_time,
                            Some(units[unit]["rtp_timestamp"].as_u64().unwrap()),
                            "{} {} target {target} unit {unit}",
                            case.codec,
                            case.mode
                        );
                        case.assert_bytes(observed, unit, encoded[unit], &output.data);
                        selected.push(unit);
                    }
                    outgoing += 1;
                    true
                },
            );
            result.is_ok()
        });
    }
    assert_eq!(
        lost,
        matches!(fault, Fault::Lost(_)),
        "{} {}: delivery fault did not hit the selected stream",
        case.codec,
        case.mode
    );
    selected
}

pub(super) fn target_for(case: &Case, spatial: u8, temporal: u8) -> u8 {
    let mut cursor = Cursor::new(case.wire, case.index).unwrap();
    let packet = std::iter::from_fn(|| cursor.next_due(Duration::from_secs(2)))
        .find(|packet| packet.kind == PacketKind::Rtp)
        .unwrap();
    let (_, dd) = captured_payload(packet.bytes);
    let first = DependencyDescriptorReader::new().read(dd).unwrap();
    let structure = first.attached_structure.unwrap();
    let mut matches = (0..structure.decode_target_count).filter(|&id| {
        let layers = structure
            .templates
            .iter()
            .filter(|template| template.dtis[usize::from(id)] != NotPresent);
        layers.clone().map(|layer| layer.spatial_id).max() == Some(spatial)
            && layers.map(|layer| layer.temporal_id).max() == Some(temporal)
    });
    let target = matches.next().unwrap_or_else(|| {
        panic!(
            "{} {} target {spatial}/{temporal} missing",
            case.codec, case.mode
        )
    });
    assert!(
        matches.next().is_none(),
        "{} {} target {spatial}/{temporal} ambiguous",
        case.codec,
        case.mode
    );
    target
}

#[test]
fn fixed_targets_match_independent_picture_lineage() {
    use std::collections::BTreeSet;

    for case in CASES.iter().filter(|case| case.encoded.is_empty()) {
        let observed: Value = serde_json::from_str(case.observations).unwrap();
        let mut ancestors = Vec::new();
        let rows: Vec<(u64, u8)> = case
            .encoded_index
            .lines()
            .skip(1)
            .map(|row| {
                let mut columns = row.split('\t').skip(1);
                (
                    columns.next().unwrap().parse().unwrap(),
                    columns.next().unwrap().parse().unwrap(),
                )
            })
            .collect();
        for row in observed["encoder_dependencies"].as_array().unwrap() {
            ancestors.push(transitive_ancestors(
                &ancestors,
                row.as_array()
                    .unwrap()
                    .iter()
                    .map(|prior| prior.as_u64().unwrap() as usize),
            ));
        }
        for witness in observed["fixed_targets"].as_array().unwrap() {
            let spatial = witness["spatial"].as_u64().unwrap_or(0) as u8;
            let temporal = witness["temporal"].as_u64().unwrap() as u8;
            let target = target_for(case, spatial, temporal);
            let pictures: BTreeSet<_> = witness["pictures"]
                .as_array()
                .unwrap()
                .iter()
                .map(|picture| picture["pts"].as_u64().unwrap())
                .collect();
            let mut expected = BTreeSet::new();
            for (i, &(pts, layer)) in rows.iter().enumerate() {
                if layer == spatial && pictures.contains(&pts) {
                    expected.insert(i);
                    expected.extend(ancestors[i].iter().copied());
                }
            }
            let expected: Vec<_> = expected.into_iter().collect();
            assert!(expected.len() >= pictures.len() && !pictures.is_empty());
            let actual = replay(case, &observed, target, None, Fault::None);
            assert_eq!(
                actual, expected,
                "{} {} target {spatial}/{temporal}",
                case.codec, case.mode
            );
        }
    }
}

pub(super) const FAMILY_CASES: [(&str, &str); 4] = [
    ("AV1", "L2T1"),
    ("H264", "L1T2"),
    ("VP8", "L1T2"),
    ("VP9", "L2T2"),
];

pub(super) struct FamilyTrace {
    pub base: u8,
    pub target: u8,
    pub restart_packet: usize,
    pub lost_packet: usize,
    pub reference: Value,
    pub first: usize,
}

pub(super) fn family_trace(case: &Case, observed: &Value) -> FamilyTrace {
    let reference: Value = serde_json::from_str(case.faults).unwrap();
    let spatial = reference["target"][0].as_u64().unwrap() as u8;
    let temporal = reference["target"][1].as_u64().unwrap() as u8;
    let first = case
        .encoded_index
        .lines()
        .skip(1)
        .position(|row| {
            row.split('\t').nth(1).unwrap().parse::<u64>().unwrap()
                == reference["restart_rtp"].as_u64().unwrap()
        })
        .unwrap();
    FamilyTrace {
        base: target_for(case, 0, 0),
        target: target_for(case, spatial, temporal),
        restart_packet: observed["wire_units"][first]["packets"][0]
            .as_u64()
            .unwrap() as usize,
        lost_packet: reference["loss"]["lost_packet"].as_u64().unwrap() as usize,
        reference,
        first,
    }
}

#[test]
fn directed_family_switch_loss_and_freshness() {
    for (codec, mode) in FAMILY_CASES {
        let case = CASES
            .iter()
            .find(|case| (case.codec, case.mode) == (codec, mode))
            .unwrap();
        let observed: Value = serde_json::from_str(case.observations).unwrap();
        let FamilyTrace {
            base,
            target,
            restart_packet,
            lost_packet,
            reference,
            first,
        } = family_trace(case, &observed);
        let switch: Vec<usize> =
            serde_json::from_value(reference["switch"]["units"].clone()).unwrap();
        assert_eq!(
            replay(
                case,
                &observed,
                base,
                Some((restart_packet, target)),
                Fault::None
            ),
            switch,
            "{codec} {mode} restart switch"
        );

        let possible: Vec<usize> =
            serde_json::from_value(reference["loss"]["units"].clone()).unwrap();
        let delivered = replay(case, &observed, target, None, Fault::Lost(lost_packet));
        assert!(
            !delivered.is_empty(),
            "{codec} {mode} should deliver before loss"
        );
        assert!(
            delivered
                .iter()
                .all(|unit| possible.binary_search(unit).is_ok()),
            "{codec} {mode} delivered a unit with a lost ancestor: {delivered:?}"
        );
        assert_eq!(
            delivered
                .into_iter()
                .filter(|&unit| unit >= first)
                .collect::<Vec<_>>(),
            possible
                .into_iter()
                .filter(|&unit| unit >= first)
                .collect::<Vec<_>>(),
            "{codec} {mode} did not resume at the intact restart"
        );
    }
}

#[test]
fn pilot_filtered_switch_and_lower_reference_loss() {
    for case in pilots() {
        let observed: Value = serde_json::from_str(case.observations).unwrap();
        let units = observed["wire_units"].as_array().unwrap();
        let switch = &observed["restart_switch"];
        let expected: Vec<usize> = serde_json::from_value(switch["units"].clone()).unwrap();
        let restart_timestamp = switch["input_rtp"].as_u64().unwrap();
        let restart = case
            .encoded_index
            .lines()
            .skip(1)
            .position(|row| {
                row.split('\t').nth(1).unwrap().parse::<u64>().unwrap() == restart_timestamp
            })
            .unwrap();
        let restart_packet = units[restart]["packets"][0].as_u64().unwrap() as usize;
        let mut selected = replay(case, &observed, 0, Some((restart_packet, 1)), Fault::None);
        if case.mode == "S2T1" {
            // The old target can continue until the independent upper entry
            // arrives. The lower sibling at that instant is not an ancestor.
            assert_eq!(
                observed["encoder_dependencies"][restart + 1],
                serde_json::json!([])
            );
            if let Some(index) = selected.iter().position(|&unit| unit == restart) {
                assert_eq!(selected.get(index + 1), Some(&(restart + 1)));
                selected.remove(index);
            }
        }
        assert_eq!(selected, expected, "{} restart switch", case.mode);

        let dropped: Vec<usize> = serde_json::from_value(observed["lost_packets"].clone()).unwrap();
        let selected = replay(case, &observed, 1, None, Fault::Refused(&dropped));
        let mut want: Vec<_> = expected.into_iter().filter(|i| *i >= restart).collect();
        let intact_prefix = observed["lost_lower"]["frames"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|frame| frame["pts"].as_u64().unwrap() < restart_timestamp)
            .count();
        let independent_upper = (1..restart).step_by(2);
        let prefix: Vec<_> = independent_upper.take(intact_prefix).collect();
        assert_eq!(prefix.len(), intact_prefix);
        want.splice(..0, prefix);
        assert_eq!(
            selected, want,
            "{} upper target after losing lower reference",
            case.mode
        );
    }
}
