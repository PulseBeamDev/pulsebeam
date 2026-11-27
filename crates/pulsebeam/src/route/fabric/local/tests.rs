use pulsebeam_core::{dd::DependencyDescriptorWriter, fabric::Header};

use super::*;

fn emission() -> Emission {
    Emission::new(Profile::H264, &[]).unwrap()
}

fn packet(sequence: u32, bytes: &[u8]) -> OwnedPacket {
    FabricPacket::new(
        Header {
            profile: Profile::H264,
            marker: false,
            sequence,
            timestamp: 90,
        },
        &[],
        bytes.into(),
    )
    .unwrap()
}

#[test]
fn reliable_expectation_snapshots_at_first_acceptance_and_rechecks_at_handoff() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let mut destinations = SlotMap::with_key();
    let initial = destinations.insert(());
    let early = destinations.insert(());
    let late = destinations.insert(());
    let denied = destinations.insert(());
    let first = fanout.install_branch(initial, data).unwrap();
    let now = Instant::now();
    let expected = DataExpectation {
        unit_seq: 3,
        fragment_count: 2,
    };
    let suffix = FabricPacket::new(
        Header {
            profile: Profile::Data,
            marker: true,
            sequence: 0,
            timestamp: 3,
        },
        &[],
        Box::<[u8]>::from(crate::participant::data_fabric::R4.as_slice()),
    )
    .unwrap();
    assert_eq!(fanout.offer(now, now, 0, suffix).unwrap().enqueued, 1);
    assert_eq!(
        fanout.take(now, first, initial).unwrap().payload(),
        crate::participant::data_fabric::R4
    );
    let early_branch = fanout.install_branch(early, data).unwrap();
    assert!(fanout.take(now, early_branch, early).is_none());
    let denied_branch = fanout.install_branch(denied, data).unwrap();
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, first, initial, Some),
        None
    );
    assert_eq!(
        fanout.accept_data_expectation(now, expected, |_, dest| dest != denied),
        Ok(ExpectationAdmission::First)
    );
    let late_branch = fanout.install_branch(late, data).unwrap();
    assert_eq!(
        fanout.accept_data_expectation(now, expected, |_, _| panic!("retry must not resnapshot")),
        Ok(ExpectationAdmission::Replay)
    );
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, first, initial, Some),
        Some(expected)
    );
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, early_branch, early, Some),
        Some(expected)
    );
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, denied_branch, denied, Some),
        None
    );
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, late_branch, late, Some),
        None
    );
    assert_eq!(
        fanout.handoff_data_expectation::<()>(now, 3, first, initial, |_| None),
        None
    );
    assert!(fanout.retire_branch(first));
    let replacement = fanout.install_branch(initial, data).unwrap();
    assert_ne!(replacement, first);
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, first, initial, Some),
        None
    );
    assert_eq!(
        fanout.handoff_data_expectation(now, 3, replacement, initial, Some),
        None
    );
}

#[test]
fn reliable_expectations_settle_wholly_missing_units_without_extending_retention() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let first = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
    let branch = fanout.install_branch(first, data).unwrap();
    let now = Instant::now();
    let make = |unit_seq| DataExpectation {
        unit_seq,
        fragment_count: 1,
    };
    let allow = |_, _| true;
    assert_eq!(
        fanout.accept_data_expectation(
            now,
            DataExpectation {
                unit_seq: u64::MAX,
                fragment_count: 0
            },
            allow
        ),
        Err(ExpectationError::InvalidCount)
    );
    for seq in 0..64 {
        assert_eq!(
            fanout.accept_data_expectation(now, make(seq), allow),
            Ok(ExpectationAdmission::First)
        );
    }
    assert_eq!(
        fanout.accept_data_expectation(now, make(64), allow),
        Err(ExpectationError::Capacity)
    );
    let replay = now + Duration::from_secs(1);
    assert_eq!(
        fanout.accept_data_expectation(
            replay,
            DataExpectation {
                unit_seq: 0,
                fragment_count: 2
            },
            allow
        ),
        Err(ExpectationError::Conflict)
    );
    assert_eq!(
        fanout.accept_data_expectation(replay, make(0), allow),
        Ok(ExpectationAdmission::Replay)
    );
    assert_eq!(
        fanout.handoff_data_expectation(replay, 0, branch, first, Some),
        Some(make(0))
    );
    assert!(fanout.take(now, branch, first).is_none());
    let expires = now + Duration::from_secs(2);
    assert_eq!(
        fanout.handoff_data_expectation(expires, 0, branch, first, Some),
        None
    );
    assert_eq!(
        fanout.accept_data_expectation(expires, make(64), allow),
        Err(ExpectationError::Capacity)
    );
    assert_eq!(
        fanout.accept_data_expectation(expires, make(512), allow),
        Err(ExpectationError::Capacity)
    );
    for seq in 0..64 {
        assert_eq!(
            fanout.tick_data_propagation(expires, |_, _, _, _| panic!("expired")),
            Some(PropagationStep::Settled {
                unit: make(seq),
                branch,
                destination: first,
                outcome: PropagationOutcome::Unknown,
            })
        );
    }
    assert_eq!(
        fanout.tick_data_propagation(expires, |_, _, _, _| panic!("drained")),
        None
    );
    for seq in 0..64 {
        assert_eq!(
            fanout.tick_data_delivery(expires, |report| {
                assert_eq!(
                    report,
                    DeliveryReport {
                        unit: make(seq),
                        branch_incarnation: NonZeroU64::MIN,
                        outcome: DeliveryOutcome::Unknown
                    }
                );
                ExpectationWrite::Queued
            }),
            Some(())
        );
    }
    assert_eq!(
        fanout.accept_data_expectation(expires, make(64), allow),
        Ok(ExpectationAdmission::First)
    );
    assert_eq!(
        fanout.accept_data_expectation(expires, make(0), allow),
        Err(ExpectationError::Retired)
    );
    assert_eq!(
        fanout.accept_data_expectation(expires, make(512), allow),
        Ok(ExpectationAdmission::First)
    );
    assert_eq!(
        fanout.accept_data_expectation(expires, make(0), allow),
        Err(ExpectationError::Retired)
    );
}

#[test]
fn repair_claims_require_exact_expectation_and_consume_only_on_successful_handoff() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let destination = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
    let branch = fanout.install_branch(destination, data).unwrap();
    let target = RepairTarget {
        branch,
        destination,
        incarnation: NonZeroU64::MIN,
    };
    let wrong = RepairTarget {
        incarnation: NonZeroU64::MAX,
        ..target
    };
    let now = Instant::now();
    let expires = now + reliable::LIFETIME;
    let unit = DataExpectation {
        unit_seq: 11,
        fragment_count: 2,
    };
    let demand = |id, index| RepairRequest {
        fragment: RepairFragment {
            unit_seq: unit.unit_seq,
            index,
        },
        id: NonZeroU64::new(id).unwrap(),
    };
    let first = demand(1, 1);
    fanout
        .accept_data_expectation(now, unit, |_, _| true)
        .unwrap();
    assert_eq!(
        fanout.request_data_repair(now, target, first),
        RepairAdmission::Ineligible
    );
    fanout.tick_data_propagation(now, |_, _, _, _| ExpectationWrite::Queued);
    assert!(fanout.settle_data_propagation(
        now,
        unit,
        branch,
        destination,
        PropagationOutcome::Confirmed
    ));
    assert_eq!(
        fanout.request_data_repair(now, wrong, first),
        RepairAdmission::Ineligible
    );
    assert_eq!(
        fanout.request_data_repair(now, target, demand(1, 2)),
        RepairAdmission::Ineligible
    );
    assert_eq!(
        fanout.request_data_repair(now, target, first),
        RepairAdmission::First
    );
    let duplicate = demand(2, 1);
    assert_eq!(
        fanout.request_data_repair(now, target, duplicate),
        RepairAdmission::Coalesced
    );
    assert_eq!(
        fanout.request_data_repair(now, target, duplicate),
        RepairAdmission::Replay
    );
    assert!(!fanout.cancel_data_repair(target, duplicate));
    assert!(!fanout.cancel_data_repair(wrong, first));
    assert_eq!(fanout.pending_repair_until(now, target, duplicate), None);
    assert_eq!(fanout.pending_repair_until(now, wrong, first), None);
    assert_eq!(
        fanout.pending_repair_until(now, target, first),
        Some(expires)
    );
    assert!(fanout.arm_network_repair(target, first));
    assert!(!fanout.arm_network_repair(target, first));
    assert!(fanout.claim_data_repair(now, target, first, 99));
    assert!(!fanout.claim_data_repair(now, target, first, 99));
    assert!(!fanout.handoff_data_repair(now, target, first, 100, || panic!("wrong sequence")));
    assert!(!fanout.handoff_data_repair(now, target, first, 99, || false));
    assert!(fanout.handoff_data_repair(now, target, first, 99, || true));
    assert!(!fanout.handoff_data_repair(now, target, first, 99, || panic!("served")));
    assert_eq!(
        fanout.request_data_repair(now, target, first),
        RepairAdmission::Replay
    );

    let next = demand(3, 1);
    assert_eq!(
        fanout.request_data_repair(now, target, next),
        RepairAdmission::First
    );
    assert!(fanout.cancel_data_repair(target, next));
    assert!(!fanout.claim_data_repair(now, target, next, 99));
    let next = demand(4, 1);
    assert_eq!(
        fanout.request_data_repair(now, target, next),
        RepairAdmission::First
    );
    assert!(fanout.claim_data_repair(now, target, next, 99));
    assert!(!fanout.handoff_data_repair(expires, target, next, 99, || panic!("expired")));
    assert_eq!(fanout.expire_data_repair(expires, wrong), None);
    assert_eq!(fanout.expire_data_repair(expires, target), Some(next));

    let following_unit = DataExpectation {
        unit_seq: 12,
        ..unit
    };
    fanout
        .accept_data_expectation(expires, following_unit, |_, _| true)
        .unwrap();
    while let Some(PropagationStep::Settled { .. }) =
        fanout.tick_data_propagation(expires, |_, _, _, _| ExpectationWrite::Queued)
    {}
    assert!(fanout.settle_data_propagation(
        expires,
        following_unit,
        branch,
        destination,
        PropagationOutcome::Confirmed
    ));
    let revoked = RepairRequest {
        fragment: RepairFragment {
            unit_seq: following_unit.unit_seq,
            index: 1,
        },
        ..demand(5, 1)
    };
    assert_eq!(
        fanout.request_data_repair(expires, target, revoked),
        RepairAdmission::First
    );
    assert!(fanout.claim_data_repair(expires, target, revoked, 100));
    assert!(fanout.retire_branch(branch));
    assert!(!fanout.handoff_data_repair(expires, target, revoked, 100, || panic!("revoked")));
}

