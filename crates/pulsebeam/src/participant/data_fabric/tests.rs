#![allow(
    clippy::unwrap_used,
    reason = "literal fixture and setup failure is a test failure"
)]

use super::*;
use data_record::{Capture, FRAGMENT_BYTES, MAX_RECORD_BYTES};

struct Fixture {
    ledger: Source,
    fanout: SourceFanout,
}

impl Fixture {
    fn new(clock: Option<Clock<'_>>) -> Self {
        Self {
            ledger: Source::new(clock),
            fanout: SourceFanout::new(
                crate::route::fabric::local::Emission::new(
                    Profile::Data,
                    &[ExtensionKind::DependencyDescriptor],
                )
                .unwrap(),
            ),
        }
    }

    fn admit(&mut self, bytes: &[u8], now: Instant, stamp: u32) -> Result<Admission<'_>, Error> {
        self.ledger
            .admit_record(&mut self.fanout, bytes, now, stamp, true)
    }

    fn admit_sctp(
        &mut self,
        bytes: &[u8],
        now: Instant,
        stamp: u32,
    ) -> Result<Admission<'_>, Error> {
        self.ledger
            .admit_record(&mut self.fanout, bytes, now, stamp, false)
    }

    fn original(&self, unit_seq: u64, index: u16, now: Instant) -> Original<'_> {
        self.ledger.original(&self.fanout, unit_seq, index, now)
    }

    fn request_original(
        &mut self,
        binding: PublisherBinding,
        unit_seq: u64,
        index: u16,
        now: Instant,
    ) -> OriginalResolution {
        self.ledger
            .request_original(&self.fanout, binding, unit_seq, index, now)
    }

    fn original_unavailable(&mut self, demand: OriginalDemand, now: Instant) -> bool {
        self.ledger.original_unavailable(&self.fanout, demand, now)
    }
}

const CLOCK: Clock<'static> = Clock {
    id: "mic",
    epoch: 1,
};
pub(crate) const R4: [u8; 23] = [
    0x50, 0x44, 1, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 1, 0, 2, 0, 1, 0, 0, 4, 1, b'b',
];
const R3_HEADER: [u8; 22] = [
    0x50, 0x44, 1, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2, 4, 0, 0, 0, 4, 1,
];
const IMAGE_0: [u8; 24] = [
    0x90, 0xe3, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0xbe, 0xde, 0, 2, 0x16, 1, 2, 3, 4, 0, 0, 0xe0,
];
const IMAGE_1: [u8; 24] = [
    0x90, 0x63, 0, 1, 0, 0, 0, 3, 0, 0, 0, 0, 0xbe, 0xde, 0, 2, 0x16, 1, 2, 3, 4, 0, 0, 0xe0,
];

pub(crate) fn r3_record() -> Vec<u8> {
    [R3_HEADER.as_slice(), &[b'a'; 1_024]].concat()
}

fn fixture(unit_seq: u64, index: u16, count: u16, len: u32, bytes: &[u8]) -> Vec<u8> {
    let record = Record::new(
        data_record::Header {
            unit_seq,
            index,
            count,
            unit_len: len,
            capture: None,
        },
        bytes,
        None,
    )
    .unwrap();
    let mut out = vec![0; record.encoded_len()];
    record.write(&mut out).unwrap();
    out
}

