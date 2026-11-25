//! Captured RTP replay at the QUIC ordering and encoded-unit seams.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use pulsebeam_agent_quic::media::{
    H264, Packet as MediaPacket, PacketBudget, av1,
    descriptor::{Publisher, Reader},
    jitter::{Buffer, GapReason, Input, Limits, Output, Push},
    units::{Codec, Depacketizer, Dependency, Event, Framing, OrderedPacket, Unit},
    vp8, vp9,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use wire_capture::{Cursor, Packet, PacketKind};

#[path = "codec_matrix/filtered.rs"]
mod filtered;

#[cfg(not(test))]
#[path = "codec_matrix/export.rs"]
mod export;

#[cfg(not(test))]
fn main() {
    export::run();
}

const SUMS: &str = include_str!(concat!(env!("CODEC_FIXTURES"), "/vp9-pilot/SHA256SUMS"));

struct Case {
    codec: &'static str,
    mode: &'static str,
    wire: &'static [u8],
    index: &'static str,
    observations: &'static str,
    faults: &'static str,
    encoded: &'static [u8],
    encoded_index: &'static str,
}

impl Case {
    fn codec(&self) -> Codec {
        match self.codec {
            "AV1" => Codec::Av1,
            "H264" => Codec::H264,
            "VP8" => Codec::Vp8,
            "VP9" => Codec::Vp9,
            unknown => panic!("unexpected codec {unknown}"),
        }
    }

    fn assert_bytes(&self, observed: &Value, index: usize, encoded_row: &str, data: &[u8]) {
        let witness = &observed["wire_units"][index];
        let label = format!("{} {} unit {index}", self.codec, self.mode);
        match self.codec() {
            Codec::Vp8 | Codec::H264 => assert_eq!(
                format!("{:x}", Sha256::digest(data)),
                witness["unit_sha256"].as_str().unwrap(),
                "{label}"
            ),
            Codec::Vp9 if !self.encoded.is_empty() => {
                let fields: Vec<_> = encoded_row.split('\t').collect();
                let offset = fields[10].parse::<usize>().unwrap();
                let size = fields[11].parse::<usize>().unwrap();
                assert!(data == &self.encoded[offset..offset + size], "{label}");
            }
            Codec::Vp9 => assert_eq!(
                format!("{:x}", Sha256::digest(data)),
                observed["pre_packetization_sha256"][index]
                    .as_str()
                    .unwrap(),
                "{label}"
            ),
            Codec::Av1 => assert_eq!(
                serde_json::to_value(av1_wire_obu_hashes(data)).unwrap(),
                witness["obus_sha256"],
                "{label}"
            ),
            Codec::Opus => unreachable!(),
        }
    }
}

const CASES: &[Case] = include!(env!("CODEC_CASES"));

fn pilots() -> impl Iterator<Item = &'static Case> {
    CASES.iter().filter(|case| !case.encoded.is_empty())
}

#[test]
fn exact_video_scope() {
    let temporal = ["L1T1", "L1T2", "L1T3"];
    let multi = [
        "L2T1",
        "L2T1h",
        "L2T2",
        "L2T2h",
        "L2T3",
        "L2T3h",
        "L3T1",
        "L3T1h",
        "L3T2",
        "L3T2h",
        "L3T3",
        "L3T3h",
        "S2T1",
        "S2T1h",
        "S2T2",
        "S2T2h",
        "S2T3",
        "S2T3h",
        "L2T2_KEY",
        "L2T2_KEY_SHIFT",
        "L2T3_KEY",
        "L3T1_KEY",
        "L3T2_KEY",
        "L3T3_KEY",
    ];
    let independent_three = ["S3T1", "S3T1h", "S3T2", "S3T2h", "S3T3", "S3T3h"];
    let mut expected = BTreeSet::new();
    for codec in ["H264", "VP8", "VP9", "AV1"] {
        expected.extend(temporal.map(|mode| (codec, mode)));
        if matches!(codec, "VP9" | "AV1") {
            expected.extend(multi.map(|mode| (codec, mode)));
        }
        if codec == "VP9" {
            expected.extend(independent_three.map(|mode| (codec, mode)));
        }
    }
    let actual: BTreeSet<_> = CASES.iter().map(|case| (case.codec, case.mode)).collect();
    assert_eq!(CASES.len(), 66);
    assert_eq!(actual, expected);
    assert_eq!(
        pilots().map(|case| case.mode).collect::<Vec<_>>(),
        ["L2T1", "S2T1"]
    );
}