#[test]
fn fresh_data_receive_anchor_advances_without_reopening_the_old_window() {
    let mut fanout = SourceFanout::new(Emission::new(Profile::Data, &[]).unwrap());
    let now = Instant::now();
    let prior = NonZeroU64::MIN;
    let replacement = NonZeroU64::new(2).unwrap();
    let packet = |sequence| {
        FabricPacket::new(
            Header {
                profile: Profile::Data,
                sequence,
                marker: true,
                timestamp: 3,
            },
            &[],
            Box::<[u8]>::from(crate::participant::data_fabric::R4.as_slice()),
        )
        .unwrap()
    };
    assert_eq!(
        fanout
            .offer_network(now, now, 0, prior, packet(1))
            .unwrap()
            .0,
        1
    );
    let low = u32::try_from(POSITIONS * 4).unwrap();
    let anchor = (1u64 << 32) + u64::from(low);
    assert_eq!(
        fanout
            .offer_network(now, now, anchor, replacement, packet(low))
            .unwrap()
            .0,
        anchor
    );
    assert!(matches!(
        fanout.offer_network(now, now, 0, prior, packet(1)),
        Err(Rejection::Stale)
    ));
    assert_eq!(
        fanout
            .offer_network(now, now, 0, prior, packet(low + 1))
            .unwrap()
            .0,
        anchor + 1
    );
}

#[test]
fn exact_media_cache_is_bounded_by_full_position_ingress_and_first_arrival() {
    let now = Instant::now();
    let expiry = now + crate::route::fabric::network_repair::MAX_REPAIR_LIFETIME;
    let mut fanout = SourceFanout::new(emission());
    let position = (1u64 << 32) | 0x1ffff;
    let original = packet(0x1ffff, &[0x65; 1188]);
    let parent = NonZeroU64::MIN;
    fanout
        .offer_bound_network(now, now, position, parent, original.clone(), None)
        .unwrap();
    let (cached, deadline) = fanout
        .cached_peer_media(position, Some(parent), now)
        .unwrap();
    assert!(cached.same_content(&original));
    assert_eq!(deadline, expiry);
    assert!(fanout.cached_peer_media(position, None, now).is_none());
    assert!(
        fanout
            .cached_peer_media(position, Some(NonZeroU64::MAX), now)
            .is_none()
    );
    assert!(
        fanout
            .cached_peer_media(0x1ffff, Some(parent), now)
            .is_none()
    );
    let duplicate = now + Duration::from_millis(60);
    fanout
        .offer_bound_network(duplicate, duplicate, position, parent, original, None)
        .unwrap();
    assert!(
        fanout
            .cached_peer_media(position, Some(parent), expiry)
            .is_none()
    );

    let exhausted = packet(0x20000, &[0x65])
        .arrival_view(0)
        .unwrap()
        .into_owned();
    fanout
        .offer_bound_network(duplicate, duplicate, position + 1, parent, exhausted, None)
        .unwrap();
    assert!(
        fanout
            .cached_peer_media(position + 1, Some(parent), duplicate)
            .is_none()
    );
    assert_eq!(
        fanout.offer_bound_network(
            duplicate,
            duplicate,
            position,
            parent,
            packet(0x1ffff, &[0x66]),
            None,
        ),
        Err(Rejection::Conflict)
    );
    assert!(
        fanout
            .cached_peer_media(position, Some(parent), duplicate)
            .is_none()
    );
}

#[test]
fn cached_data_location_requires_an_unambiguous_live_original() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let now = Instant::now();
    let fragment = RepairFragment {
        unit_seq: 3,
        index: 1,
    };
    let sequence = 1u64 << 32;
    let packet = |sequence, unit, marker| {
        let mut fragment = crate::participant::data_fabric::R4;
        fragment[11] = unit;
        FabricPacket::new(
            Header {
                profile: Profile::Data,
                sequence,
                timestamp: 0x2876_1234,
                marker,
            },
            &[],
            Box::<[u8]>::from(fragment.as_slice()),
        )
        .unwrap()
    };
    assert_eq!(fanout.cached_data_position(fragment, 2, None, now), None);
    fanout
        .offer(now, now, sequence, packet(0, 3, true))
        .unwrap();
    fanout
        .offer(now, now, sequence + 1, packet(1, 4, true))
        .unwrap();
    fanout
        .offer(now, now, sequence + 2, packet(2, 3, false))
        .unwrap();
    assert_eq!(
        fanout.cached_data_position(fragment, 2, None, now),
        Some(sequence)
    );
    assert_eq!(fanout.cached_data_position(fragment, 3, None, now), None);
    assert_eq!(
        fanout.cached_data_position(fragment, 2, None, now + reliable::LIFETIME),
        None
    );
    fanout
        .offer(now, now, sequence + 3, packet(3, 3, true))
        .unwrap();
    assert_eq!(fanout.cached_data_position(fragment, 2, None, now), None);
}

#[test]
fn live_and_repair_race_through_one_branch_queue_and_keep_original_content() {
    for (live_first, saturated, network) in [
        (true, false, false),
        (false, false, false),
        (false, true, false),
        (true, false, true),
        (false, false, true),
    ] {
        let data = Emission::new(Profile::Data, &[]).unwrap();
        let mut fanout = SourceFanout::new(data);
        let mut destinations = SlotMap::<ParticipantHandle, ()>::with_key();
        let destination = destinations.insert(());
        let now = Instant::now();
        let original = FabricPacket::new(
            Header {
                profile: Profile::Data,
                marker: true,
                sequence: 0,
                timestamp: 3,
            },
            &[],
            Box::<[u8]>::from(crate::participant::data_fabric::R4.as_slice()),
        )
        .unwrap();
        if !live_first && !network {
            assert_eq!(
                fanout
                    .offer(now, now, 0, original.clone())
                    .unwrap()
                    .enqueued,
                0
            );
        }
        let branch = fanout.install_branch(destination, data).unwrap();
        let target = RepairTarget {
            branch,
            destination,
            incarnation: NonZeroU64::MIN,
        };
        let unit = DataExpectation {
            unit_seq: 3,
            fragment_count: 2,
        };
        let request = RepairRequest {
            fragment: RepairFragment {
                unit_seq: 3,
                index: 1,
            },
            id: NonZeroU64::MIN,
        };
        let spectator = network.then(|| {
            let spectator = destinations.insert(());
            (fanout.install_branch(spectator, data).unwrap(), spectator)
        });
        fanout
            .accept_data_expectation(now, unit, |_, selected| selected == destination)
            .unwrap();
        fanout.tick_data_propagation(now, |_, _, _, _| ExpectationWrite::Queued);
        assert!(fanout.settle_data_propagation(
            now,
            unit,
            branch,
            destination,
            PropagationOutcome::Confirmed
        ));
        if live_first {
            assert_eq!(
                fanout
                    .offer(now, now, 0, original.clone())
                    .unwrap()
                    .enqueued,
                if network { 2 } else { 1 }
            );
        }
        assert_eq!(
            fanout.request_data_repair(now, target, request),
            RepairAdmission::First
        );
        let drain_filler = |fanout: &mut SourceFanout| {
            assert_eq!(
                fanout.admit_data(
                    now,
                    branch,
                    destination,
                    None,
                    |_| true,
                    |seq, _, repair| {
                        assert!(seq > 0);
                        assert_eq!(repair, None);
                        true
                    }
                ),
                Some(DataHandoff {
                    written: true,
                    completed: None
                })
            );
        };
        if saturated {
            use pulsebeam_proto::data_record::{Header as DataHeader, Record};
            for sequence in 1..=u32::try_from(QUEUE_PACKETS).unwrap() {
                let unit_seq = u64::from(sequence) + 3;
                let record = Record::new(
                    DataHeader {
                        unit_seq,
                        index: 0,
                        count: 1,
                        unit_len: 1,
                        capture: None,
                    },
                    b"x",
                    None,
                )
                .unwrap();
                let mut payload = vec![0; record.encoded_len()];
                record.write(&mut payload).unwrap();
                let filler = FabricPacket::new(
                    Header {
                        sequence,
                        timestamp: sequence + 3,
                        ..original.header()
                    },
                    &[],
                    payload.into_boxed_slice(),
                )
                .unwrap();
                assert_eq!(
                    fanout
                        .offer(now, now, u64::from(sequence), filler)
                        .unwrap()
                        .enqueued,
                    1
                );
            }
            assert_eq!(
                fanout.enqueue_data_repair(now, target, request, 0),
                RepairQueue::Backpressured
            );
            assert_eq!(fanout.offer(now, now, 0, original.clone()).unwrap().full, 1);
            drain_filler(&mut fanout);
        }
        let queued = if network {
            assert!(fanout.arm_network_repair(target, request));
            let demand = super::super::network_repair::Demand {
                request,
                sequence: 0,
                expires: now + reliable::LIFETIME,
            };
            if !live_first {
                let mut fragment = crate::participant::data_fabric::R4;
                fragment[11] = 4;
                let wrong_unit = FabricPacket::new(
                    original.header(),
                    &[],
                    Box::<[u8]>::from(fragment.as_slice()),
                )
                .unwrap();
                assert_eq!(
                    fanout.enqueue_network_repair(now, target, demand, wrong_unit, NonZeroU64::MIN),
                    RepairQueue::Unavailable
                );
                assert!(!fanout.fates.contains_key(&0));
            }
            fanout.enqueue_network_repair(now, target, demand, original.clone(), NonZeroU64::MIN)
        } else {
            fanout.enqueue_data_repair(now, target, request, 0)
        };
        assert_eq!(queued, RepairQueue::Queued);
        if let Some((branch, destination)) = spectator
            && !live_first
        {
            assert!(fanout.transit(0, 6).is_none());
            assert!(fanout.take(now, branch, destination).is_none());
        }
        if saturated {
            for _ in 1..QUEUE_PACKETS {
                drain_filler(&mut fanout);
            }
        }
        let live = fanout.offer(now, now, 0, original.clone()).unwrap();
        assert_eq!(live.duplicates, if network && live_first { 2 } else { 1 });
        assert_eq!(live.enqueued, usize::from(network && !live_first));
        let write = |sequence, packet: PacketView<'_>, claim| {
            assert_eq!(sequence, 0);
            assert_eq!(packet.header(), original.header());
            assert_eq!(packet.payload(), original.payload());
            assert_eq!(claim, Some(request));
        };
        assert_eq!(
            fanout.admit_data(
                now,
                branch,
                destination,
                Some(target.incarnation),
                |_| true,
                |seq, packet, claim| {
                    write(seq, packet, claim);
                    false
                }
            ),
            Some(DataHandoff {
                written: false,
                completed: None
            })
        );
        assert_eq!(
            fanout.admit_data(
                now,
                branch,
                destination,
                Some(target.incarnation),
                |_| true,
                |seq, packet, claim| {
                    write(seq, packet, claim);
                    true
                }
            ),
            Some(DataHandoff {
                written: true,
                completed: Some(request)
            })
        );
        assert_eq!(
            fanout.admit_data(
                now,
                branch,
                destination,
                Some(target.incarnation),
                |_| true,
                |_, _, _| -> bool { panic!("duplicate") }
            ),
            None
        );
        let again = RepairRequest {
            id: NonZeroU64::new(2).unwrap(),
            ..request
        };
        let late = now + MAX_AGE;
        assert_eq!(
            fanout.request_data_repair(late, target, again),
            RepairAdmission::First
        );
        assert_eq!(
            fanout.enqueue_data_repair(late, target, again, 0),
            RepairQueue::Queued
        );
        assert!(fanout.cancel_data_repair(target, again));
        assert_eq!(
            fanout.admit_data(
                late,
                branch,
                destination,
                Some(target.incarnation),
                |_| true,
                |_, _, _| -> bool { panic!("cancelled repeat") }
            ),
            None
        );
    }
}