#[test]
fn optional_descriptor_round_trips_through_bound_wan_live_and_repair() {
    use crate::route::fabric::local::RepairFragment;
    use pulsebeam_core::fabric::wan::Window;

    const BARE: [u8; 22] = [
        0x50, 0x44, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0,
    ];
    const CLIENT: [u8; 26] = [
        0x50, 0x44, 1, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, 0xc0, 0, 0,
    ];
    const RTP: [u8; 42] = [
        0x90, 0xe6, 0, 0, 0, 0, 0, 0, 5, 6, 7, 10, 0xbe, 0xde, 0, 1, 0x22, 0xc0, 0, 0, 0x50, 0x44,
        1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0,
    ];
    let now = Instant::now();
    let mut unqualified = Fixture::new(None);
    assert_eq!(
        unqualified.admit_sctp(&CLIENT, now, 0),
        Err(Error::Record(data_record::Error::Flags))
    );
    assert_eq!(unqualified.admit_sctp(&BARE, now, 0).unwrap().sequence, 0);
    let mut source = Fixture::new(None);
    let mut invalid = CLIENT;
    invalid[22] = 2;
    assert_eq!(
        source.admit(&invalid, now, 0),
        Err(Error::Record(data_record::Error::DescriptorLength))
    );
    let first = source.admit(&CLIENT, now, 42).unwrap();
    assert_eq!(first.sequence, 0);
    assert_eq!(first.packet.payload(), BARE);
    assert_eq!(
        first.packet.legacy_wire_len(),
        Err(WireError::ExtensionProfile)
    );
    assert_eq!(project(first.packet, None), Err(Error::Projection));
    let mut tuple = [3, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 1, 5, 6, 7, 10];
    for (ssrc_end, repair) in [
        (10, None),
        (
            11,
            Some(RepairFragment {
                unit_seq: 0,
                index: 0,
            }),
        ),
    ] {
        tuple[16] = ssrc_end;
        let window = Window::from_tuple(tuple, 1 << 2).unwrap();
        let mut expected = RTP;
        expected[11] = ssrc_end;
        let mut wire = [0; 42];
        assert_eq!(window.write(0, first.packet, &mut wire), Ok(wire.len()));
        assert_eq!(wire, expected);
        let (position, received) = window.decode(&wire).unwrap();
        assert_eq!(position, 0);
        assert!(received.same_content(&first.packet.arrival_view(6).unwrap()));
        let mut reconstructed = [0; 26];
        assert_eq!(
            project_sctp_record(&received, None, repair),
            Err(Error::Projection)
        );
        let record = project_record(&received, None, repair).unwrap();
        assert_eq!(record.write(&mut reconstructed), Ok(CLIENT.len()));
        assert_eq!(reconstructed, CLIENT);
    }
    let incorrectly_embedded = FabricPacket::new(first.packet.header(), &[], CLIENT).unwrap();
    assert_eq!(
        project_record(&incorrectly_embedded, None, None),
        Err(Error::Record(data_record::Error::Flags))
    );
    assert!(!source.admit(&CLIENT, now, 0).unwrap().new_fragment);
    let mut conflict = CLIENT;
    conflict[25] = 1;
    assert_eq!(source.admit(&conflict, now, 0), Err(Error::SourceConflict));
    assert_eq!(source.admit(&CLIENT, now, 0), Err(Error::Fenced));
}

pub(crate) fn maximum_dd_bytes(
    fragment: crate::route::fabric::local::RepairFragment,
) -> [u8; data_record::MAX_ENDPOINT_RECORD_BYTES] {
    let count = fragment.index + 1;
    let dd = [0x80; data_record::MAX_DESCRIPTOR_BYTES];
    let record = Record::new(
        data_record::Header {
            unit_seq: fragment.unit_seq,
            index: fragment.index,
            count,
            unit_len: u32::from(count) * u32::try_from(FRAGMENT_BYTES).unwrap(),
            capture: Some(Capture {
                clock: CLOCK,
                nanos: -50,
                uncertainty_ns: Some(0),
            }),
        },
        &[0x61; FRAGMENT_BYTES],
        Some(CLOCK),
    )
    .unwrap()
    .with_dependency_descriptor(&dd)
    .unwrap();
    let mut raw = [0; data_record::MAX_ENDPOINT_RECORD_BYTES];
    assert_eq!(record.write(&mut raw), Ok(1155));
    raw
}

pub(crate) fn admit_dd_fixture(bytes: &[u8], sequence: u64, now: Instant) -> OwnedPacket {
    let mut source = Fixture::new(Some(CLOCK));
    source.ledger.next_sequence = sequence;
    let admitted = source.admit(bytes, now, 0).unwrap();
    assert_eq!(admitted.sequence, sequence);
    assert_eq!(admitted.packet.payload().len(), MAX_RECORD_BYTES);
    admitted.packet.clone()
}

pub(crate) fn reconstruct_dd_fixture(
    packet: &FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
    repair: Option<crate::route::fabric::local::RepairFragment>,
) -> [u8; data_record::MAX_ENDPOINT_RECORD_BYTES] {
    let projected = project_record(packet, Some(CLOCK), repair).unwrap();
    let mut output = [0; data_record::MAX_ENDPOINT_RECORD_BYTES];
    assert_eq!(projected.write(&mut output), Ok(output.len()));
    output
}

