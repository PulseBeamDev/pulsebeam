use super::*;
use pulsebeam::entity::ParticipantExternalId;
use pulsebeam_agent_native::agent_core::{
    Participant, Snapshot, TopicMode, TopicPublisher, TopicPublisherStatus, TopicSubscriber,
    TopicSubscriberStatus,
};

type Mutation<T> = (&'static str, fn(&mut T));

fn fixture() -> (ApplicationStateOracle, ClientObservation) {
    let room = RoomId::derive(
        &DEVELOPMENT_PROJECT_ID,
        &RoomExternalId::new("room").unwrap(),
    );
    let publisher = ParticipantId::derive(&room, &ParticipantExternalId::new("publisher").unwrap());
    let viewer = ParticipantId::derive(&room, &ParticipantExternalId::new("viewer").unwrap());
    let source_connection = ConnectionId::new();
    let receiver_connection = ConnectionId::new();
    let model = ApplicationStateOracle {
        participants: [
            (publisher, true, source_connection),
            (viewer, false, receiver_connection),
        ]
        .into_iter()
        .map(|(id, video, connection)| ExpectedParticipant {
            id,
            room: "room",
            video,
            video_label: "video".into(),
            audio: false,
            audio_pinned: Vec::new(),
            automatic_audio: true,
            required: true,
            connection: Some(connection),
            topics: Default::default(),
        })
        .collect(),
    };
    let publication = model.participants[0].publications().next().unwrap();
    let snapshot = Snapshot {
        connection: ConnectionState::Connected,
        participant_id: Some(viewer.as_str()),
        room_external_id: Some("room".into()),
        participants: BTreeMap::from([(
            publisher.as_str(),
            Participant {
                id: publisher.as_str(),
                external_id: "publisher".into(),
            },
        )]),
        video_mapping: BTreeMap::from([(0, publication.id.clone())]),
        publications: BTreeMap::from([(publication.id.clone(), publication)]),
        ..Default::default()
    };
    (
        model,
        ClientObservation {
            admission: Some(receiver_connection),
            snapshot,
            video_capacity: 1,
            ..Default::default()
        },
    )
}

fn audio_fixture(capacity: u32) -> (ApplicationStateOracle, ClientObservation) {
    let (mut model, mut client) = fixture();
    for name in ["audio-first", "audio-second", "audio-third"] {
        let source = ExpectedParticipant {
            id: ParticipantId::derive(
                &model.participants[0].room_id().unwrap(),
                &ParticipantExternalId::new(name).unwrap(),
            ),
            video: false,
            audio: true,
            connection: Some(ConnectionId::new()),
            ..model.participants[0].clone()
        };
        let publication = source.publications().next().unwrap();
        client.snapshot.participants.insert(
            source.id.as_str(),
            Participant {
                id: source.id.as_str(),
                external_id: name.into(),
            },
        );
        client
            .snapshot
            .publications
            .insert(publication.id.clone(), publication);
        model.participants.push(source);
    }
    client.audio_capacity = usize::try_from(capacity).unwrap();
    client.snapshot.audio_mapping = (0..capacity)
        .zip(
            model
                .participants
                .iter()
                .filter(|peer| peer.audio)
                .map(|peer| peer.track_id(TrackKind::Audio, "audio").as_str()),
        )
        .collect();
    (model, client)
}

#[test]
fn automatic_audio_oracle_requires_every_uncontended_live_source() {
    let (mut model, mut client) = audio_fixture(3);
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    model.participants.last_mut().unwrap().required = false;
    client.snapshot.audio_mapping.remove(&2);
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();

    let mutations: &[Mutation<ClientObservation>] = &[
        ("missing automatic source", |state| {
            state.snapshot.audio_mapping.remove(&2);
        }),
        ("duplicate automatic source", |state| {
            let track = state.snapshot.audio_mapping.get(&0).unwrap().clone();
            state.snapshot.audio_mapping.insert(2, track);
        }),
        ("wrong automatic kind", |state| {
            let track = state.snapshot.video_mapping.get(&0).unwrap().clone();
            state.snapshot.audio_mapping.insert(2, track);
        }),
    ];
    for (name, mutate) in mutations {
        let (model, mut client) = audio_fixture(3);
        mutate(&mut client);
        assert!(
            model
                .check_client(
                    &model.participants[1],
                    &client,
                    &[model.participants[0].id.as_str()],
                )
                .is_err(),
            "accepted {name}"
        );
    }
}

#[test]
fn automatic_audio_oracle_allows_contended_choices_but_requires_full_slots_and_pins() {
    let (mut model, mut client) = audio_fixture(2);
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    let first = client.snapshot.audio_mapping.get(&0).unwrap().clone();
    let third = model
        .participants
        .last()
        .unwrap()
        .track_id(TrackKind::Audio, "audio")
        .as_str();
    client.snapshot.audio_mapping.insert(0, third);
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    let second = client.snapshot.audio_mapping.remove(&1).unwrap();
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.audio_mapping.insert(1, second);
    model.participants[1].audio_pinned =
        vec![client.snapshot.publications[&first].participant_id.clone()];
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.audio_mapping.insert(0, first);
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
}

#[test]
fn transition_safety_allows_pending_withdrawal_but_rejects_invalid_assignments() {
    let (model, mut client) = fixture();
    let allowed = model.participants.iter().map(|p| p.id.as_str()).collect();
    ApplicationStateOracle::check_client_safety(&client, &allowed).unwrap();
    assert!(
        model
            .check_client(&model.participants[1], &client, &[])
            .is_err()
    );
    client.snapshot.video_mapping.clear();
    model
        .check_client(&model.participants[1], &client, &[])
        .unwrap();
    client
        .snapshot
        .video_mapping
        .insert(0, "retired-track".into());
    assert!(ApplicationStateOracle::check_client_safety(&client, &allowed).is_err());
    let (_, client) = fixture();
    assert!(ApplicationStateOracle::check_client_safety(&client, &BTreeSet::new()).is_err());
}

#[test]
fn client_oracle_rejects_wrong_missing_duplicate_and_stale_assignments() {
    let (model, client) = fixture();
    model
        .check_client(
            &model.participants[1],
            &client,
            &[model.participants[0].id.as_str()],
        )
        .unwrap();
    let mutations: &[Mutation<ClientObservation>] = &[
        ("disconnected client", |state| {
            state.snapshot.connection = ConnectionState::Disconnected;
        }),
        ("missing participant", |state| {
            state.snapshot.participants.clear();
        }),
        ("missing publication", |state| {
            state.snapshot.publications.clear();
        }),
        ("wrong kind", |state| {
            state
                .snapshot
                .publications
                .values_mut()
                .next()
                .unwrap()
                .kind = MediaKind::Audio;
        }),
        ("wrong owner", |state| {
            state
                .snapshot
                .publications
                .values_mut()
                .next()
                .unwrap()
                .participant_id = state.snapshot.participant_id.clone().unwrap();
        }),
        ("missing assignment", |state| {
            state.snapshot.video_mapping.clear();
        }),
        ("duplicate assignment", |state| {
            let track = state.snapshot.video_mapping[&0].clone();
            state.snapshot.video_mapping.insert(1, track);
            state.video_capacity = 2;
        }),
        ("stale assignment", |state| {
            state
                .snapshot
                .video_mapping
                .insert(0, "retired-track".into());
        }),
    ];
    for (name, mutate) in mutations {
        let (model, mut client) = fixture();
        mutate(&mut client);
        assert!(
            model
                .check_client(
                    &model.participants[1],
                    &client,
                    &[model.participants[0].id.as_str()]
                )
                .is_err(),
            "accepted {name}"
        );
    }
}

#[test]
fn subscriber_bindings_follow_declared_room_topic_and_mode() {
    let (mut model, mut client) = fixture();
    model.participants[0]
        .topics
        .publishers
        .push(TopicPublisher {
            topic: "updates".into(),
            mode: TopicMode::Ordered,
        });
    let registration = TopicSubscriber {
        topic: "updates".into(),
        mode: TopicMode::Ordered,
    };
    model.participants[1]
        .topics
        .subscribers
        .push(registration.clone());
    client
        .snapshot
        .topics
        .subscribers
        .push(TopicSubscriberStatus {
            registration,
            channel: None,
            publishers: 1,
            buffered_messages: 0,
        });
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    client.snapshot.topics.subscribers[0].publishers = 0;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    model.participants[0].required = false;
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    client.snapshot.topics.subscribers[0].publishers = 2;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.topics.subscribers[0].publishers = 1;
    model.participants[0].topics.publishers[0].mode = TopicMode::Latest;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    model.participants[0].topics.publishers[0].mode = TopicMode::Ordered;
    model.participants[0].room = "other-room";
    client.snapshot.participants.clear();
    client.snapshot.publications.clear();
    client.snapshot.video_mapping.clear();
    assert!(
        model
            .check_client(&model.participants[1], &client, &[])
            .is_err()
    );
    client.snapshot.topics.subscribers[0].publishers = 0;
    model
        .check_client(&model.participants[1], &client, &[])
        .unwrap();
}

#[test]
fn publishers_expose_distinct_public_bindings_for_declared_registrations() {
    let (mut model, mut client) = fixture();
    for (mode, id) in [
        (TopicMode::Latest, "latest-id"),
        (TopicMode::Ordered, "ordered-id"),
    ] {
        let registration = TopicPublisher {
            topic: "updates".into(),
            mode,
        };
        model.participants[1]
            .topics
            .publishers
            .push(registration.clone());
        client
            .snapshot
            .topics
            .publishers
            .push(TopicPublisherStatus {
                registration,
                channel: None,
                data_track_id: Some(id.into()),
                next_sequence: 0,
                accepted_history: 0,
                queued_messages: 0,
                send_pending: false,
            });
    }
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    client.snapshot.topics.publishers[0].data_track_id = None;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.topics.publishers[0].data_track_id = Some("ordered-id".into());
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.topics.publishers[0].data_track_id = Some("latest-id".into());
    client.snapshot.topics.publishers[0].registration.mode = TopicMode::Lossy;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
}