#[test]
fn local_expectation_keeps_the_source_admission_deadline() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let destination = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
    let branch = fanout.install_branch(destination, data).unwrap();
    let now = Instant::now();
    let deadline = now + Duration::from_millis(10);
    let unit = DataExpectation {
        unit_seq: 7,
        fragment_count: 1,
    };
    assert_eq!(
        fanout
            .accept_data_expectation_until(now, Some(deadline), unit, |_, _| Some(NonZeroU64::MIN)),
        Ok(ExpectationAdmission::First)
    );
    assert_eq!(
        fanout.tick_data_propagation(now, |_, _, _, _| ExpectationWrite::Queued),
        Some(PropagationStep::Attempted)
    );
    assert_eq!(fanout.data_propagation_deadline(), Some(deadline));
    assert_eq!(
        fanout.accept_data_expectation_until(deadline, Some(deadline), unit, |_, _| panic!(
            "expired retry"
        )),
        Err(ExpectationError::Retired)
    );
    assert_eq!(
        fanout.tick_data_propagation(deadline, |_, _, _, _| panic!("expired admission")),
        Some(PropagationStep::Settled {
            unit,
            branch,
            destination,
            outcome: PropagationOutcome::Unknown,
        })
    );
}

#[test]
fn ordered_handoff_requires_membership_in_the_original_cohort() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let mut destinations = SlotMap::with_key();
    let receiver = destinations.insert(());
    let replaced = destinations.insert(());
    let late = destinations.insert(());
    let branch = fanout.install_branch(receiver, data).unwrap();
    let replaced_branch = fanout.install_branch(replaced, data).unwrap();
    let now = Instant::now();
    let unit = DataExpectation {
        unit_seq: 3,
        fragment_count: 2,
    };
    let incarnation = NonZeroU64::MIN;
    assert_eq!(
        fanout.accept_data_expectation_until(now, None, unit, |_, _| Some(incarnation)),
        Ok(ExpectationAdmission::First)
    );
    let late_branch = fanout.install_branch(late, data).unwrap();
    let original = FabricPacket::new(
        Header {
            profile: Profile::Data,
            marker: true,
            sequence: 0,
            timestamp: 3,
        },
        &[],
        Box::<[u8]>::from(crate::participant::data_fabric::R4.as_slice()),
    )
    .unwrap();
    assert_eq!(fanout.offer(now, now, 0, original).unwrap().enqueued, 3);
    assert_eq!(
        fanout.admit_data(
            now,
            branch,
            receiver,
            Some(incarnation),
            |_| true,
            |_, _, _| DataWrite::Deferred
        ),
        None
    );
    assert_eq!(
        fanout.admit_data(
            now,
            branch,
            receiver,
            Some(incarnation),
            |_| true,
            |_, _, _| true
        ),
        Some(DataHandoff {
            written: true,
            completed: None
        })
    );
    for (branch, receiver, incarnation) in [
        (replaced_branch, replaced, NonZeroU64::MAX),
        (late_branch, late, incarnation),
    ] {
        assert_eq!(
            fanout.admit_data(
                now,
                branch,
                receiver,
                Some(incarnation),
                |_| true,
                |_, _, _| -> bool { panic!("wrong ordered cohort") }
            ),
            None
        );
    }
}

#[test]
fn oversized_expectation_cohort_leaves_no_work_or_replay_fence() {
    let mut expectations = reliable::Expectations::default();
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout.install_branch(destination, data).unwrap();
    let now = Instant::now();
    let unit = DataExpectation {
        unit_seq: 512,
        fragment_count: 1,
    };
    let member = |destination| ((branch, destination), NonZeroU64::MIN);
    let oversized: Vec<_> = (0..=MAX_BRANCHES)
        .map(|_| member(destinations.insert(())))
        .collect();
    assert_eq!(
        expectations.accept(now, None, unit, || oversized.iter().copied(), Vec::new),
        Err(ExpectationError::Capacity)
    );
    assert_eq!(expectations.deadline(), None);
    assert_eq!(expectations.delivery_deadline(), None);
    assert_eq!(
        expectations.accept(
            now,
            None,
            DataExpectation {
                unit_seq: 0,
                ..unit
            },
            || [member(destination)],
            Vec::new,
        ),
        Ok(ExpectationAdmission::First)
    );
    let at_cap = DataExpectation {
        unit_seq: 511,
        ..unit
    };
    assert_eq!(
        expectations.accept(
            now,
            None,
            at_cap,
            || oversized[..MAX_BRANCHES].iter().copied(),
            Vec::new
        ),
        Ok(ExpectationAdmission::First)
    );
    assert_eq!(
        expectations.accept(
            now,
            None,
            at_cap,
            || -> [_; 1] { panic!("replay must not resnapshot") },
            || panic!("replay must not resnapshot remote receivers")
        ),
        Ok(ExpectationAdmission::Replay)
    );
}

#[test]
fn repair_requires_confirmed_expectation_even_after_reporting() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout.install_branch(destination, data).unwrap();
    let now = Instant::now();
    let target = RepairTarget {
        branch,
        destination,
        incarnation: NonZeroU64::MIN,
    };
    for (unit_seq, outcome) in [PropagationOutcome::Unknown, PropagationOutcome::Confirmed]
        .into_iter()
        .enumerate()
    {
        let unit_seq = u64::try_from(unit_seq).unwrap();
        let unit = DataExpectation {
            unit_seq,
            fragment_count: 1,
        };
        let request = RepairRequest {
            fragment: RepairFragment { unit_seq, index: 0 },
            id: NonZeroU64::MIN,
        };
        fanout
            .accept_data_expectation(now, unit, |_, _| true)
            .unwrap();
        fanout.tick_data_propagation(now, |_, _, _, _| ExpectationWrite::Queued);
        assert!(fanout.settle_data_propagation(now, unit, branch, destination, outcome));
        assert_eq!(
            fanout.tick_data_propagation(now, |_, _, _, _| unreachable!()),
            Some(PropagationStep::Settled {
                unit,
                branch,
                destination,
                outcome
            })
        );
        assert_eq!(
            fanout.repair_until(now, target, request),
            (outcome == PropagationOutcome::Confirmed).then_some(now + reliable::LIFETIME)
        );
    }
}

