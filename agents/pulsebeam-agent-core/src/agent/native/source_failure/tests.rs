use super::*;
use crate::tests::{
    RunReconciliation,
    native_connection::{connected, observe},
};
use crate::{ResourceCommand, ResourceResult, TopicMode, TrackSpec};
use state::codec;

fn receive(agent: &mut Agent, room: &str, participant: &str, delivery: TopicMode) -> u64 {
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::ReceiveData {
            spec: TrackSpec::Data {
                track_id: "events".into(),
                delivery,
            },
            source: crate::TrackSelector {
                room: room.into(),
                participant_id: participant.into(),
                track_id: "events".into(),
            },
        })
        .unwrap()
    else {
        panic!("receive owner")
    };
    owner
}

fn take_lookup(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> (u64, u64, Box<state::ResourceSpec>) {
    while let Some(effect) = agent.next_effect_after_turn() {
        match effect {
            Effect::Timer(_) => {}
            Effect::DataChannel(DataChannelEffect::Send {
                operation, payload, ..
            }) => {
                agent
                    .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
                        operation,
                        generation,
                        channel,
                    }))
                    .unwrap();
                for record in codec::decode_client(&payload).unwrap().records {
                    let Some(desired) = record.desired else {
                        continue;
                    };
                    let Some(state::desired_state::Value::Present(spec)) = desired.value else {
                        continue;
                    };
                    let Some(state::resource_spec::Declaration::LogicalSubscription(lookup)) =
                        &spec.declaration
                    else {
                        continue;
                    };
                    let address = lookup.source.as_ref().unwrap();
                    if address.room == "room" && address.participant_id == "alice" {
                        return (record.owned, desired.revision, spec);
                    }
                }
            }
            effect => panic!("physical work before source evaluation: {effect:?}"),
        }
    }
    panic!("source lookup request")
}

fn reply(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    reference: u64,
    confirmed: state::ConfirmedState,
) {
    observe(
        agent,
        generation,
        channel,
        vec![state::ServerStateRecord {
            key: Some(state::ResourceKey {
                key: Some(state::resource_key::Key::Owned(reference)),
            }),
            confirmed: Some(confirmed),
        }],
    );
}

fn read_error(agent: &mut Agent, owner: u64) -> ErrorInfo {
    let error = agent
        .resource(ResourceCommand::ReadData { owner })
        .unwrap_err();
    ErrorInfo::from(&error)
}

fn assert_waiting(agent: &mut Agent, owner: u64) {
    assert_eq!(
        agent.resource(ResourceCommand::ReadData { owner }).unwrap(),
        ResourceResult::Event { event: None }
    );
}

#[test]
fn rejected_exact_source_fails_only_its_cohort_with_a_durable_typed_cause() {
    for (failure, expected) in [
        (
            state::FailureCode::PermissionDenied,
            ErrorCode::PermissionDenied,
        ),
        (
            state::FailureCode::CapacityExceeded,
            ErrorCode::CapacityExceeded,
        ),
        (state::FailureCode::Unsupported, ErrorCode::Unsupported),
        (state::FailureCode::Incompatible, ErrorCode::Unsupported),
        (
            state::FailureCode::InvalidConfiguration,
            ErrorCode::InvalidArgument,
        ),
        (state::FailureCode::ResourceGone, ErrorCode::Closed),
        (state::FailureCode::InternalFailure, ErrorCode::Internal),
    ] {
        let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
        let first = receive(&mut agent, "room", "alice", TopicMode::Ordered);
        let second = receive(&mut agent, "room", "alice", TopicMode::Latest);
        let bob = receive(&mut agent, "room", "bob", TopicMode::Ordered);
        let other_room = receive(&mut agent, "other", "alice", TopicMode::Ordered);
        let (reference, revision, _) = take_lookup(&mut agent, generation, channel);
        reply(
            &mut agent,
            generation,
            channel,
            reference,
            state::ConfirmedState {
                evaluated_revision: revision,
                rejection: Some(state::Failure {
                    code: failure as i32,
                    diagnostic: "credential-secret-must-not-escape".into(),
                }),
                value: Some(state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::CreateRejected as i32,
                })),
            },
        );
        let first_state = agent.data_subscription_state(first).unwrap();
        let other_state = agent.data_subscription_state(other_room).unwrap();
        assert_eq!(
            first_state.source_status,
            crate::ffi::SubscriptionSourceStatus::Rejected
        );
        assert_eq!(
            first_state.error.as_ref().map(|value| value.code),
            Some(expected)
        );
        assert_eq!(other_state.selector.unwrap().room, "other");
        assert_eq!(
            other_state.source_status,
            crate::ffi::SubscriptionSourceStatus::Pending
        );
        assert_eq!(other_state.error, None);
        let error = read_error(&mut agent, first);
        assert_eq!(error.code, expected);
        assert!(!error.message.contains("credential-secret"));
        assert_eq!(read_error(&mut agent, second), error);
        assert_eq!(read_error(&mut agent, first), error);
        assert_waiting(&mut agent, bob);
        assert_waiting(&mut agent, other_room);
        let mut ready = Vec::new();
        while let Some(notification) = agent.next_notification() {
            match notification {
                Notification::DataReady { owners } => ready.extend(owners),
                Notification::ServerError(message) => {
                    assert!(!message.contains("credential-secret"));
                }
                _ => {}
            }
        }
        assert_eq!(ready, vec![first, second]);
        assert_eq!(agent.snapshot().topics.subscribers.len(), 2);
    }
}