fn captured_payload(bytes: &[u8]) -> (&[u8], &[u8]) {
    let packet = wire_capture::descriptor_rtp(bytes).expect("captured RTP with DD");
    (packet.payload, packet.descriptor)
}

fn av1_wire_obu_hashes(mut data: &[u8]) -> Vec<String> {
    let mut hashes = Vec::new();
    while let Some((&header, bytes)) = data.split_first() {
        assert_ne!(header & 2, 0, "canonical OBU is sized");
        let (extension, sized) = bytes.split_at(usize::from(header & 4 != 0));
        let (last, len) = sized
            .iter()
            .copied()
            .take(5)
            .enumerate()
            .scan(0_usize, |len, (i, byte)| {
                *len |= usize::from(byte & 0x7f) << (i * 7);
                Some((byte & 0x80 == 0, i + 1, *len))
            })
            .find_map(|(done, width, len)| done.then_some((width, len)))
            .unwrap();
        let (body, rest) = sized[last..].split_at(len);
        let mut hash = Sha256::new();
        hash.update([header & !2]);
        hash.update(extension);
        hash.update(body);
        hashes.push(format!("{:x}", hash.finalize()));
        data = rest;
    }
    hashes
}

fn repacketize(unit: &Unit<u8>, sequence: u32, timestamp: u32) -> Vec<MediaPacket<Vec<u8>>> {
    let budget = PacketBudget::new(620, 48).unwrap();
    match &unit.framing {
        Framing::Whole if unit.codec == Codec::H264 => H264::new(budget)
            .unwrap()
            .packetize(sequence, timestamp, unit.data.clone())
            .unwrap()
            .collect(),
        Framing::Vp8 { partitions, .. } => {
            let payloads: Vec<_> = partitions
                .iter()
                .map(|range| &unit.data[range.clone()])
                .collect();
            vp8::packetize(budget, sequence, timestamp, &payloads)
                .unwrap()
                .collect()
        }
        Framing::Vp9 {
            descriptor,
            end_of_picture,
        } => vp9::packetize(
            budget,
            sequence,
            timestamp,
            *end_of_picture,
            descriptor,
            &unit.data,
        )
        .unwrap()
        .collect(),
        Framing::Av1 {
            new_sequence,
            end_of_picture,
            ..
        } => av1::packetize(
            budget,
            sequence,
            timestamp,
            *new_sequence,
            *end_of_picture,
            &unit.data,
        )
        .unwrap()
        .collect(),
        framing => panic!("not a video frame: {framing:?}"),
    }
}

fn transitive_ancestors(
    prior: &[BTreeSet<usize>],
    direct: impl IntoIterator<Item = usize>,
) -> BTreeSet<usize> {
    let mut ancestors = BTreeSet::new();
    for index in direct {
        ancestors.insert(index);
        ancestors.extend(prior[index].iter().copied());
    }
    ancestors
}

