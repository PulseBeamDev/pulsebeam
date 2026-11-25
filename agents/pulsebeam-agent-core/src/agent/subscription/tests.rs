use super::*;
use crate::ffi::{
    ErrorCode, PublicationAdmissionStatus as Admission, PublicationExecution as Execution,
    SubscriptionSourceStatus as Source,
};
use crate::tests::{
    RunReconciliation,
    native_connection::{self, connected, observe},
};
use crate::{ResourceCommand, ResourceResult, TopicMode, TrackSelector, TrackSpec};
use pulsebeam_proto::state::{self, codec};

fn data_spec() -> TrackSpec {
    TrackSpec::Data {
        track_id: "events".into(),
        delivery: TopicMode::Lossy,
    }
}

fn publication() -> (
    Agent,
    Generation,
    ChannelId,
    u64,
    native_connection::PreparedData,
) {
    let (mut agent, generation, channel) = connected(Default::default());
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::DeclareDataPublication {
            spec: data_spec(),
            capture_clock: None,
            record_dd: false,
        })
        .unwrap()
    else {
        panic!("publication owner")
    };
    assert_eq!(
        agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .map(|publisher| publisher.registration.clone())
            .collect::<Vec<_>>(),
        vec![crate::TopicPublisher {
            topic: "events".into(),
            mode: TopicMode::Lossy
        }]
    );
    let prepared = native_connection::prepare_data(&mut agent, generation, channel);
    (agent, generation, channel, owner, prepared)
}

fn reject(revision: u64, value: state::confirmed_state::Value) -> state::ConfirmedState {
    state::ConfirmedState {
        evaluated_revision: revision,
        rejection: Some(state::Failure {
            code: state::FailureCode::PermissionDenied as i32,
            diagnostic: "private diagnostic".into(),
        }),
        value: Some(value),
    }
}

fn reply(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    reference: u64,
    confirmation: state::ConfirmedState,
) {
    observe(
        agent,
        generation,
        channel,
        vec![state::ServerStateRecord {
            key: Some(state::ResourceKey {
                key: Some(state::resource_key::Key::Owned(reference)),
            }),
            confirmed: Some(confirmation),
        }],
    );
}

#[test]
fn rejected_first_data_publication_is_not_an_execution_failure() {
    let (mut agent, generation, channel, owner, prepared) = publication();
    reply(
        &mut agent,
        generation,
        channel,
        prepared.reference,
        reject(
            prepared.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::CreateRejected as i32,
            }),
        ),
    );
    let state = agent.data_publication_state(owner).unwrap();
    assert_eq!(state.admission, Admission::Rejected);
    assert!(!state.admission_pending);
    assert_eq!(state.error.unwrap().code, ErrorCode::PermissionDenied);
    assert_eq!(state.execution, Execution::Unavailable);
    assert_eq!(state.execution_error, None);
}

#[test]
fn rejected_data_publication_update_retains_admitted_execution() {
    let (mut agent, generation, channel, owner, prepared) = publication();
    native_connection::confirm_publication(
        &mut agent,
        generation,
        channel,
        &prepared,
        native_connection::SOURCE,
    );
    let mut spec = prepared.spec.clone();
    spec.settings.as_mut().unwrap().enabled = Some(false);
    let resource = Resource::Publication {
        kind: state::TrackKind::Data,
        track_id: "events".into(),
    };
    let native = &mut agent.active.as_mut().unwrap().state;
    native.stage(resource, Some(spec)).unwrap();
    let frame = codec::decode_client(&native.next_frame().unwrap().unwrap()).unwrap();
    let proposal = &frame.records[0];
    assert_eq!(proposal.owned, prepared.reference);
    let revision = proposal.desired.as_ref().unwrap().revision;
    assert!(revision > prepared.revision);
    let state::confirmed_state::Value::Owned(live) = native
        .confirmed(&Resource::Publication {
            kind: state::TrackKind::Data,
            track_id: "events".into(),
        })
        .unwrap()
        .value
        .clone()
        .unwrap()
    else {
        panic!("admitted publication");
    };
    reply(
        &mut agent,
        generation,
        channel,
        prepared.reference,
        reject(revision, state::confirmed_state::Value::Owned(live)),
    );
    let state = agent.data_publication_state(owner).unwrap();
    assert_eq!(state.admission, Admission::Admitted);
    assert!(!state.admission_pending);
    assert_eq!(state.error.unwrap().code, ErrorCode::PermissionDenied);
    assert_eq!(state.execution, Execution::Installed);
    assert_eq!(state.execution_error, None);
}