#[test]
fn data_set_dispatch_checks_each_owner_and_stops_at_withdrawn_membership() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::OpenDataSet {
            room: "room".into(),
            filter: crate::TrackFilter {
                track_id: Some("events".into()),
                ..Default::default()
            },
        })
        .unwrap()
    else {
        panic!("data set owner")
    };
    let Effect::DataChannel(DataChannelEffect::Send {
        operation, payload, ..
    }) = agent.next_effect_after_turn().expect("selector request")
    else {
        panic!("selector request")
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation,
            generation,
            channel,
        }))
        .unwrap();
    let mut batch = codec::decode_client(&payload).unwrap();
    let record = batch.records.pop().unwrap();
    let desired = record.desired.unwrap();
    let Some(state::desired_state::Value::Present(spec)) = desired.value else {
        panic!("selector declaration")
    };
    let address = state::LogicalSourceAddress {
        room: "room".into(),
        participant_id: "alice".into(),
        kind: state::TrackKind::Data as i32,
        track_id: "events".into(),
    };
    let report = |revision, matches: Vec<state::LogicalSourceAddress>| state::ConfirmedState {
        evaluated_revision: desired.revision,
        rejection: None,
        value: Some(state::confirmed_state::Value::SubscriptionSelector(
            Box::new(state::SubscriptionSelectorState {
                spec: Some((*spec).clone()),
                status: state::SelectorMatchStatus::Complete as i32,
                membership: Some(state::SelectorMembershipSpan {
                    revision,
                    offset: 0,
                    total: matches.len() as u32,
                }),
                matches,
            }),
        )),
    };
    let deliver = |agent: &mut Agent, participant: &str, sequence| {
        Agent::deliver_completed_data(
            &mut agent.resources,
            &agent.selectors,
            &agent.active.as_ref().unwrap().state,
            &mut agent.notifications,
            &crate::TopicSubscriber {
                room: "room".into(),
                participant_id: participant.into(),
                topic: "events".into(),
                mode: TopicMode::Ordered,
            },
            "publication",
            crate::data_v1::AssembledUnit {
                unit_seq: sequence,
                captured_at: None,
                bytes: vec![42],
                fragment_descriptors: vec![],
            },
        );
    };
    deliver(&mut agent, "alice", 0);
    assert_waiting(&mut agent, owner);
    reply(
        &mut agent,
        generation,
        channel,
        record.owned,
        report(1, vec![address]),
    );
    deliver(&mut agent, "bob", 1);
    assert_waiting(&mut agent, owner);
    deliver(&mut agent, "alice", 2);
    reply(
        &mut agent,
        generation,
        channel,
        record.owned,
        report(2, vec![]),
    );
    deliver(&mut agent, "alice", 3);
    let mut read = || agent.resource(ResourceCommand::ReadData { owner }).unwrap();
    assert!(matches!(
        read(),
        ResourceResult::Event {
            event: Some(crate::DataEvent::Message { sequence: 2, .. }),
        }
    ));
    assert!(matches!(read(), ResourceResult::Event {
        event: Some(crate::DataEvent::SourceFailed { publication_id, .. }),
    } if publication_id == "publication"));
    assert_eq!(read(), ResourceResult::Event { event: None });
}