#[test]
fn raw_and_republished_units_match_independent_observations() {
    let at = Instant::now();
    for case in CASES {
        let codec = case.codec();
        let label = format!("{} {}", case.codec, case.mode);
        let observed: Value = serde_json::from_str(case.observations).unwrap();
        let expected = observed["wire_units"].as_array().unwrap();
        let mut cursor = Cursor::new(case.wire, case.index).unwrap();
        let mut decoder = Depacketizer::new(0, codec);
        let mut republished = Depacketizer::new(1, codec);
        let mut publisher = Publisher::new(u16::MAX - 2);
        let mut outgoing = u64::from(u32::MAX) - 1;
        let mut encoded_rows = case.encoded_index.lines().skip(1);
        let mut by_frame = BTreeMap::new();
        let mut ancestors = Vec::new();
        let mut position = None::<u64>;
        let mut units = 0;
        while let Some(packet) = cursor.next_due(Duration::from_secs(2)) {
            if packet.kind != PacketKind::Rtp {
                continue;
            }
            let bytes = packet.bytes;
            let packet_time = packet.send_after;
            let raw = u16::from_be_bytes([bytes[2], bytes[3]]);
            let sequence = position.map_or(u64::from(raw), |old| {
                old + u64::from(raw.wrapping_sub(old as u16))
            });
            position = Some(sequence);
            let timestamp = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
            let (payload, dd) = captured_payload(bytes);
            let next = decoder
                .push(
                    0,
                    OrderedPacket {
                        sequence,
                        timestamp,
                        rtp_time: None,
                        arrived_at: at + packet_time,
                        marker: bytes[1] & 128 != 0,
                        payload,
                        descriptor: Some(dd),
                    },
                )
                .unwrap_or_else(|err| panic!("{label} packet {}: {err:?}", packet.index));
            let Some(event) = next else { continue };
            let Event::Unit(unit) = event else {
                panic!("{label} unit {units}: {event:?}")
            };
            let witness = &expected[units];
            let row = encoded_rows.next().unwrap();
            let fields: Vec<_> = row.split('\t').collect();
            assert_eq!(
                witness["rtp_timestamp"].as_u64(),
                Some(u64::from(timestamp)),
                "{label} unit {units}"
            );
            let packets = witness["packets"].as_array().unwrap();
            assert_eq!(
                packets.last().unwrap().as_u64(),
                Some(packet.index as u64),
                "{label} unit {units}"
            );
            assert_eq!(
                unit.last_sequence - unit.first_sequence + 1,
                packets.len() as u64,
                "{label} unit {units}"
            );
            let Dependency::Parsed(facts) = &unit.dependency else {
                panic!("{label} unit {units}: {:?}", unit.dependency)
            };
            assert_eq!(
                witness["dd"].as_u64(),
                Some(u64::from(facts.frame_number)),
                "{label} unit {units}"
            );
            assert_eq!(
                facts.spatial_id(),
                fields[2].parse::<u8>().unwrap(),
                "{label} unit {units}"
            );
            assert_eq!(
                facts.temporal_id(),
                fields[3].parse::<u8>().unwrap(),
                "{label} unit {units}"
            );
            // A sender can omit an already implied reference from the DD.
            let encoded_ancestors = transitive_ancestors(
                &ancestors,
                observed["encoder_dependencies"][units]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|prior| prior.as_u64().unwrap() as usize),
            );
            let wire_ancestors = transitive_ancestors(
                &ancestors,
                facts
                    .frame_dependencies
                    .frame_diffs
                    .iter()
                    .map(|diff| by_frame[&facts.frame_number.wrapping_sub(*diff)]),
            );
            assert_eq!(wire_ancestors, encoded_ancestors, "{label} unit {units}");
            ancestors.push(encoded_ancestors);
            assert!(by_frame.insert(facts.frame_number, units).is_none());
            case.assert_bytes(&observed, units, row, &unit.data);
            let mut authored = facts.as_ref().clone();
            authored.frame_number = publisher.next_frame();
            for packet in repacketize(&unit, outgoing as u32, timestamp) {
                assert_eq!(packet.sequence, outgoing as u32, "{label} unit {units}");
                let accepted =
                    publisher.write_if(&packet, packet.first.then_some(&authored), |ext| {
                        let result = republished
                            .push(
                                1,
                                OrderedPacket {
                                    sequence: outgoing,
                                    timestamp,
                                    rtp_time: Some(u64::from(timestamp)),
                                    arrived_at: at + packet_time,
                                    marker: packet.marker,
                                    payload: &packet.payload,
                                    descriptor: Some(ext.value),
                                },
                            )
                            .unwrap_or_else(|err| panic!("{label} republish {units}: {err:?}"));
                        if packet.last {
                            let Some(Event::Unit(output)) = result else {
                                panic!("{label} republish {units}: {result:?}");
                            };
                            assert!(
                                output.data == unit.data,
                                "{label} republish unit {units}: bytes differ"
                            );
                            assert_eq!(
                                output.rtp_time,
                                Some(u64::from(timestamp)),
                                "{label} republish {units}"
                            );
                            let Dependency::Parsed(dd) = output.dependency else {
                                panic!("{label} republish {units}: {:?}", output.dependency);
                            };
                            assert_eq!(
                                dd.frame_number, authored.frame_number,
                                "{label} republish {units}"
                            );
                            assert_eq!(
                                dd.frame_dependencies, facts.frame_dependencies,
                                "{label} republish {units}"
                            );
                        } else {
                            assert!(
                                result.is_none(),
                                "{label} republish {units}: unfinished unit"
                            );
                        }
                        true
                    });
                assert_eq!(accepted, Ok(true), "{label} republish unit {units}");
                outgoing += 1;
            }
            units += 1;
        }
        assert_eq!(units, expected.len(), "{label}");
    }
}