#[test]
fn reliable_propagation_retries_and_settles_only_the_current_exact_child() {
    let data = Emission::new(Profile::Data, &[]).unwrap();
    let mut fanout = SourceFanout::new(data);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let wrong = destinations.insert(());
    let branch = fanout.install_branch(destination, data).unwrap();
    let now = Instant::now();
    let retry = now + reliable::RETRY;
    let unit = DataExpectation {
        unit_seq: 3,
        fragment_count: 2,
    };
    let confirm = PropagationOutcome::Confirmed;
    assert_eq!(
        fanout.accept_data_expectation(now, unit, |_, _| true),
        Ok(ExpectationAdmission::First)
    );
    assert_eq!(
        fanout.tick_data_propagation(now, |key, dest, expected, incarnation| {
            assert_eq!(
                (key, dest, expected, incarnation),
                (branch, destination, unit, NonZeroU64::MIN)
            );
            ExpectationWrite::Backpressured
        }),
        Some(PropagationStep::Attempted)
    );
    assert!(!fanout.settle_data_propagation(now, unit, branch, destination, confirm));
    assert_eq!(
        fanout.record_data_receipt(now, unit, branch, destination, NonZeroU64::MIN),
        None
    );
    assert_eq!(
        fanout.tick_data_propagation(now, |_, _, _, _| panic!("retry too soon")),
        None
    );
    assert_eq!(
        fanout.tick_data_propagation(retry, |key, dest, expected, incarnation| {
            assert_eq!(
                (key, dest, expected, incarnation),
                (branch, destination, unit, NonZeroU64::MIN)
            );
            ExpectationWrite::Queued
        }),
        Some(PropagationStep::Attempted)
    );
    assert!(!fanout.settle_data_propagation(retry, unit, branch, wrong, confirm));
    assert_eq!(
        fanout.tick_data_delivery(retry, |_| panic!("propagation is not delivery")),
        None
    );
    assert_eq!(
        fanout.record_data_receipt(retry, unit, branch, destination, NonZeroU64::MAX),
        None
    );
    assert_eq!(
        fanout.record_data_receipt(retry, unit, branch, wrong, NonZeroU64::MIN),
        None
    );
    assert_eq!(
        fanout.record_data_receipt(retry, unit, branch, destination, NonZeroU64::MIN),
        Some(ReceiptAcceptance::First)
    );
    assert_eq!(
        fanout.record_data_receipt(retry, unit, branch, destination, NonZeroU64::MIN),
        Some(ReceiptAcceptance::Replay)
    );
    assert!(!fanout.settle_data_propagation(
        retry,
        DataExpectation {
            fragment_count: 1,
            ..unit
        },
        branch,
        destination,
        confirm
    ));
    assert!(fanout.settle_data_propagation(retry, unit, branch, destination, confirm));
    assert!(!fanout.settle_data_propagation(
        retry,
        unit,
        branch,
        destination,
        PropagationOutcome::Failed
    ));
    assert_eq!(
        fanout.tick_data_propagation(retry, |_, _, _, _| panic!("already settled")),
        Some(PropagationStep::Settled {
            unit,
            branch,
            destination,
            outcome: confirm,
        })
    );
    let confirmed = DeliveryReport {
        unit,
        branch_incarnation: NonZeroU64::MIN,
        outcome: DeliveryOutcome::Confirmed,
    };
    assert_eq!(
        fanout.tick_data_delivery(retry, |report| {
            assert_eq!(report, confirmed);
            ExpectationWrite::Backpressured
        }),
        Some(())
    );
    assert_eq!(
        fanout.tick_data_delivery(retry, |_| panic!("retry too soon")),
        None
    );
    let at = retry + reliable::RETRY;
    assert_eq!(
        fanout.tick_data_delivery(at, |report| {
            assert_eq!(report, confirmed);
            ExpectationWrite::Queued
        }),
        Some(())
    );
    assert_eq!(
        fanout.tick_data_delivery(at, |_| panic!("already published")),
        None
    );
    let retry = at;
    let revoked = DataExpectation {
        unit_seq: 4,
        ..unit
    };
    assert_eq!(
        fanout.accept_data_expectation(retry, revoked, |_, _| true),
        Ok(ExpectationAdmission::First)
    );
    assert!(fanout.retire_branch(branch));
    let replacement = fanout.install_branch(destination, data).unwrap();
    assert_eq!(
        fanout.record_data_receipt(retry, unit, branch, destination, NonZeroU64::MIN),
        None
    );
    assert_eq!(
        fanout.record_data_receipt(retry, unit, replacement, destination, NonZeroU64::MIN),
        None
    );
    assert!(!fanout.settle_data_propagation(retry, revoked, replacement, destination, confirm));
    assert_eq!(
        fanout.tick_data_propagation(retry, |_, _, _, _| panic!("revoked before handoff")),
        Some(PropagationStep::Settled {
            unit: revoked,
            branch,
            destination,
            outcome: PropagationOutcome::Failed,
        })
    );
    assert_eq!(
        fanout.tick_data_propagation(retry, |_, _, _, _| panic!("drained")),
        None
    );
    assert_eq!(
        fanout.tick_data_delivery(retry, |report| {
            assert_eq!(
                report,
                DeliveryReport {
                    unit: revoked,
                    branch_incarnation: NonZeroU64::MIN,
                    outcome: DeliveryOutcome::Failed
                }
            );
            ExpectationWrite::Queued
        }),
        Some(())
    );
}

#[test]
fn network_registry_must_support_the_full_emission_even_when_hints_are_absent() {
    use pulsebeam_core::fabric::Extension;

    let base = emission();
    let enhanced = Emission::new(Profile::H264, &[ExtensionKind::VideoOrientation]).unwrap();
    let bare = packet(5, b"payload");
    let with_hint = FabricPacket::new(
        bare.header(),
        &[Extension {
            kind: ExtensionKind::VideoOrientation,
            value: &[0],
        }],
        Box::<[u8]>::from(&b"payload"[..]),
    )
    .unwrap();

    assert!(base.fits_registry(1, 0));
    assert!(!enhanced.fits_registry(1, 0));
    assert!(enhanced.fits_registry(1, 1));
    assert!(!enhanced.fits_registry(2, 1));
    assert!(!enhanced.fits_registry(1, u8::MAX));
    assert!(enhanced.accepts(&bare));
    assert!(enhanced.accepts(&with_hint));
    assert!(!base.accepts(&with_hint));
}

#[test]
fn network_wrap_and_differing_hop_counts_share_one_local_admission() {
    let mut fanout = SourceFanout::new(emission());
    let destination = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
    let branch = fanout.install_branch(destination, emission()).unwrap();
    let now = Instant::now();
    let anchor = u64::from(u32::MAX);
    let first = packet(u32::MAX, b"first");
    let original = NonZeroU64::MIN;
    let alternate = NonZeroU64::new(2).unwrap();
    assert_eq!(
        fanout
            .offer_network(
                now,
                now,
                anchor,
                original,
                first.arrival_view(0).unwrap().into_owned()
            )
            .unwrap()
            .1
            .enqueued,
        1
    );
    assert_eq!(
        fanout
            .offer_network(now, now, anchor, alternate, first)
            .unwrap()
            .1
            .duplicates,
        1
    );
    assert_eq!(
        fanout
            .offer_network(now, now, anchor, alternate, packet(0, b"next"))
            .unwrap()
            .1
            .enqueued,
        1
    );
    assert_eq!(
        fanout.offer_network(now, now, anchor, alternate, packet(1 << 31, b"ambiguous")),
        Err(Rejection::Stale)
    );
    let delivered = fanout.admit_with_evidence(
        now,
        branch,
        destination,
        |ingress| ingress == Some(alternate),
        |sequence, packet, evidence| {
            assert_eq!(sequence, anchor.saturating_add(1));
            assert_eq!(evidence.ingress, Some(alternate));
            assert_eq!(packet.payload(), b"next");
            Ok::<_, std::convert::Infallible>(())
        },
    );
    assert_eq!(delivered, Ok(Some(())));
    assert!(fanout.take(now, branch, destination).is_none());
}

#[test]
fn slow_branch_does_not_consume_retry_or_copy_the_shared_image() {
    for ingress in [None, NonZeroU64::new(1)] {
        prove_branch_isolation(ingress);
    }
}

fn prove_branch_isolation(ingress: Option<NonZeroU64>) {
    let mut fanout = SourceFanout::new(emission());
    let mut destinations = SlotMap::with_key();
    let slow = destinations.insert(());
    let fast = destinations.insert(());
    let left = fanout.install_branch(slow, emission()).unwrap();
    let right = fanout.install_branch(fast, emission()).unwrap();
    let now = Instant::now();
    let count = u32::try_from(QUEUE_PACKETS).unwrap();
    let offer = |fanout: &mut SourceFanout, at, sequence, payload: &[u8]| {
        let packet = packet(sequence, payload);
        if let Some(ingress) = ingress {
            fanout
                .offer_bound_network(at, at, u64::from(sequence), ingress, packet, None)
                .map(|(_, report)| report)
        } else {
            fanout.offer(at, at, u64::from(sequence), packet)
        }
    };
    for sequence in 0..count {
        let report = offer(&mut fanout, now, sequence, b"opaque").unwrap();
        assert_eq!(report.enqueued, 2);
        let fast_copy = fanout.take(now, right, fast).unwrap();
        assert_eq!(fast_copy.payload(), b"opaque");
        assert_eq!(
            fast_copy.payload().as_ptr(),
            fanout
                .fates
                .get(&u64::from(sequence))
                .unwrap()
                .packet
                .payload()
                .as_ptr()
        );
    }
    let report = offer(&mut fanout, now, count, b"retry").unwrap();
    assert_eq!(
        report,
        Offers {
            enqueued: 1,
            full: 1,
            ..Offers::default()
        }
    );
    let fast_copy = fanout.take(now, right, fast).unwrap();
    assert_eq!(fast_copy.payload(), b"retry");
    let fast_payload = fast_copy.payload().as_ptr();
    fanout.take(now, left, slow).unwrap();
    let retry_at = now.checked_add(Duration::from_millis(1)).unwrap();
    let report = offer(&mut fanout, retry_at, count, b"retry").unwrap();
    let transit = fanout.transit(count.into(), 0).unwrap();
    assert_eq!(transit.work.sequence(), u64::from(count));
    assert_eq!(transit.work.arrival(), now);
    let expiry = now.checked_add(MAX_AGE).unwrap();
    assert_eq!(
        transit
            .work
            .enqueue(expiry, &mut LiveAdmissions::default(), || {
                panic!("expired transit queued")
            }),
        LiveOffer::Stale
    );
    assert_eq!(
        report,
        Offers {
            enqueued: 1,
            duplicates: 1,
            ..Offers::default()
        }
    );
    assert!(fanout.take(retry_at, right, fast).is_none());
    for _ in 1..count {
        fanout.take(retry_at, left, slow).unwrap();
    }
    let retry = fanout.take(retry_at, left, slow).unwrap();
    assert_eq!(retry.payload().as_ptr(), fast_payload);
    assert_eq!(retry.payload(), b"retry");
}

