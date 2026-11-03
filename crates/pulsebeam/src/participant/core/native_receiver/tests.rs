use super::super::native_data::tests::{
    confirmations, frame, participant, participant_with_expiry,
};
use super::*;
use crate::participant::{effect::TrackRemoval, event::test_utils::MockParticipantSink};
use std::num::NonZeroU64;

fn subscription(source: TrackId, channel: u32, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                track_id: source.as_str(),
                kind: wire::TrackKind::Data as i32,
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Sctp(wire::SctpParameters {
                        stream_id: Some(channel),
                    })),
                }),
            },
        )),
    }
}

fn target(
    participant: &mut Participant,
    revision: u64,
    spec: wire::ResourceSpec,
    sink: &mut MockParticipantSink,
) {
    participant.handle_native_signaling(
        &frame(
            revision,
            wire::desired_state::Value::Present(Box::new(spec)),
        ),
        sink,
    );
}

fn drain_sources(participant: &mut Participant, sink: &mut MockParticipantSink) {
    for _ in 0..16 {
        if !participant.advance_native_source(sink) {
            return;
        }
    }
    panic!("source work did not quiesce");
}

#[test]
fn receiver_binding_observations_preserve_target_rejection_and_fence_source_lifetimes() {
    let mut participant = participant();
    let mut sink = MockParticipantSink::new();
    let connection = entity::ConnectionId::new();
    let id = connection.derive_publication_track_id(
        TrackKind::Data,
        "literal",
        NonZeroU64::new(1).unwrap(),
    );
    let owner = entity::ParticipantId::new();
    let track = Track::data_publication(
        track::TrackMeta {
            room_id: participant.room_id,
            shard_id: ShardId::new(0),
            id,
            origin: owner,
            connection_id: connection,
            label: Some("literal".into()),
        },
        DataLane::Ordered,
    );
    participant
        .signaling
        .observe_participant(wire::Participant {
            participant_id: owner.as_str(),
            participant_external_id: "source".into(),
        })
        .unwrap();
    participant.signaling.observe_candidate(&track).unwrap();
    let mut handles = slotmap::SlotMap::<TrackHandle, _>::with_key();
    let handle = handles.insert(id);
    participant.native_source_added(handle, &track);
    participant.native_source_binding(id, handle, true);
    let spec = subscription(id, 1, true);
    participant
        .signaling
        .apply_input(&frame(
            1,
            wire::desired_state::Value::Present(Box::new(spec.clone())),
        ))
        .unwrap();
    let work = participant.signaling.take_preparation().unwrap().unwrap();
    participant.signaling.reserve_execution(1, 8192).unwrap();
    let (owner, descriptor) = participant.signaling.source(&id.as_str()).unwrap();
    let channel = crate::media::ChannelId::from_value(1);
    // Structural binding fixture only. The connected simulator owns the real Opened/plan witness.
    let deadline = Instant::now() + Duration::from_secs(30);
    participant.native_receivers.deadlines.insert((deadline, 1));
    participant.native_receivers.receivers.insert(
        1,
        Receiver {
            transport: ReceiveTransport::Sctp {
                channel,
                lane: DataLane::Ordered,
            },
            source: id,
            participant: owner,
            track: descriptor,
            handle: None,
            enabled: false,
            opened: false,
            reset_pending: false,
            retiring: false,
            preparation: Some(work),
            deadline: Some(deadline),
        },
    );
    participant.native_receivers.channels.insert(channel, 1);
    participant
        .native_receivers
        .source_refs
        .insert((*id.as_bytes(), 1));
    participant
        .native_receivers
        .receivers
        .get_mut(&1)
        .unwrap()
        .reset_pending = true;
    participant.native_data_closed(channel, &mut sink);
    participant.native_receiver_opened(
        1,
        crate::participant::transport::native_data_config(channel, DataLane::Ordered),
        &mut sink,
    );
    let pending = participant.native_receivers.receivers.get_mut(&1).unwrap();
    assert!(!pending.opened && !pending.retiring && pending.preparation.is_some());
    assert_eq!(pending.deadline, Some(deadline));
    pending.reset_pending = false;
    participant.native_receiver_opened(
        1,
        crate::participant::transport::native_data_config(channel, DataLane::Ordered),
        &mut sink,
    );
    drain_sources(&mut participant, &mut sink);
    assert!(participant.native_receivers.deadlines.is_empty());
    assert!(matches!(
        confirmations(&mut participant).last().unwrap().value,
        Some(wire::confirmed_state::Value::Owned(_))
    ));
    participant.expire_native_data(deadline, &mut sink);
    assert!(confirmations(&mut participant).is_empty());
    assert!(participant.native_receivers.can_write(channel));
    target(&mut participant, 2, subscription(id, 1, false), &mut sink);
    assert!(participant.native_receivers.sources[id.as_bytes()].bound);
    assert!(!participant.native_receivers.can_write(channel));
    target(&mut participant, 3, spec.clone(), &mut sink);
    assert!(participant.native_receivers.can_write(channel));
    target(&mut participant, 4, subscription(id, 2, true), &mut sink);
    participant.native_source_removed(id, TrackRemoval::Withdrawn);
    assert!(!participant.native_receivers.can_write(channel));
    drain_sources(&mut participant, &mut sink);
    let values = confirmations(&mut participant);
    let value = values.last().unwrap();
    assert_eq!(value.evaluated_revision, 4);
    assert_eq!(
        value.rejection.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert!(
        matches!(&value.value, Some(wire::confirmed_state::Value::Owned(live)) if live.spec.as_ref() == Some(&spec) && live.execution == wire::ExecutionStatus::WaitingForSource as i32)
    );
    assert!(sink.owned_unsubscribes.is_empty());
    handles.remove(handle);
    let replacement = handles.insert(id);
    participant.native_source_added(replacement, &track);
    participant.native_source_binding(id, handle, true);
    assert!(!participant.native_receivers.can_write(channel));
    participant.native_source_binding(id, replacement, true);
    drain_sources(&mut participant, &mut sink);
    assert!(participant.native_receivers.can_write(channel));
    assert!(
        !participant
            .native_receivers
            .received
            .contains(&(handle.data().as_ffi(), 1))
    );
    assert!(
        participant
            .native_receivers
            .received
            .contains(&(replacement.data().as_ffi(), 1))
    );
    let values = confirmations(&mut participant);
    assert_eq!(values.last().unwrap().evaluated_revision, 4);
    assert!(values.last().unwrap().rejection.is_some());
    participant.native_source_removed(id, TrackRemoval::SourceEnded);
    assert!(!participant.native_receivers.can_write(channel));
    drain_sources(&mut participant, &mut sink);
    assert_eq!(sink.owned_unsubscribes, vec![1]);
    assert!(participant.native_receivers.receivers[&1].retiring);
    assert!(
        confirmations(&mut participant)
            .iter()
            .all(|value| !matches!(value.value, Some(wire::confirmed_state::Value::Absent(_))))
    );
    participant.complete_native_data_cutoff(1, true);
    let values = confirmations(&mut participant);
    let value = values.last().unwrap();
    assert_eq!(value.evaluated_revision, 4);
    assert!(value.rejection.is_some());
    assert!(
        matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::SourceEnded as i32)
    );
    assert!(participant.native_receivers.receivers.is_empty());
    assert!(participant.native_receivers.source_refs.is_empty());
    assert!(participant.native_receivers.received.is_empty());
    assert!(participant.native_receivers.sources.is_empty());
    assert!(participant.disconnect_reason.is_none());
}

