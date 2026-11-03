use super::*;
use pulsebeam_proto::state;

fn spec(mode: TopicMode) -> TrackSpec {
    TrackSpec::Data {
        label: "state".into(),
        delivery: mode,
    }
}

fn deadline(agent: &mut Agent) -> TimerId {
    let Effect::Timer(TimerEffect::Schedule { timer, after }) = next_effect(agent) else {
        panic!("admission deadline")
    };
    assert_eq!(after, Duration::from_secs(10));
    timer
}

fn dispatched(agent: &mut Agent, timer: TimerId) -> (OperationId, ChannelId, TopicBody) {
    assert_eq!(
        next_effect(agent),
        Effect::Timer(TimerEffect::Cancel { timer })
    );
    next_topic_send(agent)
}

fn finished(agent: &mut Agent) -> Vec<(OperationId, Option<DataSendFailure>)> {
    drain_notifications(agent)
        .into_iter()
        .filter_map(|notification| match notification {
            Notification::DataSendFinished { send, error } => Some((send, error)),
            _ => None,
        })
        .collect()
}

#[test]
fn owned_sends_wait_for_host_acceptance_and_latest_supersession_is_explicit() {
    let (mut agent, generation, _, publication) =
        native_connection::connected_publisher(TopicMode::Latest);
    let first = agent.send_data(spec(TopicMode::Latest), vec![]).unwrap();
    let first_timer = deadline(&mut agent);
    let (first_operation, channel, body) = dispatched(&mut agent, first_timer);
    assert_eq!(channel, publication.channel);
    let TopicBody::Message(message) = body else {
        panic!("binary message")
    };
    assert!(message.payload.is_empty());
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer: first_timer }))
        .unwrap();
    assert!(
        agent.next_effect().is_none(),
        "a stale admission deadline cannot override an in-flight host outcome"
    );
    assert!(
        finished(&mut agent).is_empty(),
        "queueing is not host acceptance"
    );
    let second = agent.send_data(spec(TopicMode::Latest), vec![1]).unwrap();
    let second_timer = deadline(&mut agent);
    let third = agent
        .send_data(spec(TopicMode::Latest), vec![0, 255, 128])
        .unwrap();
    let third_timer = deadline(&mut agent);
    assert_eq!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Cancel {
            timer: second_timer
        })
    );
    assert_eq!(
        finished(&mut agent),
        vec![(second, Some(DataSendFailure::Superseded))]
    );
    acknowledge_send(&mut agent, generation, channel, first_operation);
    let (third_operation, _, body) = dispatched(&mut agent, third_timer);
    let TopicBody::Message(message) = body else {
        panic!("binary message")
    };
    assert_eq!(message.payload, [0, 255, 128]);
    assert_eq!(finished(&mut agent), vec![(first, None)]);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: third_operation,
            generation,
            channel,
            message: "host queue full".into(),
        }))
        .unwrap();
    assert_eq!(
        finished(&mut agent),
        vec![(third, Some(DataSendFailure::HostRejected))]
    );
    acknowledge_send(&mut agent, generation, channel, third_operation);
    assert!(
        finished(&mut agent).is_empty(),
        "late host events cannot resurrect a send"
    );
    assert!(agent.next_effect().is_none());
}

#[test]
fn owned_send_waits_for_its_first_authoritative_owned_binding() {
    let (mut agent, generation, signal) = native_connection::connected(TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Lossy)],
        subscribers: vec![],
    });
    let ticket = agent.send_data(spec(TopicMode::Lossy), vec![255]).unwrap();
    let timer = deadline(&mut agent);
    assert!(agent.next_effect().is_none());
    assert!(finished(&mut agent).is_empty());
    let publication = native_connection::prepare_data(&mut agent, generation, signal);
    native_connection::observe(
        &mut agent,
        generation,
        signal,
        vec![native_connection::confirmed(
            state::resource_key::Key::Track(native_connection::SOURCE.into()),
            0,
            state::confirmed_state::Value::Track(state::Track {
                track_id: native_connection::SOURCE.into(),
                participant_id: "p1".into(),
                label: "state".into(),
                kind: Some(state::track::Kind::Data(state::DataTrack {
                    delivery: state::DataDelivery::Lossy as i32,
                })),
            }),
        )],
    );
    assert!(
        agent.next_effect().is_none(),
        "discovery does not admit a publication"
    );
    assert!(finished(&mut agent).is_empty());
    native_connection::confirm_publication(
        &mut agent,
        generation,
        signal,
        &publication,
        native_connection::SOURCE,
    );
    let (operation, channel, body) = dispatched(&mut agent, timer);
    let TopicBody::Message(message) = body else {
        panic!("bound message")
    };
    assert_eq!(channel, publication.channel);
    assert_eq!(message.data_track_id, native_connection::SOURCE);
    assert_eq!(message.payload, [255]);
    acknowledge_send(&mut agent, generation, channel, operation);
    assert_eq!(finished(&mut agent), vec![(ticket, None)]);
}