#[test]
fn maximum_dd_record_keeps_its_budget_across_wan_relay_and_repair() {
    use crate::route::fabric::local::RepairFragment;
    use core::num::NonZeroU32;
    use pulsebeam_core::fabric::{MAX_WAN_RTP_BYTES, wan::Window};

    let fragment = RepairFragment {
        unit_seq: 17,
        index: 0,
    };
    let sequence = 0;
    let raw = maximum_dd_bytes(fragment);
    let packet = admit_dd_fixture(&raw, sequence, Instant::now());
    let window =
        |ssrc| Window::new(Profile::Data, 0, 2, NonZeroU32::new(ssrc).unwrap(), 1 << 2).unwrap();
    let upstream = window(7);
    let downstream = window(8);
    let repair = downstream
        .repair_window(sequence, NonZeroU32::new(9).unwrap())
        .unwrap();
    let mut first_wire = [0; MAX_WAN_RTP_BYTES];
    let first_len = upstream.write(sequence, &packet, &mut first_wire).unwrap();
    assert_eq!(first_len + 16, 1190);
    let (position, first_hop) = upstream.decode(&first_wire[..first_len]).unwrap();

    let mut live_wire = [0; MAX_WAN_RTP_BYTES];
    let mut repair_wire = [0; MAX_WAN_RTP_BYTES];
    assert_eq!(
        downstream.write(position, &first_hop, &mut live_wire),
        Ok(first_len)
    );
    assert_eq!(
        repair.write(position, &first_hop, &mut repair_wire),
        Ok(first_len)
    );
    let live = downstream.decode(&live_wire[..first_len]).unwrap();
    let repaired = repair.decode(&repair_wire[..first_len]).unwrap();
    assert_eq!(live, repaired);
    for ((received, packet), demand) in [(live, None), (repaired, Some(fragment))] {
        assert_eq!(received, sequence);
        assert_eq!(reconstruct_dd_fixture(&packet, demand), raw);
    }
}

#[test]
fn appendix_b_index_one_first_freezes_canonical_identity_and_projects_unchanged() {
    let now = Instant::now();
    let mut source = Fixture::new(None);
    assert!(matches!(
        source.admit(R4.get(..22).unwrap(), now, 0),
        Err(Error::Record(_))
    ));
    use pulsebeam_proto::data_control::{
        PublisherBinding, PublisherObservation, SourceUnit, publisher_observation,
    };
    let binding = PublisherBinding {
        source: core::num::NonZeroU64::new(12).unwrap(),
    };
    let ack = PublisherObservation {
        record: Some(publisher_observation::Record::Admission(SourceUnit {
            source_incarnation: 12,
            unit_seq: Some(3),
        })),
    };
    assert_eq!(source.original(3, 0, now), Original::Unavailable);
    let first = source.admit(&R4, now, 0x0102_0304).unwrap();
    assert!(first.new_fragment && first.new_unit);
    assert_eq!(
        first.first_unit(),
        Some(AdmittedUnit {
            unit_seq: 3,
            fragment_count: 2,
            expires: now + LIFETIME,
        })
    );
    assert_eq!(first.source_ack(binding), Some(ack.clone()));
    assert_eq!(first.sequence, 0);
    let frozen = first.packet.clone();
    assert_eq!(
        source.original(3, 0, now),
        Original::Unseen {
            expires: now + LIFETIME
        }
    );
    assert_eq!(
        source.original(3, 1, now),
        Original::Cached {
            sequence: 0,
            packet: &frozen,
            expires: now + LIFETIME
        }
    );
    assert_eq!(source.original(3, 2, now), Original::Unavailable);
    assert_eq!(source.original(3, 0, now + LIFETIME), Original::Unavailable);
    assert_eq!(source.original(3, 1, now + LIFETIME), Original::Unavailable);
    let retry = source
        .admit(&R4, now + Duration::from_secs(1), 0x1122_3344)
        .unwrap();
    assert!(!retry.new_fragment && !retry.new_unit);
    assert_eq!(retry.first_unit(), None);
    assert_eq!(retry.source_ack(binding), Some(ack.clone()));
    assert_eq!(retry.sequence, 0);
    assert_eq!(retry.packet, &frozen);
    let r3 = r3_record();
    let second = source
        .admit(&r3, now + Duration::from_secs(1), 0x0102_0304)
        .unwrap();
    assert!(second.new_fragment && !second.new_unit);
    assert_eq!(second.first_unit(), None);
    assert_eq!(second.source_ack(binding), Some(ack));
    assert_eq!(second.sequence, 1);
    for (packet, prefix, original) in [
        (&frozen, IMAGE_0, R4.as_slice()),
        (second.packet, IMAGE_1, r3.as_slice()),
    ] {
        let mut wire = [0; 1_152];
        let len = packet.write_local(&mut wire).unwrap();
        assert_eq!(wire.get(..24), Some(prefix.as_slice()));
        assert_eq!(wire.get(24..len), Some(original));
        assert_eq!(project(packet, None), Ok(original));
    }
    let invalid_marker = FabricPacket::new(
        Header {
            marker: false,
            ..frozen.header()
        },
        &[],
        frozen.payload(),
    )
    .unwrap();
    let invalid_label = FabricPacket::new(
        Header {
            timestamp: 4,
            ..frozen.header()
        },
        &[],
        frozen.payload(),
    )
    .unwrap();
    assert_eq!(project(&invalid_marker, None), Err(Error::Projection));
    assert_eq!(project(&invalid_label, None), Err(Error::Projection));
    assert!(
        matches!(source.original(3, 0, now + Duration::from_secs(1)), Original::Cached { sequence: 1, packet, expires } if packet.payload() == r3 && expires == now + LIFETIME)
    );
    assert_eq!(source.admit(&R4, now + LIFETIME, 0), Err(Error::Expired));
    assert_eq!(source.admit(&r3, now + LIFETIME, 0), Err(Error::Expired));
}