fn dd_switch_packet(
    writer: &mut DependencyDescriptorWriter,
    sequence: u32,
    frame_number: u16,
    attach: bool,
) -> OwnedPacket {
    dd_test_packet(writer, sequence, frame_number, attach, false)
}

fn dd_test_packet(
    writer: &mut DependencyDescriptorWriter,
    sequence: u32,
    frame_number: u16,
    attach: bool,
    omitted: bool,
) -> OwnedPacket {
    use pulsebeam_core::dd::{
        DdFields, DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
        TemplateDependencyStructure,
    };
    let base = FrameDependencyTemplate {
        dtis: [DecodeTargetIndication::Switch].into_iter().collect(),
        ..Default::default()
    };
    let enhancement = FrameDependencyTemplate {
        temporal_id: 1,
        dtis: [DecodeTargetIndication::NotPresent].into_iter().collect(),
        ..Default::default()
    };
    let descriptor = DependencyDescriptor {
        start_of_frame: true,
        end_of_frame: true,
        template_id: if omitted { 6 } else { 5 },
        frame_number,
        attached_structure: attach.then(|| {
            Box::new(TemplateDependencyStructure {
                template_id_offset: 5,
                decode_target_count: 1,
                templates: [base.clone(), enhancement.clone()].into_iter().collect(),
                ..Default::default()
            })
        }),
        active_decode_targets: None,
        fields: DdFields::empty(),
        frame_dependencies: if omitted { enhancement } else { base },
        resolution: None,
    };
    write_dd_test_packet(writer, Profile::H264, sequence, 9000, &descriptor)
}

fn write_dd_test_packet(
    writer: &mut DependencyDescriptorWriter,
    profile: Profile,
    sequence: u32,
    timestamp: u32,
    descriptor: &pulsebeam_core::dd::DependencyDescriptor,
) -> OwnedPacket {
    use pulsebeam_core::{dd::MAX_DD_LEN, fabric::Extension};
    let mut raw = [0; MAX_DD_LEN];
    let len = writer.write(descriptor, &mut raw).unwrap();
    FabricPacket::new(
        Header {
            profile,
            marker: true,
            sequence,
            timestamp,
        },
        &[Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value: &raw[..len],
        }],
        Box::from(&b"opaque"[..]),
    )
    .unwrap()
}

#[test]
fn protection_requires_qualified_h264_base_packets() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    let base = dd_test_packet(&mut writer, 100, 10, true, false);
    let enhanced = dd_test_packet(&mut writer, 101, 11, false, true);
    for (position, image, qualifies) in [
        (100, base, BasePacket::Base),
        (101, enhanced, BasePacket::Enhancement),
        (102, packet(102, b"no DD"), BasePacket::Unknown),
    ] {
        fanout.offer(now, now, position, image).unwrap();
        assert_eq!(fanout.transit(position, 1).unwrap().h264_base, qualifies);
    }
}

#[test]
fn filtered_gap_suspends_without_holding_opaque_whole_source() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let whole = destinations.insert(());
    let filtered = destinations.insert(());
    let full_branch = fanout.install_branch(whole, emission).unwrap();
    let target_branch = fanout
        .install_filtered_branch(filtered, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    let mut offer = |fanout: &mut SourceFanout, sequence, frame, attach| {
        let packet = dd_switch_packet(&mut writer, sequence, frame, attach);
        let offered = fanout
            .offer(now, now, u64::from(sequence), packet.clone())
            .unwrap();
        let delivered = fanout.take(now, full_branch, whole).unwrap();
        assert_eq!(delivered.payload(), packet.payload());
        assert_eq!(delivered.extensions().next(), packet.extensions().next());
        offered
    };
    assert_eq!(offer(&mut fanout, 100, 10, true).enqueued, 2);
    assert_eq!(offer(&mut fanout, 111, 12, false).suspended, 1);
    assert!(fanout.take(now, target_branch, filtered).is_none());
    assert_eq!(offer(&mut fanout, 110, 11, true).enqueued, 1);
    assert!(fanout.take(now, target_branch, filtered).is_none());
    assert_eq!(offer(&mut fanout, 112, 13, true).enqueued, 2);
    assert_eq!(
        fanout.admit_with_evidence(
            now,
            target_branch,
            filtered,
            |_| true,
            |sequence, packet, evidence| {
                assert_eq!(sequence, 112);
                assert!(packet.header().marker);
                assert_eq!(evidence.mode, pulsebeam_core::dd::output::Mode::Filtered(0));
                assert!(evidence.entry);
                let (frame, dd) = evidence.dd.unwrap();
                assert_eq!((frame, dd.structure_sequence), (13, 112));
                Err::<(), _>("queue refused")
            }
        ),
        Err("queue refused")
    );
    assert_eq!(
        fanout
            .take(now, target_branch, filtered)
            .unwrap()
            .header()
            .sequence,
        112
    );
    // A distinct packet cannot claim the same frame's start and end.
    let conflict = offer(&mut fanout, 113, 13, false);
    assert_eq!(conflict.suspended, 1);
    assert_eq!(conflict.enqueued, 1);
    assert_eq!(offer(&mut fanout, 114, 14, true).enqueued, 2);
    let absent = fanout.offer(now, now, 115, packet(115, b"no DD")).unwrap();
    assert_eq!(absent.suspended, 1);
    assert!(fanout.take(now, target_branch, filtered).is_none());
    assert_eq!(
        fanout.take(now, full_branch, whole).unwrap().payload(),
        b"no DD"
    );
}

#[test]
fn stale_filtered_head_fences_live_continuation_until_fresh_entry() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let now = Instant::now();
    let next = now + Duration::from_millis(10);
    let expired = now + MAX_AGE + Duration::from_millis(1);
    let mut writer = DependencyDescriptorWriter::new();
    let mut offer = |fanout: &mut SourceFanout, at, sequence: u16, attach| {
        let packet = dd_switch_packet(&mut writer, u32::from(sequence), sequence, attach);
        fanout.offer(at, at, u64::from(sequence), packet).unwrap()
    };
    assert_eq!(offer(&mut fanout, now, 100, true).enqueued, 1);
    assert_eq!(
        fanout.admit_with_evidence(
            now,
            branch,
            destination,
            |_| true,
            |sequence, _, evidence| {
                assert_eq!(sequence, 100);
                assert!(evidence.entry);
                Err::<(), _>("partial carrier admission")
            }
        ),
        Err("partial carrier admission")
    );
    assert_eq!(offer(&mut fanout, next, 101, false).enqueued, 1);
    assert!(fanout.take(expired, branch, destination).is_none());
    assert_eq!(offer(&mut fanout, expired, 102, false).awaiting_entry, 1);
    assert!(fanout.take(expired, branch, destination).is_none());
    assert_eq!(offer(&mut fanout, expired, 103, true).enqueued, 1);
    assert_eq!(
        fanout.admit_with_evidence(
            expired,
            branch,
            destination,
            |_| true,
            |sequence, _, evidence| {
                assert!(evidence.entry);
                assert_eq!(evidence.reset_after, Some(101));
                assert!(evidence.dd.is_some());
                Ok::<_, ()>(sequence)
            }
        ),
        Ok(Some(103))
    );
}

#[test]
fn spatial_dd_reports_height_and_cumulative_target_bytes_before_subscribers_exist() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication::{NotPresent, Switch},
        DependencyDescriptor, FrameDependencyTemplate, RenderResolution,
        TemplateDependencyStructure,
    };
    let emission = Emission::new(Profile::Vp9, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let base = FrameDependencyTemplate {
        dtis: [Switch, Switch].into_iter().collect(),
        ..Default::default()
    };
    let upper = FrameDependencyTemplate {
        spatial_id: 1,
        dtis: [NotPresent, Switch].into_iter().collect(),
        frame_diffs: [1].into_iter().collect(),
        ..Default::default()
    };
    let structure = TemplateDependencyStructure {
        decode_target_count: 2,
        templates: [base.clone(), upper.clone()].into_iter().collect(),
        resolutions: [
            RenderResolution {
                width: 320,
                height: 180,
            },
            RenderResolution {
                width: 640,
                height: 360,
            },
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    for (index, template, bytes) in [(0, base, [6, 6, 0]), (1, upper, [0, 6, 0])] {
        let packet = write_dd_test_packet(
            &mut writer,
            Profile::Vp9,
            100 + u32::from(index),
            9000,
            &DependencyDescriptor {
                start_of_frame: true,
                end_of_frame: true,
                template_id: index,
                frame_number: 10 + u16::from(index),
                attached_structure: (index == 0).then(|| Box::new(structure.clone())),
                frame_dependencies: template,
                ..Default::default()
            },
        );
        let report = fanout
            .offer(now, now, u64::from(100 + u32::from(index)), packet)
            .unwrap();
        assert_eq!(
            report.dd_sample,
            DecodeTargetSample::Measured {
                context: DecodeTargetRateContext {
                    structure_origin: 100,
                    active_targets: 0b11
                },
                through: 100 + u64::from(index),
                bytes,
                pictures: if index == 0 { [1, 0, 0] } else { [0, 1, 0] },
            }
        );
        assert_eq!(
            report
                .dd_structure
                .map(|facts| (facts.decode_target_heights, facts.temporal_only)),
            (index == 0).then_some((Some([180, 360, 0]), Some(false)))
        );
        assert_eq!(report.enqueued, 0);
    }
}

#[test]
fn vp9_picture_marker_follows_the_highest_retained_spatial_frame() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication::{NotPresent, Switch},
        DependencyDescriptor, FrameDependencyTemplate, TemplateDependencyStructure,
    };
    let template = |spatial_id, dtis: [_; 3]| FrameDependencyTemplate {
        spatial_id,
        dtis: dtis.into_iter().collect(),
        ..Default::default()
    };
    let base = template(0, [Switch, Switch, Switch]);
    let middle = template(1, [Switch, Switch, NotPresent]);
    let top = template(2, [NotPresent, Switch, NotPresent]);
    let structure = TemplateDependencyStructure {
        decode_target_count: 3,
        templates: [base.clone(), middle.clone(), top.clone()]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    for (frame, ending_target) in [(base, 2), (middle, 0), (top, 1)] {
        let mut qualified = Qualified {
            descriptor: DependencyDescriptor {
                frame_dependencies: frame,
                ..Default::default()
            },
            active_targets: 0b111,
            structure_sequence: 0,
            structure_origin: 0,
            target_chains: Default::default(),
        };
        for end_of_frame in [false, true] {
            qualified.descriptor.end_of_frame = end_of_frame;
            for target in 0..3 {
                assert_eq!(
                    spatial_filtered_marker(&qualified, Some(&structure), target),
                    Some(end_of_frame && target == ending_target),
                );
            }
        }
    }
}

#[test]
fn wan_vp9_entry_waits_for_the_selected_picture_end() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication::{NotPresent, Switch},
        DependencyDescriptor, FrameDependencyTemplate, TemplateDependencyStructure,
    };
    let emission = Emission::new(Profile::Vp9, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let destination = SlotMap::with_key().insert(());
    let low = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let high = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(1).unwrap())
        .unwrap();
    let base = FrameDependencyTemplate {
        dtis: [Switch, Switch].into_iter().collect(),
        ..Default::default()
    };
    let upper = FrameDependencyTemplate {
        spatial_id: 1,
        dtis: [NotPresent, Switch].into_iter().collect(),
        frame_diffs: [1].into_iter().collect(),
        ..Default::default()
    };
    let structure = TemplateDependencyStructure {
        decode_target_count: 2,
        templates: [base.clone(), upper.clone()].into_iter().collect(),
        ..Default::default()
    };
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    let mut offer = |fanout: &mut SourceFanout, index, frame_dependencies| {
        let position = 100 + u32::from(index);
        let packet = write_dd_test_packet(
            &mut writer,
            Profile::Vp9,
            position,
            9000,
            &DependencyDescriptor {
                start_of_frame: true,
                end_of_frame: true,
                template_id: index,
                frame_number: 10 + u16::from(index),
                attached_structure: (index == 0).then(|| Box::new(structure.clone())),
                frame_dependencies,
                ..Default::default()
            },
        );
        fanout
            .offer_bound_network(now, now, u64::from(position), NonZeroU64::MIN, packet, None)
            .unwrap();
    };
    let take = |fanout: &mut SourceFanout, branch| {
        fanout
            .admit_with_evidence(
                now,
                branch,
                destination,
                |_| true,
                |sequence, _, evidence| {
                    Ok::<_, std::convert::Infallible>((sequence, evidence.marker))
                },
            )
            .unwrap()
    };
    offer(&mut fanout, 0, base);
    assert_eq!(take(&mut fanout, low), Some((100, true)));
    assert_eq!(take(&mut fanout, high), None);
    offer(&mut fanout, 1, upper);
    assert_eq!(take(&mut fanout, high), Some((100, false)));
    assert_eq!(take(&mut fanout, high), Some((101, true)));
    assert_eq!(take(&mut fanout, low), None);
}