#[test]
fn producer_repacketizes_encoder_authored_spatial_references() {
    let at = Instant::now();
    for pilot in pilots() {
        assert_digest(pilot.mode, "encoded.bin", pilot.encoded);
        assert_digest(pilot.mode, "encoded.tsv", pilot.encoded_index.as_bytes());
        let observed: Value = serde_json::from_str(pilot.observations).unwrap();
        let units = observed["wire_units"].as_array().unwrap();
        let dependencies = observed["encoder_dependencies"].as_array().unwrap();
        let mut capture = Cursor::new(pilot.wire, pilot.index).unwrap();
        let mut input_dd = Reader::new();
        let mut output_dd = Reader::new();
        let mut publisher =
            Publisher::new(u16::try_from(units[0]["dd"].as_u64().unwrap()).unwrap());
        let mut receiver = Depacketizer::new(1, Codec::Vp9);
        let mut sequence = u64::from(u32::MAX) - 1;
        let mut completed = 0;
        let mut structures = 0;
        for (i, row) in pilot.encoded_index.lines().skip(1).enumerate() {
            let fields: Vec<_> = row.split('\t').collect();
            assert_eq!(fields[0].parse::<usize>().unwrap(), i);
            let offset = fields[10].parse::<usize>().unwrap();
            let size = fields[11].parse::<usize>().unwrap();
            let encoded = &pilot.encoded[offset..offset + size];
            let first = usize::try_from(units[i]["packets"][0].as_u64().unwrap()).unwrap();
            let original = loop {
                let original = capture.next_due(Duration::from_secs(2)).unwrap();
                if original.index == first {
                    break original;
                }
            };
            let (payload, dd) = captured_payload(original.bytes);
            let facts = input_dd.read(dd).unwrap();
            assert!(facts.start_of_frame);
            assert_eq!(facts.spatial_id(), fields[2].parse::<u8>().unwrap());
            assert_eq!(facts.temporal_id(), fields[3].parse::<u8>().unwrap());
            let mut expected_diffs: Vec<_> = dependencies[i]
                .as_array()
                .unwrap()
                .iter()
                .map(|prior| {
                    u16::try_from(i - usize::try_from(prior.as_u64().unwrap()).unwrap()).unwrap()
                })
                .collect();
            let mut captured_diffs = facts.frame_dependencies.frame_diffs.to_vec();
            expected_diffs.sort_unstable();
            captured_diffs.sort_unstable();
            assert_eq!(captured_diffs, expected_diffs, "{} unit {i}", pilot.mode);
            structures += usize::from(facts.attached_structure.is_some());
            let (descriptor, _) = vp9::Descriptor::parse(payload).unwrap();
            let timestamp = u32::try_from(units[i]["rtp_timestamp"].as_u64().unwrap()).unwrap();
            let budget = PacketBudget::new(512, 32).unwrap();
            let packets = vp9::packetize(
                budget,
                sequence as u32,
                timestamp,
                fields[8] == "1",
                &descriptor,
                encoded,
            )
            .unwrap();
            for packet in packets {
                assert_eq!(packet.sequence, sequence as u32);
                assert!(
                    publisher
                        .write_if(&packet, packet.first.then_some(&facts), |ext| {
                            let parsed = output_dd.read(ext.value).unwrap();
                            assert_eq!(parsed.start_of_frame, packet.first);
                            assert_eq!(parsed.end_of_frame, packet.last);
                            assert_eq!(parsed.frame_number, facts.frame_number);
                            assert_eq!(parsed.frame_dependencies, facts.frame_dependencies);
                            assert_eq!(
                                parsed.attached_structure.as_deref(),
                                packet
                                    .first
                                    .then_some(facts.attached_structure.as_deref())
                                    .flatten()
                            );
                            let result = receiver
                                .push(
                                    1,
                                    OrderedPacket {
                                        sequence,
                                        timestamp,
                                        rtp_time: None,
                                        arrived_at: at,
                                        marker: packet.marker,
                                        payload: &packet.payload,
                                        descriptor: Some(ext.value),
                                    },
                                )
                                .unwrap();
                            if packet.last {
                                let Some(Event::Unit(unit)) = result else {
                                    panic!("{} unit {i}: {result:?}", pilot.mode)
                                };
                                assert_eq!(unit.data, encoded);
                                completed += 1;
                            } else {
                                assert!(result.is_none());
                            }
                            true
                        })
                        .unwrap()
                );
                sequence += 1;
            }
        }
        assert_eq!(completed, units.len());
        assert!(
            structures > 1,
            "{} must include a restarted structure",
            pilot.mode
        );
    }
}