#[test]
fn owned_send_binding_wait_is_bounded_and_timeout_releases_only_its_admission() {
    let mut agent = Agent::new(config()).unwrap();
    assert!(matches!(
        agent.send_data(spec(TopicMode::Lossy), vec![]),
        Err(AgentError::InvalidResource(
            TrackResourceError::NotConnected
        ))
    ));
    assert!(agent.snapshot().topics.publishers.is_empty());
    agent.command(AgentCommand::Connect).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
    let mut first = None;
    for _ in 0..TOPIC_SEND_QUEUE_CAPACITY {
        let ticket = agent
            .send_data(spec(TopicMode::Lossy), vec![0, 255])
            .unwrap();
        let timer = deadline(&mut agent);
        first.get_or_insert((ticket, timer));
    }
    assert!(matches!(
        agent.send_data(spec(TopicMode::Lossy), vec![]),
        Err(AgentError::InvalidTopic(TopicError::SendQueueFull(_)))
    ));
    assert!(matches!(
        agent.send_data(
            TrackSpec::Data {
                label: "oversized".into(),
                delivery: TopicMode::Lossy
            },
            vec![0; MAX_TOPIC_PAYLOAD_BYTES + 1]
        ),
        Err(AgentError::InvalidTopic(TopicError::PayloadTooLarge { .. }))
    ));
    assert_eq!(
        agent.snapshot().topics.publishers.len(),
        1,
        "invalid sends do not establish producer interest"
    );
    let (ticket, timer) = first.unwrap();
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    assert_eq!(
        finished(&mut agent),
        vec![(ticket, Some(DataSendFailure::TimedOut))]
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Cancel { timer })
    );
    agent.send_data(spec(TopicMode::Lossy), vec![]).unwrap();
    deadline(&mut agent);
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: agent.snapshot().desired_revision.saturating_add(1),
            connected: false,
            ..DesiredState::default()
        }))
        .unwrap();
    let results = finished(&mut agent);
    assert_eq!(results.len(), TOPIC_SEND_QUEUE_CAPACITY);
    assert!(
        results
            .iter()
            .all(|(_, error)| *error == Some(DataSendFailure::Disconnected))
    );
    assert_eq!(
        agent.snapshot().topics.publishers.len(),
        1,
        "disconnect preserves lazy producer declarations"
    );
}

#[test]
fn queued_timeout_updates_observation_and_cannot_transmit_later() {
    let (mut agent, generation, _, _) = native_connection::connected_publisher(TopicMode::Lossy);
    let first = agent.send_data(spec(TopicMode::Lossy), vec![1]).unwrap();
    let first_timer = deadline(&mut agent);
    let (operation, channel, _) = dispatched(&mut agent, first_timer);
    let second = agent.send_data(spec(TopicMode::Lossy), vec![2]).unwrap();
    let timer = deadline(&mut agent);
    assert_eq!(agent.snapshot().topics.publishers[0].queued_messages, 1);
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    assert_eq!(
        finished(&mut agent),
        vec![(second, Some(DataSendFailure::TimedOut))]
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Cancel { timer })
    );
    assert_eq!(agent.snapshot().topics.publishers[0].queued_messages, 0);
    acknowledge_send(&mut agent, generation, channel, operation);
    assert_eq!(finished(&mut agent), vec![(first, None)]);
    assert!(agent.next_effect().is_none());
}

#[test]
fn owned_source_end_rejects_in_flight_and_queued_bytes() {
    let (mut agent, generation, signal, publication) =
        native_connection::connected_publisher(TopicMode::Ordered);
    let first = agent.send_data(spec(TopicMode::Ordered), vec![1]).unwrap();
    let timer = deadline(&mut agent);
    let (old_operation, channel, _) = dispatched(&mut agent, timer);
    assert_eq!(channel, publication.channel);
    let second = agent.send_data(spec(TopicMode::Ordered), vec![2]).unwrap();
    deadline(&mut agent);
    native_connection::observe(
        &mut agent,
        generation,
        signal,
        vec![native_connection::confirmed(
            state::resource_key::Key::Owned(publication.reference),
            publication.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::SourceEnded as i32,
            }),
        )],
    );
    assert_eq!(
        finished(&mut agent),
        vec![
            (first, Some(DataSendFailure::ChannelFailed)),
            (second, Some(DataSendFailure::ChannelFailed)),
        ]
    );
    let mut closed = false;
    while let Some(effect) = agent.next_effect() {
        match effect {
            Effect::Timer(TimerEffect::Cancel { .. }) => {}
            Effect::Rtc(RtcEffect::CloseDataChannel {
                generation: actual,
                channel: actual_channel,
            }) => {
                assert_eq!(actual, generation);
                assert_eq!(actual_channel, channel);
                assert!(!closed);
                closed = true;
            }
            effect => panic!("source-local cutoff: {effect:?}"),
        }
    }
    assert!(closed);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert!(agent.snapshot().terminal_failure.is_none());
    acknowledge_send(&mut agent, generation, channel, old_operation);
    assert!(finished(&mut agent).is_empty());
    assert!(
        agent.next_effect().is_none(),
        "retired bytes cannot move to a replacement"
    );
}

#[test]
fn terminal_connection_failure_rejects_a_send_waiting_for_its_first_binding() {
    let mut agent = Agent::new(config()).unwrap();
    let (_, operation) = begin_connect(&mut agent, desired(1), channel(12));
    let send = agent.send_data(spec(TopicMode::Lossy), vec![1]).unwrap();
    deadline(&mut agent);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 401,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        finished(&mut agent),
        vec![(send, Some(DataSendFailure::TerminalFailure))]
    );
    assert!(agent.send_data(spec(TopicMode::Lossy), vec![2]).is_err());
    agent.command(AgentCommand::Connect).unwrap();
    assert!(
        finished(&mut agent).is_empty(),
        "explicit retry cannot retarget the failed send"
    );
}