#[test]
fn missing_source_preparation_deadlines_expire_cancel_or_resolve_without_ending_the_source() {
    enum Outcome {
        Expire,
        Cancel,
        SourceArrives,
    }
    for outcome in [Outcome::Expire, Outcome::Cancel, Outcome::SourceArrives] {
        let mut participant = participant_with_expiry(unix_now().saturating_sub(1));
        let mut sink = MockParticipantSink::new();
        let connection = entity::ConnectionId::new();
        let id = connection.derive_publication_track_id(
            TrackKind::Data,
            "not-yet-observed",
            NonZeroU64::new(1).unwrap(),
        );
        target(&mut participant, 1, subscription(id, 1, true), &mut sink);
        assert!(participant.native_receivers.waiting.is_empty());
        assert!(participant.disconnect_reason.is_none());
        participant.input(ParticipantInput::Authorization {
            revision: 2,
            expiry: Some(i64::try_from(unix_now() + 60).unwrap()),
        });
        let before = Instant::now();
        participant.evaluate_native_preparations(&mut sink);
        let deadline = participant.native_receivers.waiting[&1].deadline;
        assert!(deadline >= before + Duration::from_secs(30));
        assert!(deadline <= Instant::now() + Duration::from_secs(30));
        participant.expire_native_data(deadline - Duration::from_nanos(1), &mut sink);
        assert!(participant.native_receivers.waiting.contains_key(&1));
        assert!(participant.native_receivers.channels.is_empty());
        assert!(sink.owned_subscriptions.is_empty());
        assert!(
            confirmations(&mut participant)
                .iter()
                .all(|value| !matches!(value.value, Some(wire::confirmed_state::Value::Owned(_))))
        );
        let rejection = match outcome {
            Outcome::Expire => {
                participant.expire_native_data(deadline, &mut sink);
                Some(wire::FailureCode::InternalFailure)
            }
            Outcome::Cancel => {
                participant.handle_native_signaling(
                    &frame(2, wire::desired_state::Value::Absent(true)),
                    &mut sink,
                );
                None
            }
            Outcome::SourceArrives => {
                let owner = entity::ParticipantId::new();
                let track = Track::data_publication(
                    track::TrackMeta {
                        room_id: participant.room_id,
                        shard_id: ShardId::new(0),
                        id,
                        origin: owner,
                        connection_id: connection,
                        label: Some("not-yet-observed".into()),
                    },
                    DataLane::Ordered,
                );
                participant
                    .signaling
                    .observe_participant(wire::Participant {
                        participant_id: owner.as_str(),
                        participant_external_id: "source".into(),
                    })
                    .unwrap();
                participant.signaling.observe_candidate(&track).unwrap();
                let mut handles = slotmap::SlotMap::<TrackHandle, _>::with_key();
                let handle = handles.insert(id);
                participant.native_source_added(handle, &track);
                drain_sources(&mut participant, &mut sink);
                // Metadata resolved; the unconnected fixture cannot open SCTP.
                Some(wire::FailureCode::Incompatible)
            }
        };
        assert!(participant.native_receivers.waiting.is_empty());
        assert!(participant.native_receivers.deadlines.is_empty());
        assert!(participant.native_receivers.source_refs.is_empty());
        assert!(sink.owned_subscriptions.is_empty());
        assert!(sink.owned_unsubscribes.is_empty());
        let values = confirmations(&mut participant);
        let value = values.last().unwrap();
        assert_eq!(
            value.rejection.as_ref().map(|failure| failure.code),
            rejection.map(|code| code as i32)
        );
        let reason = if rejection.is_some() {
            wire::AbsenceReason::CreateRejected
        } else {
            wire::AbsenceReason::Removed
        };
        assert!(
            matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == reason as i32)
        );
        participant.expire_native_data(deadline, &mut sink);
        assert!(confirmations(&mut participant).is_empty());
        assert!(participant.disconnect_reason.is_none());
    }
}