fn receive(agent: &mut Agent, participant: &str) -> u64 {
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::ReceiveData {
            spec: data_spec(),
            source: TrackSelector {
                room: "room".into(),
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

fn receiver(
    admit: bool,
) -> (
    Agent,
    Generation,
    ChannelId,
    u64,
    native_connection::PreparedData,
) {
    let (mut agent, generation, channel) = connected(Default::default());
    let owner = receive(&mut agent, "alice");
    let subscriber = agent.snapshot().topics.subscribers[0].registration.clone();
    let topics = crate::TopicRegistrations {
        publishers: vec![],
        subscribers: vec![subscriber.clone()],
    };
    native_connection::answer_source_lookup_batch(
        &mut agent,
        generation,
        channel,
        &topics,
        Some("alice-track"),
    );
    let prepared = if admit {
        native_connection::prepare_subscription(
            &mut agent,
            generation,
            channel,
            "alice-track",
            &subscriber,
        )
    } else {
        native_connection::prepare_data(&mut agent, generation, channel)
    };
    (agent, generation, channel, owner, prepared)
}

#[test]
fn rejected_first_data_subscription_is_not_an_execution_failure() {
    let (mut agent, generation, channel, owner, prepared) = receiver(false);
    reply(
        &mut agent,
        generation,
        channel,
        prepared.reference,
        reject(
            prepared.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::CreateRejected as i32,
            }),
        ),
    );
    let state = agent.data_subscription_state(owner).unwrap();
    assert_eq!(state.source_status, Source::Rejected);
    assert_eq!(state.admission, Admission::Rejected);
    assert!(!state.admission_pending);
    assert_eq!(state.error.unwrap().code, ErrorCode::PermissionDenied);
    assert_eq!(state.execution, Execution::Unavailable);
    assert_eq!(state.execution_error, None);
}

#[test]
fn closing_live_catalog_preserves_independently_owned_subscription() {
    let (mut agent, generation, channel, owner, _) = receiver(true);
    let before = agent.data_subscription_state(owner).unwrap();
    assert_eq!(before.admission, Admission::Admitted);
    assert_eq!(before.execution, Execution::Installed);
    let topics = agent.snapshot().topics.clone();
    let watch = agent
        .catalog_watch("room".into(), Default::default())
        .unwrap();
    let opened = native_connection::drain_batches(&mut agent, generation, channel);
    assert!(opened.iter().all(|batch| batch.records.is_empty()));
    let commands: Vec<_> = opened
        .into_iter()
        .flat_map(|batch| batch.catalog_records)
        .collect();
    let [
        state::CatalogClientRecord {
            command: Some(state::catalog_client_record::Command::Open(request)),
        },
    ] = commands.as_slice()
    else {
        panic!("one catalog open, got {commands:?}");
    };
    assert_eq!(request.room, "room");
    native_connection::catalog_page(&mut agent, generation, channel, request.watch_id, "room");
    assert_eq!(
        agent.catalog_watch_state(watch).unwrap().status,
        crate::ffi::WatchStatus::Live
    );
    assert_eq!(agent.data_subscription_state(owner).unwrap(), before);
    assert_eq!(
        agent.close_catalog_watch(watch).unwrap().status,
        crate::ffi::WatchStatus::Closed
    );
    let closed = native_connection::drain_batches(&mut agent, generation, channel);
    assert!(closed.iter().all(|batch| batch.records.is_empty()));
    let commands: Vec<_> = closed
        .into_iter()
        .flat_map(|batch| batch.catalog_records)
        .collect();
    assert!(matches!(commands.as_slice(), [state::CatalogClientRecord {
        command: Some(state::catalog_client_record::Command::Close(close)),
    }] if close.watch_id == request.watch_id));
    assert_eq!(agent.data_subscription_state(owner).unwrap(), before);
    assert_eq!(agent.snapshot().topics, topics);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(
        agent.snapshot().connection,
        crate::ConnectionState::Connected
    );
}

#[test]
fn failed_data_binding_does_not_reject_the_logical_admission() {
    let (mut agent, generation, channel, owner, prepared) = receiver(true);
    let resource = Resource::LogicalData(crate::TrackSelector {
        room: "room".into(),
        participant_id: "alice".into(),
        track_id: "events".into(),
    });
    let native = &agent.active.as_ref().unwrap().state;
    let state::confirmed_state::Value::LogicalSubscription(mut logical) =
        native.confirmed(&resource).unwrap().value.clone().unwrap()
    else {
        panic!("admitted receiver");
    };
    logical.execution = Some(state::logical_subscription_state::Execution::Failure(
        state::Failure {
            code: state::FailureCode::CapacityExceeded as i32,
            ..Default::default()
        },
    ));
    reply(
        &mut agent,
        generation,
        channel,
        prepared.reference,
        state::ConfirmedState {
            evaluated_revision: prepared.revision,
            rejection: None,
            value: Some(state::confirmed_state::Value::LogicalSubscription(logical)),
        },
    );
    let state = agent.data_subscription_state(owner).unwrap();
    assert_eq!(state.admission, Admission::Admitted);
    assert!(!state.admission_pending);
    assert_eq!(state.execution, Execution::Failed);
    assert_eq!(state.error, None);
    assert_eq!(
        state.execution_error.unwrap().code,
        ErrorCode::CapacityExceeded
    );
    assert_eq!(state.terminal_error, None);
}

#[test]
fn unresolved_and_denied_selector_never_inherits_another_receiver() {
    let (mut agent, generation, channel, alice, _) = receiver(true);
    let alice_state = agent.data_subscription_state(alice).unwrap();
    assert_eq!(alice_state.source_status, Source::Available);
    assert_eq!(alice_state.admission, Admission::Admitted);
    assert_eq!(alice_state.execution, Execution::Installed);
    let bob = receive(&mut agent, "bob");
    let pending = agent.data_subscription_state(bob).unwrap();
    assert_eq!(pending.source_status, Source::Pending);
    assert_eq!(pending.source_track_id, None);
    assert_eq!(pending.admission, Admission::Pending);
    assert_eq!(pending.execution, Execution::Unavailable);
    let mut lookup = None;
    while let Some(effect) = agent.next_effect_after_turn() {
        if let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = effect
        {
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
                if let Some(state::desired_state::Value::Present(spec)) = desired.value {
                    if matches!(&spec.declaration, Some(state::resource_spec::Declaration::LogicalSubscription(source)) if source.source.as_ref().is_some_and(|address| address.participant_id == "bob"))
                    {
                        lookup = Some((record.owned, desired.revision));
                    }
                }
            }
            if lookup.is_some() {
                break;
            }
        }
    }
    let (reference, revision) = lookup.expect("bob lookup");
    reply(
        &mut agent,
        generation,
        channel,
        reference,
        reject(
            revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::CreateRejected as i32,
            }),
        ),
    );
    let denied = agent.data_subscription_state(bob).unwrap();
    assert_eq!(denied.source_status, Source::Rejected);
    assert_eq!(denied.source_track_id, None);
    assert_eq!(denied.admission, Admission::Rejected);
    assert_eq!(denied.execution, Execution::Unavailable);
    assert_eq!(denied.error.unwrap().code, ErrorCode::PermissionDenied);
    assert_eq!(denied.execution_error, None);
    assert_eq!(
        agent.data_subscription_state(alice).unwrap().execution,
        Execution::Installed
    );
}