#[test]
fn original_retrieval_coalesces_and_live_admission_wins_over_a_stale_reply() {
    let now = Instant::now();
    let retry = now + ORIGINAL_RETRY;
    let mut source = Fixture::new(None);
    let endpoint = PublisherBinding {
        source: core::num::NonZeroU64::MIN,
    };
    source.admit(&R4, now, 0).unwrap();
    let OriginalResolution::Request(demand) = source.request_original(endpoint, 3, 0, now) else {
        panic!("missing original request");
    };
    assert_eq!(demand.unit.unwrap().unit_seq, Some(3));
    assert_eq!(demand.fragment_index, Some(0));
    assert_eq!(
        source.request_original(endpoint, 3, 0, now),
        OriginalResolution::Pending
    );
    assert_eq!(
        source.request_original(endpoint, 3, 0, retry),
        OriginalResolution::Request(demand)
    );
    assert!(!source.original_unavailable(
        OriginalDemand {
            request_id: 2,
            ..demand
        },
        retry
    ));
    assert!(source.original_unavailable(demand, retry));
    assert!(!source.original_unavailable(demand, retry));
    assert_eq!(
        source.request_original(endpoint, 3, 0, retry),
        OriginalResolution::Unavailable
    );

    let packet = r3_record();
    let live = source.admit(&packet, retry, 0).unwrap();
    assert!(!live.new_unit && live.new_fragment);
    assert_eq!(live.packet.payload(), packet.as_slice());
    assert!(!source.original_unavailable(demand, retry));
    assert_eq!(
        source.request_original(endpoint, 3, 0, retry),
        OriginalResolution::Cached {
            sequence: 1,
            expires: now + LIFETIME
        }
    );
    assert_eq!(
        source.request_original(endpoint, 3, 0, now + LIFETIME),
        OriginalResolution::Unavailable
    );
}

#[test]
fn original_only_positions_survive_live_window_without_becoming_live() {
    let now = Instant::now();
    let mut source = Fixture::new(None);
    let original = source.admit(&R4, now, 7).unwrap();
    let position = original.sequence;
    let bytes = original.packet.payload().as_ptr();
    let hops = original.packet.remaining_hops();
    assert!(source.fanout.live_sequence_anchor(now).is_none());
    let retry = source.admit(&R4, now, 9).unwrap();
    assert_eq!(retry.sequence, position);
    assert_eq!(retry.packet.header().ingress_ntp32, 7);
    assert_eq!(retry.packet.payload().as_ptr(), bytes);

    let fresh = fixture(4, 0, 1, 1, b"x");
    source.ledger.next_sequence = 4_097;
    let fresh = source.admit(&fresh, now, 10).unwrap().sequence;
    source.fanout.offer_retained_data(now, now, fresh).unwrap();
    assert_eq!(source.fanout.live_sequence_anchor(now), Some(fresh));
    assert!(source.fanout.transit(position, hops).is_none());
    assert_eq!(
        source
            .fanout
            .endpoint_data(position)
            .unwrap()
            .payload()
            .as_ptr(),
        bytes
    );
    assert_eq!(source.original(3, 1, now + LIFETIME), Original::Unavailable);
    source.ledger.expire(&mut source.fanout, now + LIFETIME);
    assert!(source.fanout.endpoint_data(position).is_none());
}