#[test]
fn wan_projection_flushes_unproved_or_oversized_runs() {
    use crate::{
        link::wan_host::SendFixture,
        route::fabric::{relay::SendProjection, target_demand::Projection},
    };
    use pulsebeam_core::{
        dd::{
            DecodeTargetIndication::NotPresent, DependencyDescriptor, FrameDependencyTemplate,
            evidence::TargetSet,
        },
        fabric::{Extension, MAX_WAN_RTP_BYTES, wan::Window},
    };
    use std::num::{NonZeroU32, NonZeroUsize};
    let now = Instant::now();
    let dd = ExtensionKind::DependencyDescriptor;
    let window = Window::new(Profile::Vp9, 0, 1, NonZeroU32::MIN, dd.bit()).unwrap();
    let (mut fixture, _, tx) =
        SendFixture::with_capacity(window, now + MAX_AGE, NonZeroUsize::new(4).unwrap());
    let proof = ProjectionEvidence::new(
        &Qualified {
            descriptor: DependencyDescriptor {
                frame_dependencies: FrameDependencyTemplate {
                    dtis: [NotPresent].into_iter().collect(),
                    ..Default::default()
                },
                ..Default::default()
            },
            active_targets: 1,
            structure_origin: 90,
            structure_sequence: 91,
            target_chains: Default::default(),
        },
        FrameStatus::Resolved(10),
    )
    .unwrap();
    let demand = |attachment| {
        Projection::targets(
            pulsebeam_core::dd::evidence::ProjectionContext {
                active_targets: 1,
                attachment,
            },
            TargetSet::new(1).unwrap(),
        )
    };
    let requested = demand(90);
    let mut projection = SendProjection::default();
    let mut admissions = LiveAdmissions::default();
    let mut offer = |position, excluded: bool, length, requested| {
        let packet = FabricPacket::new(
            Header {
                profile: Profile::Vp9,
                marker: true,
                sequence: u32::try_from(position).unwrap(),
                timestamp: 9000,
            },
            &[Extension {
                kind: dd,
                value: &[0xc0, 0, 0],
            }],
            vec![1; length].into_boxed_slice(),
        )
        .unwrap();
        let work = LiveWork::new(position, position, now, packet.remaining_hops()).unwrap();
        let transit = Transit {
            work,
            packet: packet.arrival_view(packet.remaining_hops()).unwrap(),
            projection: excluded.then_some(proof),
            h264_base: BasePacket::Unknown,
            ready_at: None,
            sender_ntp: None,
        };
        let mut report = Offers::default();
        projection.offer(now, &tx, &mut admissions, requested, &transit, &mut report);
        report
    };
    offer(100, false, 100, requested);
    offer(101, true, 100, requested);
    offer(101, true, 100, requested);
    assert_eq!(fixture.take(), Some((100, None)));
    assert_eq!(fixture.take(), None);
    assert_eq!(offer(102, false, 100, requested).omitted, 1);
    assert_eq!(fixture.take(), Some((102, OmissionRun::new(1))));

    offer(103, true, 100, requested);
    assert_eq!(offer(105, false, 100, requested).omitted, 0);
    assert_eq!(fixture.take(), Some((103, None)));
    assert_eq!(fixture.take(), Some((105, None)));

    offer(106, true, 100, requested);
    assert_eq!(
        offer(107, false, MAX_WAN_RTP_BYTES - 20, requested).omitted,
        0
    );
    assert_eq!(fixture.take(), Some((106, None)));
    assert_eq!(fixture.take(), Some((107, None)));

    offer(108, true, 100, requested);
    assert_eq!(offer(109, false, 100, None).omitted, 0);
    assert_eq!(fixture.take(), Some((108, None)));
    assert_eq!(fixture.take(), Some((109, None)));
    assert_eq!(fixture.take(), None);

    for position in 110..114 {
        assert_eq!(offer(position, false, 100, requested).enqueued, 1);
    }
    offer(114, true, 100, requested);
    let saturated = offer(115, false, 100, requested);
    assert_eq!(saturated.omitted, 0);
    assert_eq!(saturated.enqueued, 0);
    for position in 110..114 {
        assert_eq!(fixture.take(), Some((position, None)));
    }
    offer(116, false, 100, requested);
    assert_eq!(fixture.take(), Some((116, None)));

    offer(117, true, 100, requested);
    assert_eq!(offer(118, false, 100, demand(91)).omitted, 1);
    assert_eq!(fixture.take(), Some((118, OmissionRun::new(1))));
    assert_eq!(fixture.take(), None);

    offer(119, true, 100, demand(91));
    assert_eq!(offer(120, false, 100, demand(92)).omitted, 0);
    assert_eq!(fixture.take(), Some((119, None)));
    assert_eq!(fixture.take(), Some((120, None)));
}

#[test]
fn wan_omission_carrier_bridges_only_the_proven_gap() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication::Switch, DependencyDescriptor, FrameDependencyTemplate,
        TemplateDependencyStructure,
    };
    let emission = Emission::new(Profile::Vp9, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let now = Instant::now();
    let primary = NonZeroU64::MIN;
    let fallback = primary.saturating_add(1);
    for (count, expected) in [(2, true), (1, false)] {
        let mut fanout = SourceFanout::new(emission);
        let mut destinations = SlotMap::with_key();
        let destination = destinations.insert(());
        let branch = fanout
            .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
            .unwrap();
        let template = FrameDependencyTemplate {
            dtis: [Switch].into_iter().collect(),
            ..Default::default()
        };
        let mut writer = DependencyDescriptorWriter::new();
        let mut image = |sequence, first: bool| {
            write_dd_test_packet(
                &mut writer,
                Profile::Vp9,
                sequence,
                sequence * 3000,
                &DependencyDescriptor {
                    start_of_frame: true,
                    end_of_frame: true,
                    frame_number: u16::try_from(sequence).unwrap(),
                    attached_structure: first.then(|| {
                        Box::new(TemplateDependencyStructure {
                            decode_target_count: 1,
                            templates: [template.clone()].into_iter().collect(),
                            ..Default::default()
                        })
                    }),
                    frame_dependencies: template.clone(),
                    ..Default::default()
                },
            )
        };
        let take = |fanout: &mut SourceFanout, at, expected| {
            assert_eq!(
                fanout
                    .admit_with_evidence(
                        at,
                        branch,
                        destination,
                        |_| true,
                        |source, _, output| { Ok::<_, ()>((source, output.output_position)) }
                    )
                    .unwrap(),
                expected
            );
        };
        let entry = image(100, true);
        assert_eq!(
            fanout
                .offer_bound_network(now, now, 100, primary, entry, None)
                .unwrap()
                .1
                .enqueued,
            1
        );
        take(&mut fanout, now, Some((100, 100)));
        let carrier = image(103, false);
        let report = fanout
            .offer_bound_network(
                now,
                now,
                103,
                primary,
                carrier.clone(),
                OmissionRun::new(count),
            )
            .unwrap()
            .1;
        assert_eq!(report.enqueued, usize::from(expected));
        take(&mut fanout, now, expected.then_some((103, 101)));
        let after_wait = now + DD_REORDER_WAIT;
        assert_eq!(
            fanout
                .offer_bound_network(after_wait, after_wait, 103, fallback, carrier, None)
                .unwrap()
                .1
                .enqueued,
            0
        );
        take(&mut fanout, after_wait, None);
        assert!(!fanout.has_media_position(101));
        assert!(!fanout.has_media_position(102));
    }
}