#[test]
fn terminal_session_cause_survives_subscription_observer_close() {
    let (mut agent, generation, channel, owner, _) = receiver(true);
    let initial = agent.data_subscription_state(owner).unwrap();
    assert_eq!(initial.execution, Execution::Installed);
    let observer = crate::observer::DataSubscriptionStateObserver::new(initial);

    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![native_connection::confirmed(
            state::resource_key::Key::Session(true),
            0,
            state::confirmed_state::Value::Session(state::SessionState {
                authorization: Some(state::Authorization {
                    expires_at_unix_seconds: 10_001,
                    revision: 1,
                }),
                closed: Some(state::CloseReason::ParticipantReplaced as i32),
                diagnostic: "opaque diagnostic".into(),
                successor_receipt: None,
            }),
        )],
    );
    let failed = agent.data_subscription_state(owner).unwrap();
    assert_eq!(failed.execution, Execution::Failed);
    let cause = failed
        .execution_error
        .clone()
        .expect("terminal execution cause");
    assert_eq!(cause.code, ErrorCode::Replaced);
    assert_eq!(cause.message, "Agent was replaced");
    let Err(crate::AgentError::OperationFailed(send_cause)) =
        agent.send_data(data_spec(), vec![0, 255])
    else {
        panic!("send must retain the terminal session cause");
    };
    assert_eq!(send_cause, cause);
    observer.close(failed).unwrap();
    let closed = observer.state();
    assert!(closed.closed);
    assert_eq!(closed.execution, Execution::Failed);
    assert_eq!(closed.execution_error, Some(cause));
}