#[test]
fn inherited_exact_source_loses_reliable_continuity_on_confirmed_replacement() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let source = crate::TrackSelector {
        room: "room".into(),
        participant_id: "alice".into(),
        track_id: "events".into(),
    };
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::ReceiveInheritedData {
            source: source.clone(),
        })
        .unwrap()
    else {
        panic!("inherited exact owner")
    };
    assert_eq!(agent.data_subscription_state(owner).unwrap().delivery, None);
    let (reference, revision, spec) = take_lookup(&mut agent, generation, channel);
    let confirmed = |publication_id: &str, source_revision, delivery: state::DataDelivery| {
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::LogicalSubscription(
                Box::new(state::LogicalSubscriptionState {
                    spec: Some((*spec).clone()),
                    source_revision,
                    execution: None,
                    data_v1: None,
                    source: Some(state::Track {
                        publication_id: publication_id.into(),
                        participant_id: source.participant_id.clone(),
                        room_id: source.room.clone(),
                        track_id: source.track_id.clone(),
                        kind: Some(state::track::Kind::Data(state::DataTrack {
                            capture_clock: None,
                            delivery: delivery as i32,
                            record_dd: false,
                        })),
                    }),
                }),
            )),
        }
    };
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        confirmed("original", 1, state::DataDelivery::Ordered),
    );
    assert_eq!(
        agent.data_subscription_state(owner).unwrap().delivery,
        Some(crate::ffi::TopicMode::Ordered)
    );
    let registration = crate::TopicSubscriber {
        room: source.room.clone(),
        participant_id: source.participant_id.clone(),
        topic: source.track_id.clone(),
        mode: TopicMode::Ordered,
    };
    assert_eq!(
        agent.resources.receive_data_v1(
            &registration,
            "original",
            crate::data_v1::AssembledUnit {
                unit_seq: 7,
                captured_at: None,
                bytes: b"old".to_vec(),
                fragment_descriptors: vec![],
            }
        ),
        [owner]
    );
    assert!(
        matches!(agent.resource(ResourceCommand::ReadData { owner }), Ok(ResourceResult::Event {
        event: Some(crate::DataEvent::Message { publication_id, delivery: crate::ffi::DataDelivery::Reliable, .. }),
    }) if publication_id == "original")
    );
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        confirmed("replacement", 2, state::DataDelivery::Latest),
    );
    assert_eq!(
        read_error(&mut agent, owner).code,
        ErrorCode::ContinuityLost
    );
    assert!(agent.resources.inherited_data_demand().next().is_none());
}

#[test]
fn delivery_mismatch_fails_only_incompatible_receivers_and_revocation_keeps_the_first_cause() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let ordered = receive(&mut agent, "room", "alice", TopicMode::Ordered);
    let latest = receive(&mut agent, "room", "alice", TopicMode::Latest);
    let lossy = receive(&mut agent, "room", "alice", TopicMode::Lossy);
    let (reference, revision, spec) = take_lookup(&mut agent, generation, channel);
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::LogicalSubscription(
                Box::new(state::LogicalSubscriptionState {
                    spec: Some(*spec),
                    source_revision: 1,
                    execution: None,
                    data_v1: None,
                    source: Some(state::Track {
                        publication_id: "opaque-source".into(),
                        participant_id: "alice".into(),
                        room_id: "room".into(),
                        track_id: "events".into(),
                        kind: Some(state::track::Kind::Data(state::DataTrack {
                            capture_clock: None,
                            delivery: state::DataDelivery::Ordered as i32,
                            record_dd: false,
                        })),
                    }),
                }),
            )),
        },
    );
    let ordered_state = agent.data_subscription_state(ordered).unwrap();
    let latest_state = agent.data_subscription_state(latest).unwrap();
    let ordered_selector = ordered_state.selector.as_ref().unwrap();
    assert_eq!(ordered_selector.room, "room");
    assert_eq!(ordered_selector.participant_id, "alice");
    assert_eq!(
        ordered_state.source_track_id.as_deref(),
        Some("opaque-source")
    );
    assert_eq!(ordered_state.terminal_error, None);
    assert_eq!(
        latest_state.source_track_id.as_deref(),
        Some("opaque-source")
    );
    let incompatible = read_error(&mut agent, latest);
    assert_eq!(incompatible.code, ErrorCode::InvalidArgument);
    assert_eq!(
        agent
            .data_subscription_state(latest)
            .unwrap()
            .terminal_error,
        Some(incompatible.clone())
    );
    assert_eq!(read_error(&mut agent, lossy), incompatible);
    assert_waiting(&mut agent, ordered);
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::PermissionRevoked as i32,
            })),
        },
    );
    assert_eq!(
        read_error(&mut agent, ordered).code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(read_error(&mut agent, latest), incompatible);
    assert_eq!(read_error(&mut agent, lossy), incompatible);
}

