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
    let canonical = source_connection.derive_track_id(TrackKind::Video, "opaque-owned-source");
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
            media: if video {
                vec![Publication {
                    id: canonical.as_str(),
                    participant_id: id.as_str(),
                    kind: MediaKind::Video,
                    label: "video".into(),
                }]
            } else {
                Vec::new()
            },
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
        let mut source = ExpectedParticipant {
            id: ParticipantId::derive(
                &model.participants[0].room_id().unwrap(),
                &ParticipantExternalId::new(name).unwrap(),
            ),
            video: false,
            audio: true,
            connection: Some(ConnectionId::new()),
            ..model.participants[0].clone()
        };
        source.media = vec![Publication {
            id: source
                .connection
                .unwrap()
                .derive_track_id(TrackKind::Audio, "opaque-audio-source")
                .as_str(),
            participant_id: source.id.as_str(),
            kind: MediaKind::Audio,
            label: "audio".into(),
        }];
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
fn automatic_audio_oracle_requires_all_sources_without_a_fixed_receiver_inventory() {
    let (mut model, mut client) = audio_fixture(3);
    client.audio_capacity = 1;
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    let first = client.snapshot.audio_mapping.remove(&0).unwrap();
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    model.participants[1].audio_pinned =
        vec![client.snapshot.publications[&first].participant_id.clone()];
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.audio_mapping.insert(0, first.clone());
    model
        .check_client(&model.participants[1], &client, &requested)
        .unwrap();
    model.participants[1].automatic_audio = false;
    assert!(
        model
            .check_client(&model.participants[1], &client, &requested)
            .is_err()
    );
    client
        .snapshot
        .audio_mapping
        .retain(|_, track| track == &first);
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

    use pulsebeam_agent_native::agent_core as core;
    let mut agent = core::Agent::new(core::AgentConfig {
        endpoint: "https://sfu.test/".into(),
        token: "token".into(),
        retry: Default::default(),
        log_level: Default::default(),
    })
    .unwrap();
    agent
        .command(core::AgentCommand::ReplaceDesired(core::DesiredState {
            revision: 1,
            connected: true,
            ..Default::default()
        }))
        .unwrap();
    let core::Effect::Rtc(core::RtcEffect::CreateOffer { generation, .. }) =
        agent.next_effect().unwrap()
    else {
        panic!("expected admission offer");
    };
    let (_, mut client) = fixture();
    client.snapshot.generation = Some(generation);
    let publication = client.snapshot.publications.pop_first().unwrap().1;
    client
        .owned_receivers
        .push(super::super::client::OwnedReceiverEvidence {
            track: pulsebeam_agent_native::NativeReceiverInfo {
                generation: generation.get(),
                receiver: 1,
                track_id: "native-receiver".into(),
                mid: "v0".into(),
                media_index: 0,
            },
            publication,
        });
    ApplicationStateOracle::check_client_safety(&client, &allowed).unwrap();
    assert!(
        model
            .check_client(&model.participants[1], &client, &[])
            .is_err()
    );
    let mutations: &[Mutation<ClientObservation>] = &[
        ("retired receiver", |state| state.owned_receivers.clear()),
        ("old generation", |state| {
            state.owned_receivers[0].track.generation += 1;
        }),
        ("wrong receiver coordinate", |state| {
            state.owned_receivers[0].track.media_index = 1;
        }),
        ("wrong source", |state| {
            state.owned_receivers[0].publication.id = "unknown".into();
        }),
        ("wrong kind", |state| {
            state.owned_receivers[0].publication.kind = MediaKind::Audio;
        }),
        ("foreign origin", |state| {
            state.owned_receivers[0].publication.participant_id = "foreign".into();
        }),
    ];
    for (name, mutate) in mutations {
        let mut invalid = client.clone();
        mutate(&mut invalid);
        assert!(
            ApplicationStateOracle::check_client_safety(&invalid, &allowed).is_err(),
            "accepted {name}"
        );
    }
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

#[test]
fn public_catalog_binding_preserves_opaque_identity_and_declared_constraints() {
    let (model, client) = fixture();
    let mut publisher = model.participants[0].clone();
    let mut descriptors = client
        .snapshot
        .publications
        .values()
        .cloned()
        .collect::<Vec<_>>();
    assert!(!publisher.bind_media(&[]).unwrap());
    assert!(publisher.bind_media(&descriptors).unwrap());
    let canonical = publisher.track_id(TrackKind::Video, "video");
    assert_ne!(
        canonical,
        publisher
            .connection
            .unwrap()
            .derive_track_id(TrackKind::Video, "video")
    );
    descriptors.push(descriptors[0].clone());
    assert!(publisher.bind_media(&descriptors).is_err());
    descriptors.pop();
    descriptors[0].label = "wrong".into();
    assert!(!publisher.bind_media(&descriptors).unwrap());
    descriptors[0].label = "video".into();
    descriptors[0].participant_id = model.participants[1].id.as_str();
    assert!(!publisher.bind_media(&descriptors).unwrap());
    descriptors[0].participant_id = publisher.id.as_str();
    descriptors[0].id = publisher
        .connection
        .unwrap()
        .derive_track_id(TrackKind::Audio, "opaque-wrong-kind")
        .as_str();
    assert!(publisher.bind_media(&descriptors).is_err());
}
