use super::*;
use pulsebeam::entity::ParticipantExternalId;
use pulsebeam::id::ShardId;
use pulsebeam::route::{NodeRouteAddress, NodeTransportAddress, RouteId, TransportRoute};
use pulsebeam::sim_observation::*;
use pulsebeam::track::TrackMeta;
use pulsebeam_agent_native::agent_core::{DesiredState, Participant, Snapshot, VideoSubscription};

type Mutation<T> = (&'static str, fn(&mut T));

fn fixture() -> (ControlStateOracle, Observation, ClientObservation) {
    let room = RoomId::derive(
        &DEVELOPMENT_PROJECT_ID,
        &RoomExternalId::new("room").unwrap(),
    );
    let publisher = ParticipantId::derive(&room, &ParticipantExternalId::new("publisher").unwrap());
    let viewer = ParticipantId::derive(&room, &ParticipantExternalId::new("viewer").unwrap());
    let shard0 = ShardId::new(0);
    let shard1 = ShardId::new(1);
    let origin = NodeRouteAddress::new(RouteId::new(shard0, 0), 1);
    let destination = NodeRouteAddress::new(RouteId::new(shard1, 0), 1);
    let source_transport = NodeTransportAddress::new(TransportRoute::new(shard0, 0), 1);
    let receiver_transport = NodeTransportAddress::new(TransportRoute::new(shard1, 0), 1);
    let source_connection = ConnectionId::new();
    let receiver_connection = ConnectionId::new();
    let track = source_connection.derive_track_id(TrackKind::Video, "video");
    let model = ControlStateOracle {
        participants: vec![
            ExpectedParticipant {
                id: publisher,
                room: "room",
                video: true,
                video_label: "video".into(),
                audio: false,
                required: true,
                connection: Some(source_connection),
                topics: Default::default(),
                video_targets: HashSet::new(),
            },
            ExpectedParticipant {
                id: viewer,
                room: "room",
                video: false,
                video_label: "video".into(),
                audio: false,
                required: true,
                connection: Some(receiver_connection),
                topics: Default::default(),
                video_targets: HashSet::from([track]),
            },
        ],
    };
    let control = ControlObservation {
        generation: 1,
        shard_generations: vec![1, 1],
        participants: vec![
            ParticipantObservation {
                id: publisher,
                room,
                connection: source_connection,
                shard: shard0,
                transport: Some(source_transport),
            },
            ParticipantObservation {
                id: viewer,
                room,
                connection: receiver_connection,
                shard: shard1,
                transport: Some(receiver_transport),
            },
        ],
        tracks: vec![TrackObservation {
            meta: TrackMeta {
                room_id: room,
                shard_id: shard0,
                id: track,
                origin: publisher,
                connection_id: source_connection,
                label: Some("video".into()),
            },
            candidates: vec![publisher, viewer],
            active: vec![viewer],
        }],
        subscriptions: [publisher, viewer]
            .into_iter()
            .flat_map(|subscriber| {
                [TrackKind::Audio, TrackKind::Video]
                    .into_iter()
                    .map(move |kind| SubscriptionObservation {
                        room,
                        subscriber,
                        selector: SelectorObservation {
                            track: None,
                            publisher: None,
                            kind: Some(kind),
                            label: None,
                        },
                        selection: if kind == TrackKind::Video {
                            SelectionPolicy::Allocated
                        } else {
                            SelectionPolicy::All
                        },
                    })
            })
            .collect(),
        placements: vec![
            PlacementObservation {
                track,
                address: origin,
                resident: true,
                route_installed: true,
            },
            PlacementObservation {
                track,
                address: destination,
                resident: true,
                route_installed: true,
            },
        ],
    };
    let shards = vec![
        ShardObservation {
            shard: shard0,
            generation: 1,
            pending_generation: None,
            pending_effects: 0,
            participants: vec![ShardParticipantObservation {
                id: publisher,
                connection: source_connection,
                transport: source_transport,
                video: Default::default(),
                audio: Vec::new(),
            }],
            transports: vec![(source_transport, Some(publisher))],
            tracks: vec![track],
            plans: vec![PlanObservation {
                track: Some(track),
                local: vec![],
                remote: vec![destination],
                reverse: Some(origin),
            }],
            routes: vec![RouteObservation {
                address: origin,
                track: Some(track),
                reverse: true,
            }],
        },
        ShardObservation {
            shard: shard1,
            generation: 1,
            pending_generation: None,
            pending_effects: 0,
            participants: vec![ShardParticipantObservation {
                id: viewer,
                connection: receiver_connection,
                transport: receiver_transport,
                video: Default::default(),
                audio: Vec::new(),
            }],
            transports: vec![(receiver_transport, Some(viewer))],
            tracks: vec![track],
            plans: vec![PlanObservation {
                track: Some(track),
                local: vec![Some(viewer)],
                remote: vec![],
                reverse: Some(origin),
            }],
            routes: vec![RouteObservation {
                address: destination,
                track: Some(track),
                reverse: false,
            }],
        },
    ];
    let desired = DesiredState {
        revision: 1,
        connected: true,
        video: vec![VideoSubscription {
            slot: 0,
            track_id: track.as_str(),
            selector: None,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
            playout_delay: Default::default(),
        }],
        ..Default::default()
    };
    let publication = Publication {
        id: track.as_str(),
        participant_id: publisher.as_str(),
        kind: MediaKind::Video,
        label: "video".into(),
    };
    let snapshot = Snapshot {
        participant_id: Some(viewer.as_str()),
        room_external_id: Some("room".into()),
        participants: BTreeMap::from([(
            publisher.as_str(),
            Participant {
                id: publisher.as_str(),
                external_id: "publisher".into(),
            },
        )]),
        publications: BTreeMap::from([(track.as_str(), publication)]),
        accepted_intent_revision: 1,
        desired_revision: 1,
        video_mapping: BTreeMap::from([(0, track.as_str())]),
        ..Default::default()
    };
    (
        model,
        Observation { control, shards },
        ClientObservation {
            ordered_cursors: Vec::new(),
            admission: Some(receiver_connection),
            snapshot,
            sent_intent_revision: Some(1),
            video_operation: 0,
            accepted: Some(desired.clone()),
            submitted: desired,
            video_receivers: BTreeSet::from([0]),
            audio_receivers: BTreeSet::new(),
            video_capacity: 1,
            audio_capacity: 0,
        },
    )
}

fn audio_fixture(capacity: u32) -> (ControlStateOracle, ClientObservation) {
    let (mut model, _, mut client) = fixture();
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
    client.audio_receivers = (0..capacity).collect();
    client.snapshot.audio_mapping = (0..capacity)
        .zip(
            model
                .participants
                .iter()
                .filter(|peer| peer.audio)
                .map(|peer| peer.track_id(TrackKind::Audio, "audio").as_str()),
        )
        .collect();
    client.accepted.as_mut().unwrap().audio.automatic = true;
    client.submitted.audio.automatic = true;
    (model, client)
}

#[test]
fn automatic_audio_oracle_requires_every_uncontended_live_source() {
    let (mut model, mut client) = audio_fixture(3);
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&[], &model.participants[1], &client, &requested)
        .unwrap();
    model.participants.last_mut().unwrap().required = false;
    client.snapshot.audio_mapping.remove(&2);
    model
        .check_client(&[], &model.participants[1], &client, &requested)
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
                    &[],
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
    let (model, mut client) = audio_fixture(2);
    let requested = [model.participants[0].id.as_str()];
    model
        .check_client(&[], &model.participants[1], &client, &requested)
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
        .check_client(&[], &model.participants[1], &client, &requested)
        .unwrap();
    let second = client.snapshot.audio_mapping.remove(&1).unwrap();
    assert!(
        model
            .check_client(&[], &model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.audio_mapping.insert(1, second);
    client.accepted.as_mut().unwrap().audio.pinned = vec![first.clone()];
    client.submitted.audio.pinned = vec![first.clone()];
    assert!(
        model
            .check_client(&[], &model.participants[1], &client, &requested)
            .is_err()
    );
    client.snapshot.audio_mapping.insert(0, first);
    model
        .check_client(&[], &model.participants[1], &client, &requested)
        .unwrap();
}

#[test]
fn transition_safety_allows_outstanding_intent_but_rejects_invalid_assignments() {
    let (model, _, mut client) = fixture();
    let allowed = model.participants.iter().map(|p| p.id.as_str()).collect();
    client.sent_intent_revision = Some(2);
    client.accepted = None;
    ControlStateOracle::check_client_safety(&client, &allowed).unwrap();
    assert!(
        model
            .check_client(
                &[],
                &model.participants[1],
                &client,
                &[model.participants[0].id.as_str()]
            )
            .is_err()
    );
    client
        .snapshot
        .video_mapping
        .insert(0, "retired-track".into());
    assert!(ControlStateOracle::check_client_safety(&client, &allowed).is_err());
    let (_, _, client) = fixture();
    assert!(ControlStateOracle::check_client_safety(&client, &BTreeSet::new()).is_err());
}

#[test]
fn application_and_wire_revisions_are_independent() {
    let (model, _, mut client) = fixture();
    client.sent_intent_revision = Some(4);
    client.snapshot.accepted_intent_revision = 4;
    model
        .check_client(
            &[],
            &model.participants[1],
            &client,
            &[model.participants[0].id.as_str()],
        )
        .unwrap();
}

#[test]
fn complete_plan_and_owner_values_agree() {
    let (model, owners, client) = fixture();
    model.check_owners(&owners).unwrap();
    model
        .check_client(
            &[],
            &model.participants[1],
            &client,
            &[model.participants[0].id.as_str()],
        )
        .unwrap();
}

#[test]
fn owner_oracle_rejects_missing_duplicate_cross_room_and_retired_values() {
    let mutations: &[Mutation<Observation>] = &[
        ("missing publication", |state| {
            state.control.tracks.clear();
        }),
        ("duplicate publication", |state| {
            state.control.tracks.push(state.control.tracks[0].clone());
        }),
        ("wrong publication kind", |state| {
            let meta = &mut state.control.tracks[0].meta;
            meta.id = meta
                .connection_id
                .derive_track_id(TrackKind::Audio, "video");
        }),
        ("stale publication connection", |state| {
            state.control.tracks[0].meta.connection_id = ConnectionId::new();
        }),
        ("cross room", |state| {
            state.control.participants[0].room = RoomId::derive(
                &DEVELOPMENT_PROJECT_ID,
                &RoomExternalId::new("other").unwrap(),
            );
        }),
        ("missing control owner", |state| {
            state.control.participants.pop();
        }),
        ("missing shard participant", |state| {
            state.shards[1].participants.clear();
        }),
        ("stale connection", |state| {
            state.shards[1].participants[0].connection = ConnectionId::new();
        }),
        ("stale transport", |state| {
            state.shards[1].transports[0].0 =
                NodeTransportAddress::new(TransportRoute::new(ShardId::new(1), 0), 2);
        }),
        ("missing runtime", |state| {
            state.shards[1].tracks.clear();
        }),
        ("missing binding", |state| {
            state.shards[1].plans[0].local.clear();
        }),
        ("duplicate binding", |state| {
            let member = state.shards[1].plans[0].local[0];
            state.shards[1].plans[0].local.push(member);
        }),
        ("missing remote route", |state| {
            state.shards[0].plans[0].remote.clear();
        }),
        ("stale epoch", |state| {
            let route = &mut state.shards[1].routes[0];
            route.address = NodeRouteAddress::new(route.address.route, 2);
        }),
        ("missing reverse route", |state| {
            state.shards[1].plans[0].reverse = None;
        }),
        ("missing destination placement", |state| {
            state.control.placements.pop();
        }),
        ("incomplete generation", |state| {
            state.shards[1].pending_generation = Some(2);
        }),
        ("stale applied generation", |state| {
            state.shards[1].generation = 0;
        }),
    ];
    for (name, mutate) in mutations {
        let (model, mut state, _) = fixture();
        mutate(&mut state);
        assert!(model.check_owners(&state).is_err(), "accepted {name}");
    }
}

#[test]
fn client_oracle_rejects_wrong_missing_duplicate_and_unrequested_assignments() {
    let mutations: &[Mutation<ClientObservation>] = &[
        ("wrong desired revision", |state| {
            state.snapshot.desired_revision += 1;
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
            state.video_receivers.insert(1);
        }),
        ("stale assignment", |state| {
            state
                .snapshot
                .video_mapping
                .insert(0, "retired-track".into());
        }),
        ("unnegotiated receiver", |state| {
            let track = state.snapshot.video_mapping.remove(&0).unwrap();
            state.snapshot.video_mapping.insert(5, track);
        }),
        ("unaccepted revision", |state| {
            state.snapshot.accepted_intent_revision = 2;
        }),
        ("unrequested assignment", |state| {
            state.accepted.as_mut().unwrap().video.clear();
            state.submitted.video.clear();
        }),
    ];
    for (name, mutate) in mutations {
        let (model, _, mut state) = fixture();
        mutate(&mut state);
        assert!(
            model
                .check_client(
                    &[],
                    &model.participants[1],
                    &state,
                    &[model.participants[0].id.as_str()]
                )
                .is_err(),
            "accepted {name}"
        );
    }
}