type Jitter = Buffer<u64, Packet<'static>>;

fn drain(buffer: &mut Jitter, now: Instant, delivered: &mut Vec<Packet<'static>>) -> usize {
    let mut lost = 0;
    while let Some(output) = buffer.poll(now).expect("monotonic receiver clock") {
        match output {
            Output::Packet { input, .. } => delivered.push(input.packet),
            Output::Gap { sequence, reason } => {
                assert_eq!(reason, GapReason::Deadline);
                lost += usize::try_from(sequence.end - sequence.start).expect("bounded gap");
            }
        }
    }
    lost
}

fn replay(
    cursor: &mut Cursor<'static>,
    buffer: &mut Jitter,
    start: Instant,
    excluded: &[usize],
    reorder: Option<usize>,
) -> (Vec<Packet<'static>>, usize) {
    let mut delivered = Vec::new();
    let mut held = None;
    let mut reordered = false;
    let mut observed_loss = 0;
    let mut previous = None::<u32>;
    // Step through recorded time, including idle time for gap expiration.
    for millis in 0..=1000 {
        let elapsed = Duration::from_millis(millis);
        let now = start + elapsed;
        while let Some(packet) = cursor.next_due(elapsed) {
            if packet.kind != PacketKind::Rtp || excluded.contains(&packet.index) {
                continue;
            }
            let [_, _, a, b, c, d, e, f, ..] = packet.bytes else {
                panic!("incomplete RTP fixed header");
            };
            let raw = u16::from_be_bytes([*a, *b]);
            // The QUIC jitter API consumes canonical 32-bit sequence numbers.
            let sequence = previous.map_or(u32::from(raw), |anchor| {
                anchor + u32::from(raw.wrapping_sub(anchor as u16))
            });
            previous = Some(sequence);
            let input = || Input {
                source: packet.epoch,
                sequence,
                timestamp: u32::from_be_bytes([*c, *d, *e, *f]),
                arrived_at: now,
                retained_bytes: NonZeroUsize::new(packet.bytes.len()).expect("nonempty RTP"),
                packet,
            };
            if reorder == Some(packet.index) {
                held = Some(input());
                continue;
            }
            assert!(matches!(buffer.push(now, input()), Push::Queued));
            if let Some(earlier) = held.take() {
                assert!(matches!(
                    buffer.push(now, input()),
                    Push::Rejected {
                        reason: pulsebeam_agent_quic::media::jitter::Rejected::Duplicate,
                        ..
                    }
                ));
                assert!(matches!(buffer.push(now, earlier), Push::Queued));
                reordered = true;
            }
        }
        observed_loss += drain(buffer, now, &mut delivered);
    }
    assert_eq!(reordered, reorder.is_some());
    (delivered, observed_loss)
}