#[test]
fn changed_fragment_fences_source_but_mismatched_new_index_only_closes_unit() {
    let now = Instant::now();
    let mut source = Fixture::new(None);
    source.admit(&R4, now, 0).unwrap();
    let mismatched = fixture(3, 0, 1, 1, b"c");
    assert_eq!(source.admit(&mismatched, now, 0), Err(Error::UnitConflict));
    assert_eq!(source.fanout.endpoint_data_count(), 0);
    assert_eq!(source.admit(&R4, now, 0), Err(Error::Expired));
    let newest = fixture(4, 0, 1, 1, b"c");
    assert_eq!(source.admit(&newest, now, 0).unwrap().sequence, 1);
    let changed = fixture(4, 0, 1, 1, b"x");
    assert_eq!(source.admit(&changed, now, 0), Err(Error::SourceConflict));
    assert_eq!(source.original(4, 0, now), Original::Unavailable);
    assert_eq!(source.admit(&newest, now, 0), Err(Error::Fenced));
}

#[test]
fn window_and_caps_do_not_commit_rejected_units() {
    let now = Instant::now();
    let mut source = Fixture::new(None);
    for id in 0..64 {
        assert_eq!(
            source
                .admit(&fixture(id, 0, 1, 0, b""), now, 0)
                .unwrap()
                .sequence,
            id
        );
    }
    assert_eq!(
        source.admit(&fixture(511, 0, 1, 0, b""), now, 0),
        Err(Error::Capacity)
    );
    assert_eq!(source.ledger.max_seen, Some(63));
    let advance = fixture(512, 0, 1, 0, b"");
    assert_eq!(source.admit(&advance, now, 0).unwrap().sequence, 64);
    assert_eq!(source.fanout.endpoint_data_count(), 64);
    assert!(source.fanout.endpoint_data(0).is_none());
    assert_eq!(
        source.admit(&fixture(0, 0, 1, 0, b""), now, 0),
        Err(Error::Expired)
    );
    assert_eq!(
        source
            .admit(&fixture(1, 0, 1, 0, b""), now, 0)
            .unwrap()
            .sequence,
        1
    );
    let sparse = fixture(u64::MAX, 0, 1, 0, b"");
    assert_eq!(source.admit(&sparse, now, 0).unwrap().sequence, 65);
    assert_eq!(source.fanout.endpoint_data_count(), 1);
    assert!(source.fanout.endpoint_data(65).is_some());
    assert_eq!(
        source
            .admit(&fixture(u64::MAX - 511, 0, 1, 0, b""), now, 0)
            .unwrap()
            .sequence,
        66
    );
    assert_eq!(
        source.admit(&fixture(u64::MAX - 512, 0, 1, 0, b""), now, 0),
        Err(Error::Expired)
    );
}

#[test]
fn full_records_are_byte_bounded_and_capture_never_fabricates_act() {
    let now = Instant::now();
    let mut source = Fixture::new(Some(CLOCK));
    let mut raw = [0; MAX_RECORD_BYTES];
    let mut capped = false;
    'units: for unit_seq in 0..64 {
        for index in 0..64 {
            let record = Record::new(
                data_record::Header {
                    unit_seq,
                    index,
                    count: 64,
                    unit_len: 65_536,
                    capture: Some(Capture {
                        clock: CLOCK,
                        nanos: -50,
                        uncertainty_ns: Some(0),
                    }),
                },
                &[1; FRAGMENT_BYTES],
                Some(CLOCK),
            )
            .unwrap();
            assert_eq!(record.write(&mut raw), Ok(MAX_RECORD_BYTES));
            let sequence_before = source.ledger.next_sequence;
            match source.admit(&raw, now, 0) {
                Ok(admission) => {
                    assert_eq!(project(admission.packet, Some(CLOCK)), Ok(raw.as_slice()));
                    assert_eq!(
                        project(admission.packet, None),
                        Err(Error::Record(data_record::Error::Clock))
                    );
                    assert_eq!(admission.packet.extensions().count(), 0);
                }
                Err(error) => {
                    assert_eq!(error, Error::Capacity);
                    assert_eq!(source.ledger.next_sequence, sequence_before);
                    capped = true;
                    break 'units;
                }
            }
        }
    }
    assert!(capped);
    assert!(source.ledger.charged <= BYTE_LIMIT);
    source.ledger.expire(&mut source.fanout, now + LIFETIME);
    assert_eq!(source.ledger.live, 0);
    assert_eq!(source.fanout.endpoint_data_count(), 0);
    assert_eq!(
        source.admit(&fixture(0, 0, 1, 0, b""), now + LIFETIME, 0),
        Err(Error::Expired)
    );
}
