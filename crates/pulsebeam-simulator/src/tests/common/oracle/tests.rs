use super::*;
use pulsebeam::entity::ParticipantId;
use pulsebeam_agent_native::agent_core::{
    Participant, Snapshot, TopicMode, TopicPublisher, TopicSubscriber,
    ffi::{
        DataPublicationState, DataSubscriptionState, PublicationAdmissionStatus,
        PublicationExecution, SubscriptionSourceStatus, TrackSelector,
    },
};

type Mutation<T> = (&'static str, fn(&mut T));

fn fixture() -> (ApplicationStateOracle, ClientObservation) {
    let publisher = ParticipantId::new("publisher").unwrap();
    let viewer = ParticipantId::new("viewer").unwrap();
    let source_connection = ConnectionId::new();
    let receiver_connection = ConnectionId::new();
    let canonical = pulsebeam::entity::PublicationId::mint();
    let model = ApplicationStateOracle {
        participants: [
            (publisher, "publisher", true, source_connection),
            (viewer, "viewer", false, receiver_connection),
        ]
        .into_iter()
        .map(|(id, external_id, video, connection)| ExpectedParticipant {
            id,
            external_id: external_id.into(),
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
                    participant_id: id.as_str().to_owned(),
                    kind: MediaKind::Video,
                    track_id: "video".into(),
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
        participant_id: Some(viewer.as_str().to_owned()),
        room_id: Some("room".into()),
        participants: BTreeMap::from([(
            publisher.as_str().to_owned(),
            Participant {
                id: publisher.as_str().to_owned(),
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
            external_id: name.into(),
            id: ParticipantId::new(name).unwrap(),
            video: false,
            audio: true,
            connection: Some(ConnectionId::new()),
            ..model.participants[0].clone()
        };
        source.media = vec![Publication {
            id: pulsebeam::entity::PublicationId::mint().as_str(),
            participant_id: source.id.as_str().to_owned(),
            kind: MediaKind::Audio,
            track_id: "audio".into(),
        }];
        let publication = source.publications().next().unwrap();
        client.snapshot.participants.insert(
            source.id.as_str().to_owned(),
            Participant {
                id: source.id.as_str().to_owned(),
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
                .map(|peer| peer.publication_id(TrackKind::Audio, "audio").as_str()),
        )
        .collect();
    (model, client)
}

#[test]
fn automatic_audio_oracle_requires_every_uncontended_live_source() {
    let (mut model, mut client) = audio_fixture(3);
    let requested = [model.participants[0].id.as_str().to_owned()];
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
                    &[model.participants[0].id.as_str().to_owned()],
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
    let requested = [model.participants[0].id.as_str().to_owned()];
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
    let allowed = model
        .participants
        .iter()
        .map(|p| p.id.as_str().to_owned())
        .collect();
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
        .command(core::AgentCommand::Join("room".into()))
        .unwrap();
    let core::Effect::Rtc(core::RtcEffect::CreateOffer { generation, .. }) =
        agent.next_effect().unwrap()
    else {
        panic!("expected admission offer");
    };
    let (mut cross_room, mut client) = fixture();
    client.snapshot.generation = Some(generation);
    let publication = client.snapshot.publications.pop_first().unwrap().1;
    client
        .owned_receivers
        .push(super::super::client::OwnedReceiverEvidence {
            track: pulsebeam_agent_native::NativeReceiverInfo {
                generation: generation.get(),
                receiver: 1,
                publication_id: "native-receiver".into(),
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
    cross_room.participants[0].room = "other-room";
    client.snapshot.participants.clear();
    client.snapshot.remote_tracks.clear();
    let requested = [cross_room.participants[0].id.as_str().to_owned()];
    assert!(client.snapshot.publications.is_empty());
    cross_room
        .check_client(&cross_room.participants[1], &client, &requested)
        .unwrap();
    let mut unbound = cross_room.participants.clone();
    unbound[0].media.clear();
    assert!(
        ApplicationStateOracle {
            participants: unbound
        }
        .check_client(&cross_room.participants[1], &client, &requested)
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
        assert!(
            cross_room
                .check_client(&cross_room.participants[1], &invalid, &requested)
                .is_err(),
            "accepted explicit remote source with {name}"
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
            &[model.participants[0].id.as_str().to_owned()],
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
                    &[model.participants[0].id.as_str().to_owned()]
                )
                .is_err(),
            "accepted {name}"
        );
    }
}

#[test]
fn subscriber_bindings_follow_declared_room_topic_and_mode() {
    let (mut model, mut client) = fixture();
    let publisher = TopicPublisher {
        topic: "updates".into(),
        mode: TopicMode::Lossy,
    };
    model.participants[0]
        .topics
        .publishers
        .push(publisher.clone());
    let registration = TopicSubscriber {
        room: model.participants[0].room.to_owned(),
        participant_id: model.participants[0].external_id.clone(),
        topic: publisher.topic.clone(),
        mode: publisher.mode,
    };
    model.participants[1]
        .topics
        .subscribers
        .push(registration.clone());
    client.data_catalog.insert(
        (registration.participant_id.clone(), publisher),
        "catalog-id".into(),
    );
    let bound = DataSubscriptionState {
        observation_revision: 1,
        intent_revision: 1,
        evaluated_revision: 1,
        accepted_revision: Some(1),
        rejected_revision: None,
        selector: Some(TrackSelector {
            room: registration.room.clone(),
            participant_id: registration.participant_id.clone(),
            track_id: registration.topic.clone(),
        }),
        delivery: Some(registration.mode.into()),
        local_enabled: true,
        closed: false,
        source_status: SubscriptionSourceStatus::Available,
        source_track_id: Some("catalog-id".into()),
        admission: PublicationAdmissionStatus::Admitted,
        admission_pending: false,
        error: None,
        execution: PublicationExecution::Installed,
        execution_error: None,
        terminal_error: None,
    };
    client
        .data_subscribers
        .insert(registration.clone(), bound.clone());
    let requested = [model.participants[0].id.as_str().to_owned()];
    let check = |model: &ApplicationStateOracle, client: &ClientObservation| {
        model.check_client(&model.participants[1], client, &requested)
    };
    check(&model, &client).unwrap();
    let mut wrong_id = bound.clone();
    wrong_id.source_track_id = Some("not-the-catalog-id".into());
    client
        .data_subscribers
        .insert(registration.clone(), wrong_id);
    assert!(check(&model, &client).is_err());
    let mut unbound = bound.clone();
    unbound.delivery = None;
    client
        .data_subscribers
        .insert(registration.clone(), unbound.clone());
    assert!(
        check(&model, &client).is_err(),
        "installed binding cannot omit delivery"
    );
    unbound.source_track_id = None;
    unbound.execution = PublicationExecution::Unavailable;
    client
        .data_subscribers
        .insert(registration.clone(), unbound.clone());
    assert!(check(&model, &client).is_err());
    model.participants[0].required = false;
    check(&model, &client).unwrap();
    client.data_subscribers.insert(registration.clone(), bound);
    let peer = &mut model.participants[0];
    peer.topics.publishers[0].mode = TopicMode::Latest;
    assert!(check(&model, &client).is_err());
    model.participants[0].topics.publishers[0].mode = TopicMode::Lossy;
    let prior_id = std::mem::replace(
        &mut model.participants[0].external_id,
        "other-publisher".into(),
    );
    assert!(check(&model, &client).is_err());
    model.participants[0].external_id = prior_id;
    model.participants[0].room = "other-room";
    client.snapshot.participants.clear();
    client.snapshot.publications.clear();
    client.snapshot.video_mapping.clear();
    assert!(
        model
            .check_client(&model.participants[1], &client, &[])
            .is_err()
    );
    client.data_subscribers.insert(registration, unbound);
    model
        .check_client(&model.participants[1], &client, &[])
        .unwrap();
}

#[test]
fn publishers_must_install_each_declared_data_track() {
    let (mut model, mut client) = fixture();
    for (topic, mode) in [("state", TopicMode::Latest), ("events", TopicMode::Lossy)] {
        let registration = TopicPublisher {
            topic: topic.into(),
            mode,
        };
        model.participants[1]
            .topics
            .publishers
            .push(registration.clone());
        client.data_publishers.insert(
            registration,
            DataPublicationState {
                observation_revision: 1,
                intent_revision: 1,
                evaluated_revision: 1,
                accepted_revision: Some(1),
                rejected_revision: None,
                track_id: topic.into(),
                delivery: mode.into(),
                local_enabled: true,
                closed: false,
                admission: PublicationAdmissionStatus::Admitted,
                admission_pending: false,
                error: None,
                execution: PublicationExecution::Installed,
                execution_error: None,
            },
        );
    }
    let requested = [model.participants[0].id.as_str().to_owned()];
    let check =
        |client: &ClientObservation| model.check_client(&model.participants[1], client, &requested);
    check(&client).unwrap();
    let state = client.data_publishers.values_mut().next().unwrap();
    state.execution = PublicationExecution::Unavailable;
    assert!(check(&client).is_err());
    let state = client.data_publishers.values_mut().next().unwrap();
    state.execution = PublicationExecution::Installed;
    state.track_id = "not-declared".into();
    assert!(check(&client).is_err());
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
    let canonical = publisher.publication_id(TrackKind::Video, "video");
    assert_eq!(canonical.as_str(), descriptors[0].id);
    descriptors.push(descriptors[0].clone());
    assert!(publisher.bind_media(&descriptors).is_err());
    descriptors.pop();
    descriptors[0].track_id = "wrong".into();
    assert!(!publisher.bind_media(&descriptors).unwrap());
    descriptors[0].track_id = "video".into();
    descriptors[0].participant_id = model.participants[1].id.as_str().to_owned();
    assert!(!publisher.bind_media(&descriptors).unwrap());
    descriptors[0].participant_id = publisher.id.as_str().to_owned();
    descriptors[0].kind = MediaKind::Audio;
    assert!(!publisher.bind_media(&descriptors).unwrap());
    descriptors[0].kind = MediaKind::Video;
    descriptors[0].id = "vid_04NCB5P75CY1FN9ZPW5ZQEXDNXG".into();
    assert!(publisher.bind_media(&descriptors).is_err());
}