#[test]
fn protected_h264_whole_branch_compacts_only_uncommitted_proven_positions() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout.install_branch(destination, emission).unwrap();
    let mut writer = DependencyDescriptorWriter::new();
    let now = Instant::now();
    let primary = NonZeroU64::MIN;
    let standby = primary.saturating_add(1);
    let take = |fanout: &mut SourceFanout, at| {
        fanout
            .admit_with_evidence(
                at,
                branch,
                destination,
                |_| true,
                |source, _, output| Ok::<_, ()>((source, output.output_position)),
            )
            .unwrap()
    };
    let first = dd_switch_packet(&mut writer, 100, 10, true);
    fanout
        .offer_bound_network(now, now, 100, primary, first, None)
        .unwrap();
    assert_eq!(take(&mut fanout, now), Some((100, 100)));
    let enhancement = dd_test_packet(&mut writer, 101, 11, false, true);
    fanout
        .offer_bound_network(now, now, 101, primary, enhancement, None)
        .unwrap();
    assert_eq!(take(&mut fanout, now), Some((101, 101)));

    let carrier = dd_switch_packet(&mut writer, 103, 13, false);
    fanout
        .offer_bound_network(now, now, 103, standby, carrier.clone(), OmissionRun::new(2))
        .unwrap();
    assert_eq!(take(&mut fanout, now), Some((103, 102)));
    fanout
        .offer_bound_network(now, now, 103, primary, carrier, None)
        .unwrap();
    assert_eq!(take(&mut fanout, now), None);

    // Keep the real loss at position 104, even though 105 was omitted.
    let partial = dd_switch_packet(&mut writer, 106, 16, true);
    fanout
        .offer_bound_network(now, now, 106, primary, partial.clone(), None)
        .unwrap();
    assert_eq!(take(&mut fanout, now), None);
    fanout
        .offer_bound_network(now, now, 106, standby, partial.clone(), OmissionRun::new(1))
        .unwrap();
    assert_eq!(take(&mut fanout, now), None);
    let after_wait = now + DD_REORDER_WAIT;
    fanout
        .offer_bound_network(after_wait, after_wait, 106, primary, partial, None)
        .unwrap();
    assert_eq!(take(&mut fanout, after_wait), Some((106, 104)));
    let next = dd_switch_packet(&mut writer, 108, 18, false);
    fanout
        .offer_bound_network(
            after_wait,
            after_wait,
            108,
            standby,
            next,
            OmissionRun::new(1),
        )
        .unwrap();
    assert_eq!(take(&mut fanout, after_wait), Some((108, 105)));

    // A late independent repair fills the reserved RTP slot without applying
    // a later compaction to it or reopening an authenticated omission.
    let repair = dd_switch_packet(&mut writer, 104, 14, false);
    fanout
        .offer_bound_network(after_wait, after_wait, 104, standby, repair.clone(), None)
        .unwrap();
    assert_eq!(take(&mut fanout, after_wait), Some((104, 103)));
    fanout
        .offer_bound_network(after_wait, after_wait, 104, primary, repair, None)
        .unwrap();
    let omitted = dd_test_packet(&mut writer, 105, 15, false, true);
    fanout
        .offer_bound_network(after_wait, after_wait, 105, primary, omitted, None)
        .unwrap();
    assert_eq!(take(&mut fanout, after_wait), None);
}

#[test]
fn ordered_filter_compacts_only_qualified_omissions() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    for (sequence, attach, omit) in [(100, true, false), (101, false, true), (102, false, false)] {
        let packet = dd_test_packet(&mut writer, u32::from(sequence), sequence, attach, omit);
        let report = fanout.offer(now, now, u64::from(sequence), packet).unwrap();
        assert_eq!(report.omitted, usize::from(omit));
        let selected_bytes = if omit { 0 } else { 6 };
        assert_eq!(
            report.dd_sample,
            DecodeTargetSample::Measured {
                context: DecodeTargetRateContext {
                    structure_origin: 100,
                    active_targets: 1
                },
                through: u64::from(sequence),
                bytes: [selected_bytes, 0, 0],
                pictures: [0; 3],
            }
        );
    }
    let take = |fanout: &mut SourceFanout, at, expected| {
        assert_eq!(
            fanout
                .admit_with_evidence(
                    at,
                    branch,
                    destination,
                    |_| true,
                    |position, _, evidence| {
                        Ok::<_, std::convert::Infallible>((position, evidence.output_position))
                    }
                )
                .unwrap(),
            Some(expected)
        );
    };
    take(&mut fanout, now, (100, 100));
    take(&mut fanout, now, (102, 101));

    // Advance past an unknown, not deliberately omitted, position.
    let packet = dd_test_packet(&mut writer, 104, 104, true, false);
    fanout.offer(now, now, 104, packet.clone()).unwrap();
    let later = now + DD_REORDER_WAIT;
    fanout.offer(later, now, 104, packet).unwrap();
    take(&mut fanout, later, (104, 103));
}

#[test]
fn wan_rtp_admission_matrix_distinguishes_whole_stream_and_dd() {
    // Admission only. Native decoder and switch proofs belong to the simulator.
    for (profile, bare, with_dd) in [
        (Profile::Opus, true, true),
        (Profile::H264, true, true),
        (Profile::Vp8, true, false),
        (Profile::Vp9, false, true),
        (Profile::Av1, true, true),
        (Profile::Data, false, false),
    ] {
        assert_eq!(
            Emission::new(profile, &[]).unwrap().supports_wan_rtp(),
            bare,
            "{profile:?} without DD"
        );
        assert_eq!(
            Emission::new(profile, &[ExtensionKind::DependencyDescriptor])
                .unwrap()
                .supports_wan_rtp(),
            with_dd,
            "{profile:?} with DD",
        );
    }
}