#[test]
fn accepted_missing_source_remains_waiting_without_a_failed_read() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let owner = receive(&mut agent, "room", "alice", TopicMode::Ordered);
    let pending = agent.data_subscription_state(owner).unwrap();
    assert_eq!(
        pending.source_status,
        crate::ffi::SubscriptionSourceStatus::Pending
    );
    assert!(pending.admission_pending);
    assert_eq!(pending.intent_revision, 1);
    let (reference, revision, spec) = take_lookup(&mut agent, generation, channel);
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::LogicalSubscription(
                Box::new(state::LogicalSubscriptionState {
                    data_v1: None,
                    spec: Some(*spec),
                    source_revision: 0,
                    execution: None,
                    source: None,
                }),
            )),
        },
    );
    assert_waiting(&mut agent, owner);
    let unavailable = agent.data_subscription_state(owner).unwrap();
    assert_eq!(
        unavailable.source_status,
        crate::ffi::SubscriptionSourceStatus::Unavailable
    );
    assert_eq!(unavailable.source_track_id, None);
    assert_eq!(unavailable.error, None);
    assert_eq!(
        unavailable.execution,
        crate::ffi::PublicationExecution::Unavailable
    );
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
}

#[test]
fn resolved_source_description_does_not_claim_data_receiver_execution() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let owner = receive(&mut agent, "room", "alice", TopicMode::Ordered);
    let (reference, revision, spec) = take_lookup(&mut agent, generation, channel);
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::LogicalSubscription(
                Box::new(state::LogicalSubscriptionState {
                    spec: Some(*spec),
                    source_revision: 1,
                    execution: None,
                    data_v1: None,
                    source: Some(state::Track {
                        publication_id: "opaque-data-track".into(),
                        participant_id: "alice".into(),
                        room_id: "room".into(),
                        track_id: "events".into(),
                        kind: Some(state::track::Kind::Data(state::DataTrack {
                            capture_clock: None,
                            delivery: state::DataDelivery::Ordered as i32,
                            record_dd: false,
                        })),
                    }),
                }),
            )),
        },
    );
    let state = agent.data_subscription_state(owner).unwrap();
    assert_eq!(
        state.source_status,
        crate::ffi::SubscriptionSourceStatus::Available
    );
    assert_eq!(state.source_track_id.as_deref(), Some("opaque-data-track"));
    assert!(!state.admission_pending);
    assert_eq!(
        state.admission,
        crate::ffi::PublicationAdmissionStatus::Admitted
    );
    assert_eq!(
        state.execution,
        crate::ffi::PublicationExecution::Unavailable
    );
}

#[test]
fn retired_lookup_rejection_cannot_fail_a_fresh_receive_lifetime() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let old = receive(&mut agent, "room", "alice", TopicMode::Ordered);
    let (reference, revision, _) = take_lookup(&mut agent, generation, channel);
    agent
        .resource(ResourceCommand::Release { owner: old })
        .unwrap();
    let mut retiring = false;
    while let Some(effect) = agent.next_effect_after_turn() {
        if let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = effect
        {
            retiring = codec::decode_client(&payload)
                .unwrap()
                .records
                .iter()
                .any(|record| {
                    record.owned == reference
                        && matches!(
                            record
                                .desired
                                .as_ref()
                                .and_then(|desired| desired.value.as_ref()),
                            Some(state::desired_state::Value::Absent(true))
                        )
                });
            agent
                .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
                    operation,
                    generation,
                    channel,
                }))
                .unwrap();
            if retiring {
                break;
            }
        }
    }
    assert!(
        retiring,
        "old lookup retirement must be staged before the fresh lifetime"
    );
    let fresh = receive(&mut agent, "room", "alice", TopicMode::Ordered);
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        state::ConfirmedState {
            evaluated_revision: revision,
            rejection: Some(state::Failure {
                code: state::FailureCode::PermissionDenied as i32,
                diagnostic: String::new(),
            }),
            value: Some(state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::CreateRejected as i32,
            })),
        },
    );
    assert_waiting(&mut agent, fresh);
}
