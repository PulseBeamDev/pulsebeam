use super::*;
use pulsebeam_proto::state;

fn spec(mode: TopicMode) -> TrackSpec {
    TrackSpec::Data {
        track_id: "state".into(),
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
fn data_v1_send_settles_local_admission_without_promising_host_acceptance() {
    let (mut agent, generation, _, publication) =
        native_connection::connected_publisher(TopicMode::Latest);
    let send = agent.send_data(spec(TopicMode::Latest), vec![]).unwrap();
    assert_eq!(finished(&mut agent), vec![(send, None)]);
    let (operation, channel, payload) = next_data_record(&mut agent);
    assert_eq!(channel, publication.channel);
    assert!(
        pulsebeam_proto::data_record::Record::decode(&payload, None)
            .unwrap()
            .bytes()
            .is_empty()
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation,
            generation,
            channel,
            message: "host queue full".into(),
        }))
        .unwrap();
    acknowledge_send(&mut agent, generation, channel, operation);
    assert!(finished(&mut agent).is_empty());
}

#[test]
fn data_v1_send_does_not_bypass_an_owned_declaration_or_binding() {
    let (mut agent, _, _) = native_connection::connected(TopicRegistrations::default());
    let declaration = spec(TopicMode::Lossy);
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::DeclareDataPublication {
            capture_clock: None,
            spec: declaration.clone(),
            record_dd: false,
        })
        .unwrap()
    else {
        panic!("data publication")
    };
    assert!(agent.send_data(spec(TopicMode::Ordered), vec![1]).is_err());
    assert!(matches!(
        agent.send_data(declaration, vec![1]),
        Err(AgentError::InvalidTopic(TopicError::PublisherUnavailable(
            _
        )))
    ));
    agent.resource(ResourceCommand::Release { owner }).unwrap();
}

#[test]
fn owned_send_requires_authoritative_installation_not_discovery() {
    let (mut agent, generation, signal) = native_connection::connected(TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Lossy)],
        subscribers: vec![],
    });
    assert!(agent.send_data(spec(TopicMode::Lossy), vec![255]).is_err());
    let publication = native_connection::prepare_data(&mut agent, generation, signal);
    native_connection::observe(
        &mut agent,
        generation,
        signal,
        vec![native_connection::confirmed(
            state::resource_key::Key::Publication(native_connection::SOURCE.into()),
            0,
            state::confirmed_state::Value::Track(state::Track {
                publication_id: native_connection::SOURCE.into(),
                participant_id: "p1".into(),
                room_id: "room".into(),
                track_id: "state".into(),
                kind: Some(state::track::Kind::Data(state::DataTrack {
                    capture_clock: None,
                    delivery: state::DataDelivery::Lossy as i32,
                    record_dd: false,
                })),
            }),
        )],
    );
    assert!(
        agent.next_effect_after_turn().is_none(),
        "discovery does not admit a publication"
    );
    assert!(agent.send_data(spec(TopicMode::Lossy), vec![255]).is_err());
    assert!(finished(&mut agent).is_empty());
    native_connection::confirm_publication(
        &mut agent,
        generation,
        signal,
        &publication,
        native_connection::SOURCE,
    );
    let ticket = agent.send_data(spec(TopicMode::Lossy), vec![255]).unwrap();
    let (operation, channel, bytes) = next_data_record(&mut agent);
    assert_eq!(channel, publication.channel);
    assert_eq!(
        pulsebeam_proto::data_record::Record::decode(&bytes, None)
            .unwrap()
            .bytes(),
        [255]
    );
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
    agent.command(AgentCommand::Join("room".into())).unwrap();
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
                track_id: "oversized".into(),
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
fn send_queued_during_join_enters_only_the_verified_data_v1_binding() {
    let (mut agent, generation, signal) = native_connection::joining(TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Lossy)],
        subscribers: vec![],
    });
    let send = agent
        .send_data(spec(TopicMode::Lossy), vec![0, 255])
        .unwrap();
    let timer = deadline(&mut agent);
    assert!(finished(&mut agent).is_empty());
    native_connection::complete_initial_state(&mut agent, generation, signal);
    let publication = native_connection::prepare_data(&mut agent, generation, signal);
    assert!(finished(&mut agent).is_empty());
    native_connection::confirm_publication(
        &mut agent,
        generation,
        signal,
        &publication,
        native_connection::SOURCE,
    );
    assert_eq!(finished(&mut agent), vec![(send, None)]);
    assert_eq!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Cancel { timer })
    );
    let (_, channel, bytes) = next_data_record(&mut agent);
    assert_eq!(channel, publication.channel);
    assert_eq!(
        pulsebeam_proto::data_record::Record::decode(&bytes, None)
            .unwrap()
            .bytes(),
        [0, 255]
    );
}

#[test]
fn owned_source_end_fences_pending_data_records() {
    let (mut agent, generation, signal, publication) =
        native_connection::connected_publisher(TopicMode::Lossy);
    let send = agent
        .send_data(spec(TopicMode::Lossy), vec![1; 1025])
        .unwrap();
    let (old_operation, channel, _) = next_data_record(&mut agent);
    assert_eq!(channel, publication.channel);
    assert_eq!(finished(&mut agent), vec![(send, None)]);
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
    assert!(finished(&mut agent).is_empty());
    let mut closed = false;
    while let Some(effect) = agent.next_effect_after_turn() {
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
        agent.next_effect_after_turn().is_none(),
        "retired bytes cannot move to a replacement"
    );
}

#[test]
fn terminal_connection_failure_rejects_a_send_waiting_for_its_first_binding() {
    for (status, code) in [
        (401, crate::ffi::ErrorCode::PermissionDenied),
        (409, crate::ffi::ErrorCode::Replaced),
    ] {
        let mut agent = Agent::new(config()).unwrap();
        let (_, operation) = begin_connect(&mut agent, desired(1), channel(12));
        let send = agent.send_data(spec(TopicMode::Lossy), vec![1]).unwrap();
        deadline(&mut agent);
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status,
                    headers: vec![],
                    body: vec![],
                },
            }))
            .unwrap();
        let observed = crate::ffi::AgentState::from(agent.snapshot())
            .error
            .unwrap();
        assert_eq!(observed.code, code);
        let results = finished(&mut agent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, send);
        let failure = results[0].1.as_ref().expect("terminal send failure");
        assert_eq!(crate::ffi::ErrorInfo::from(failure), observed);
        let rejected = agent
            .send_data(spec(TopicMode::Lossy), vec![2])
            .unwrap_err();
        assert_eq!(crate::ffi::ErrorInfo::from(&rejected), observed);
        if code == crate::ffi::ErrorCode::Replaced {
            assert_eq!(
                agent.command(AgentCommand::Connect),
                Err(AgentError::Replaced)
            );
        } else {
            agent.command(AgentCommand::Connect).unwrap();
            assert!(agent.snapshot().terminal_failure.is_none());
        }
        assert_eq!(crate::ffi::ErrorInfo::from(failure), observed);
        assert!(
            finished(&mut agent).is_empty(),
            "explicit retry cannot retarget the failed send"
        );
        while let Some(effect) = agent.next_effect_after_turn() {
            assert!(!matches!(
                effect,
                Effect::DataChannel(DataChannelEffect::Send { .. })
            ));
        }
    }
}