#[test]
fn required_av1_dd_is_not_inferred_from_the_extension_mask() {
    let optional = Emission::new(Profile::Av1, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let required = optional.with_required_av1_dd().unwrap();
    assert!(optional.carries_video_dd());
    assert!(!optional.selects_dd_encoding());
    assert!(required.selects_dd_encoding());
    assert!(optional.supports_wan_rtp());
    assert!(!required.supports_wan_rtp());
    let (profile, extensions) = required.wire_profile();
    assert_eq!(
        Emission::from_wire_profile(profile, extensions, MAJOR, REGISTRY_REVISION),
        Some(optional)
    );
}

#[test]
fn one_publication_projects_independent_targets_across_wrap() {
    for profile in [Profile::H264, Profile::Vp8, Profile::Vp9, Profile::Av1] {
        prove_independent_decode_targets(profile);
    }
}

fn prove_independent_decode_targets(profile: Profile) {
    use pulsebeam_core::dd::{
        DecodeTargetIndication::{NotPresent, Switch},
        DependencyDescriptor, FrameDependencyTemplate, TemplateDependencyStructure,
        output::Mode,
    };
    let spatial = matches!(profile, Profile::Vp9 | Profile::Av1);
    let timestamp = |seq: u32| {
        let picture = if spatial && seq == 102 {
            1
        } else {
            seq.saturating_sub(100)
        };
        9000_u32.saturating_add(picture.saturating_mul(3000))
    };
    let emission = Emission::new(profile, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let whole = destinations.insert(());
    let low = destinations.insert(());
    let high = destinations.insert(());
    let whole_branch = fanout.install_branch(whole, emission).unwrap();
    let low_branch = fanout
        .install_filtered_branch(low, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let high_branch = fanout
        .install_filtered_branch(high, emission, DecodeTarget::new(1).unwrap())
        .unwrap();
    let templates = [[Switch, Switch], [Switch, NotPresent], [NotPresent, Switch]]
        .into_iter()
        .enumerate()
        .map(|(index, dtis)| FrameDependencyTemplate {
            spatial_id: u8::from(spatial && index == 2),
            temporal_id: if spatial {
                u8::from(index == 1)
            } else {
                u8::try_from(index).unwrap()
            },
            dtis: dtis.into_iter().collect(),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    for (seq, index) in (100_u32..).zip([0, 1, 2, 0]) {
        let dd = DependencyDescriptor {
            start_of_frame: true,
            end_of_frame: true,
            template_id: u8::try_from(index).unwrap(),
            frame_number: (u16::MAX - 1)
                .wrapping_add(u16::try_from(seq.saturating_sub(100)).unwrap()),
            frame_dependencies: templates[index].clone(),
            attached_structure: (seq == 100).then(|| {
                Box::new(TemplateDependencyStructure {
                    decode_target_count: 2,
                    templates: templates.iter().cloned().collect(),
                    ..Default::default()
                })
            }),
            ..Default::default()
        };
        let packet = write_dd_test_packet(&mut writer, profile, seq, timestamp(seq), &dd);
        fanout.offer(now, now, u64::from(seq), packet).unwrap();
    }
    for (branch, destination, mode, expected) in [
        (whole_branch, whole, Mode::Whole, &[100, 101, 102, 103][..]),
        (low_branch, low, Mode::Filtered(0), &[100, 101, 103][..]),
        (high_branch, high, Mode::Filtered(1), &[100, 102, 103][..]),
    ] {
        for (projected, &source) in (100..).zip(expected) {
            assert_eq!(
                fanout.admit_with_evidence(
                    now,
                    branch,
                    destination,
                    |_| true,
                    |sequence, packet, evidence| {
                        assert_eq!(
                            packet.header().timestamp,
                            timestamp(u32::try_from(sequence).unwrap())
                        );
                        assert_eq!(evidence.mode, mode);
                        let (number, dd) = evidence.dd.unwrap();
                        assert_eq!(
                            number,
                            u64::from(u16::MAX - 1).saturating_add(sequence.saturating_sub(100))
                        );
                        assert_eq!(
                            dd.descriptor.frame_dependencies.spatial_id,
                            u8::from(spatial && sequence == 102)
                        );
                        Ok::<_, ()>((sequence, evidence.output_position))
                    }
                ),
                Ok(Some((source, projected)))
            );
        }
        assert!(fanout.take(now, branch, destination).is_none());
    }
}

#[test]
fn same_encoding_can_stage_one_distinct_decode_target_per_destination() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let target = |index| DecodeTarget::new(index).unwrap();
    let first = fanout
        .install_filtered_branch(destination, emission, target(0))
        .unwrap();
    assert!(fanout.install_branch(destination, emission).is_none());
    assert!(
        fanout
            .install_filtered_branch(destination, emission, target(0))
            .is_none()
    );
    let staged = fanout
        .install_filtered_branch(destination, emission, target(1))
        .unwrap();
    assert_eq!(fanout.branch_target(first), Some(target(0)));
    assert_eq!(fanout.branch_target(staged), Some(target(1)));
    assert!(
        fanout
            .install_filtered_branch(destination, emission, target(2))
            .is_none()
    );
    assert!(fanout.retire_branch(staged));
    assert_eq!(fanout.branch_target(staged), None);
    assert_eq!(fanout.branch_target(first), Some(target(0)));
}

#[test]
fn vp9_member_can_stage_whole_or_one_distinct_decode_target_without_replacing_the_other() {
    let emission = Emission::new(Profile::Vp9, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let member = destinations.insert(());
    let other = destinations.insert(());
    let low = DecodeTarget::new(0).unwrap();
    let high = DecodeTarget::new(2).unwrap();
    let whole = fanout.install_branch(member, emission).unwrap();
    let staged = fanout
        .install_filtered_branch(member, emission, low)
        .unwrap();
    assert_eq!(fanout.branch_for_target(member, None), Some(whole));
    assert_eq!(fanout.branch_for_target(member, Some(low)), Some(staged));
    assert_eq!(fanout.branch_for_target(other, Some(low)), None);
    assert!(fanout.install_branch(member, emission).is_none());
    assert!(
        fanout
            .install_filtered_branch(member, emission, high)
            .is_none()
    );
    assert!(fanout.retire_branch(staged));
    let committed = fanout
        .install_filtered_branch(member, emission, high)
        .unwrap();
    assert!(fanout.retire_branch(whole));
    assert!(
        fanout
            .install_filtered_branch(member, emission, high)
            .is_none()
    );
    let fallback = fanout.install_branch(member, emission).unwrap();
    assert_eq!(
        fanout.branch_for_target(member, Some(high)),
        Some(committed)
    );
    assert_eq!(fanout.branch_for_target(member, None), Some(fallback));
    fanout.retain_destinations(|destination| destination != member);
    assert_eq!(fanout.branch_for_target(member, Some(high)), None);
    assert_eq!(fanout.branch_for_target(member, None), None);
}

#[test]
fn viewer_demand_omits_a_suspended_target_only_with_its_staged_replacement() {
    use super::super::target_demand::Projection;
    use pulsebeam_core::dd::{
        DecodeTargetIndication::Switch, DependencyDescriptor, FrameDependencyTemplate,
        TemplateDependencyStructure,
    };
    let emission = Emission::new(Profile::Vp9, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let frame = FrameDependencyTemplate {
        dtis: [Switch, Switch].into_iter().collect(),
        ..Default::default()
    };
    let structure = TemplateDependencyStructure {
        decode_target_count: 2,
        templates: [frame.clone()].into_iter().collect(),
        ..Default::default()
    };
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    let mut keyframe = |position: u16| {
        write_dd_test_packet(
            &mut writer,
            Profile::Vp9,
            u32::from(position),
            9000 * u32::from(position),
            &DependencyDescriptor {
                start_of_frame: true,
                end_of_frame: true,
                frame_number: position,
                attached_structure: Some(Box::new(structure.clone())),
                frame_dependencies: frame.clone(),
                ..Default::default()
            },
        )
    };
    fanout.offer(now, now, 100, keyframe(100)).unwrap();
    let destination = SlotMap::<ParticipantHandle, _>::with_key().insert(());
    let high = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(1).unwrap())
        .unwrap();
    let base = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let demand =
        |fanout: &SourceFanout, allowed_targets| match fanout.viewer_projection(|_, branch| {
            if branch == high {
                ViewerDemand::Staged {
                    replacement: base,
                    allowed_targets,
                }
            } else {
                ViewerDemand::Include
            }
        }) {
            Some(Projection::Targets { targets, .. }) => targets.mask(),
            other => panic!("expected targeted demand: {other:?}"),
        };
    assert_eq!(demand(&fanout, None), 0b01);
    fanout.offer(now, now, 101, keyframe(101)).unwrap();
    assert_eq!(demand(&fanout, None), 0b11);
    assert_eq!(demand(&fanout, Some(0b01)), 0b01);
    assert_eq!(demand(&fanout, Some(0b00)), 0b11);
    fanout.branches[high]
        .filter
        .as_mut()
        .unwrap()
        .lose_context();
    assert!(fanout.retire_branch(base));
    assert_eq!(demand(&fanout, Some(0b01)), 0b10);
}

#[test]
fn filtered_only_fanout_qualifies_reordered_dd_in_source_order() {
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let mut fanout = SourceFanout::new(emission);
    let mut destinations = SlotMap::with_key();
    let destination = destinations.insert(());
    let branch = fanout
        .install_filtered_branch(destination, emission, DecodeTarget::new(0).unwrap())
        .unwrap();
    let now = Instant::now();
    let mut writer = DependencyDescriptorWriter::new();
    let first = dd_switch_packet(&mut writer, 100, 10, true);
    let second = dd_switch_packet(&mut writer, 101, 11, true);
    let third = dd_switch_packet(&mut writer, 102, 12, false);
    let offer = |fanout: &mut SourceFanout, packet: OwnedPacket| {
        let position = u64::from(packet.header().sequence);
        fanout.offer(now, now, position, packet).unwrap()
    };
    assert_eq!(offer(&mut fanout, first).enqueued, 1);
    assert_eq!(
        fanout
            .take(now, branch, destination)
            .unwrap()
            .header()
            .sequence,
        100
    );
    assert_eq!(offer(&mut fanout, third).enqueued, 0);
    assert!(fanout.take(now, branch, destination).is_none());
    assert_eq!(offer(&mut fanout, second).enqueued, 2);
    for position in 101..=102 {
        assert_eq!(
            fanout
                .take(now, branch, destination)
                .unwrap()
                .header()
                .sequence,
            position
        );
    }
    assert!(fanout.take(now, branch, destination).is_none());

    let held = fanout
        .offer(now, now, 104, dd_switch_packet(&mut writer, 104, 14, false))
        .unwrap();
    assert_eq!(held.enqueued, 0);
    let later = now + DD_REORDER_WAIT;
    let recovered = fanout
        .offer(
            later,
            later,
            105,
            dd_switch_packet(&mut writer, 105, 15, true),
        )
        .unwrap();
    assert_eq!(recovered.suspended, 1);
    assert_eq!(recovered.enqueued, 1);
    assert_eq!(
        fanout
            .take(later, branch, destination)
            .unwrap()
            .header()
            .sequence,
        105
    );
    assert!(fanout.take(later, branch, destination).is_none());
}

#[test]
fn expiry_and_branch_incarnation_fence_queued_work() {
    let mut fanout = SourceFanout::new(emission());
    let mut destinations = SlotMap::with_key();
    let receiver = destinations.insert(());
    let stranger = destinations.insert(());
    let original = fanout.install_branch(receiver, emission()).unwrap();
    let now = Instant::now();
    fanout.offer(now, now, 1, packet(1, b"old")).unwrap();
    assert!(fanout.take(now, original, stranger).is_none());
    assert!(fanout.retire_branch(original));
    let replacement = fanout.install_branch(receiver, emission()).unwrap();
    assert_ne!(original, replacement);
    assert!(fanout.take(now, original, receiver).is_none());
    assert!(fanout.take(now, replacement, receiver).is_none());
    fanout.offer(now, now, 2, packet(2, b"current")).unwrap();
    let expired = now.checked_add(MAX_AGE).unwrap();
    assert!(fanout.take(expired, replacement, receiver).is_none());
    assert_eq!(
        fanout.offer(expired, expired, 2, packet(2, b"current")),
        Err(Rejection::Stale)
    );
    fanout.retain_destinations(|_| false);
    assert!(fanout.take(now, replacement, receiver).is_none());
}

#[test]
fn exact_content_conflict_cancels_all_queued_work() {
    let mut fanout = SourceFanout::new(emission());
    let receiver = SlotMap::with_key().insert(());
    let branch = fanout.install_branch(receiver, emission()).unwrap();
    let now = Instant::now();
    fanout.offer(now, now, 1, packet(1, b"frozen")).unwrap();
    assert_eq!(
        fanout.offer(now, now, 1, packet(1, b"changed")),
        Err(Rejection::Conflict)
    );
    assert!(fanout.take(now, branch, receiver).is_none());
    assert!(fanout.install_branch(receiver, emission()).is_none());
    assert_eq!(
        fanout.offer(now, now, 2, packet(2, b"new")),
        Err(Rejection::Fenced)
    );
}

#[test]
fn local_zero_hop_retains_the_actual_arrival_budget() {
    let mut fanout = SourceFanout::new(emission());
    let receiver = SlotMap::with_key().insert(());
    let now = Instant::now();
    let original = packet(1, b"opaque");
    fanout.offer(now, now, 1, original.clone()).unwrap();
    let branch = fanout.install_branch(receiver, emission()).unwrap();
    let attempt = original.arrival_view(0).unwrap().into_owned();
    assert_eq!(attempt.remaining_hops(), 0);
    assert_eq!(fanout.offer(now, now, 1, attempt).unwrap().enqueued, 1);
    let received = fanout.take(now, branch, receiver).unwrap();
    assert_eq!(received.remaining_hops(), 0);
    assert_eq!(received.payload(), original.payload());
}