fn assert_digest(mode: &str, name: &str, data: &[u8]) {
    let expected_name = format!("{mode}/{name}");
    let expected = SUMS.lines().find_map(|line| {
        let (digest, name) = line.split_once("  ")?;
        (name == expected_name).then_some(digest)
    });
    assert_eq!(
        Some(format!("{:x}", Sha256::digest(data)).as_str()),
        expected
    );
}

fn membership(delivered: &[Packet<'_>], frames: &[Vec<usize>]) -> Vec<usize> {
    let present: BTreeSet<_> = delivered.iter().map(|packet| packet.index).collect();
    frames
        .iter()
        .enumerate()
        .filter_map(|(unit, packets)| {
            packets
                .iter()
                .all(|index| present.contains(index))
                .then_some(unit)
        })
        .collect()
}

#[test]
fn pilot_wire_order_loss_and_resume_are_observable() {
    for case in pilots() {
        assert_digest(case.mode, "wire.bin", case.wire);
        assert_digest(case.mode, "wire.tsv", case.index.as_bytes());
        assert_digest(case.mode, "observations.json", case.observations.as_bytes());
        let observed: Value =
            serde_json::from_str(case.observations).expect("authoring observations");
        let frames: Vec<Vec<usize>> = observed["wire_units"]
            .as_array()
            .expect("wire frames")
            .iter()
            .map(|row| {
                serde_json::from_value(row["packets"].clone()).expect("frame packet indexes")
            })
            .collect();
        let expected: Vec<_> = frames.iter().flatten().copied().collect();
        let mut cursor = Cursor::new(case.wire, case.index).expect("pilot cursor");
        let mut buffer = Jitter::new(0, Limits::default());
        let start = Instant::now();
        let (complete, loss) = replay(&mut cursor, &mut buffer, start, &[], Some(frames[0][1]));
        assert_eq!(loss, 0);
        assert_eq!(
            complete.iter().map(|x| x.index).collect::<Vec<_>>(),
            expected
        );
        assert!(complete.iter().all(|packet| packet.epoch == 0));

        // Drop the second lower frame after the stream has started. The
        // same-time upper sibling remains separately observable even in L2T1:
        // jitter orders packets; it does not make codec dependency decisions.
        let lost_unit = 2;
        let excluded = &frames[lost_unit];
        let epoch = cursor.rewind().expect("next capture lifetime");
        buffer.replace_source(epoch);
        let (with_loss, gaps) = replay(
            &mut cursor,
            &mut buffer,
            start + Duration::from_secs(2),
            excluded,
            None,
        );
        assert_eq!(gaps, excluded.len());
        assert!(with_loss.iter().all(|packet| packet.epoch == epoch));
        assert_eq!(
            with_loss.iter().map(|x| x.index).collect::<Vec<_>>(),
            expected
                .into_iter()
                .filter(|idx| !excluded.contains(idx))
                .collect::<Vec<_>>()
        );
        let wanted: Vec<_> = (0..frames.len()).filter(|&i| i != lost_unit).collect();
        assert_eq!(membership(&with_loss, &frames), wanted);
        assert!(wanted.contains(&(lost_unit + 1)));
        assert!(
            wanted.contains(&16),
            "later real encoder restart is observable"
        );

        let mut includes_wrong_frame = with_loss.clone();
        includes_wrong_frame.extend(
            complete
                .iter()
                .copied()
                .filter(|x| excluded.contains(&x.index)),
        );
        assert_ne!(membership(&includes_wrong_frame, &frames), wanted);
        let missing_resume: Vec<_> = with_loss
            .into_iter()
            .filter(|x| !frames[16].contains(&x.index))
            .collect();
        assert_ne!(membership(&missing_resume, &frames), wanted);
    }
}
