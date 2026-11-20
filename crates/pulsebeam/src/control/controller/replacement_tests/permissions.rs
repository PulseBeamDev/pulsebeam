use super::*;
use crate::control::lifecycle::{TrackLifecycleOperation, TrackLifecycleOutcome};
use crate::control::permissions::ResourcePermissions;
use crate::control::topology::TrackAncestry;
use crate::entity::{ParticipantId, TrackKind};
use crate::participant::ParticipantEffect;
use crate::track::{Track, TrackMeta};
use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind as GrantKind};

fn permissions(
    actions: Vec<Action>,
    publisher: Option<&str>,
    kind: Option<GrantKind>,
    label: Option<&str>,
) -> ResourcePermissions {
    ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("sdp-scope").unwrap()),
        grants: GrantSet::new(vec![
            Grant::new(
                vec!["sdp-scope".into()],
                vec![Action::Join],
                None,
                None,
                None,
            )
            .unwrap(),
            Grant::new(
                vec!["sdp-scope".into()],
                actions,
                publisher.map(str::to_owned),
                kind,
                label.map(str::to_owned),
            )
            .unwrap(),
        ])
        .unwrap(),
    }
}

#[test]
fn current_room_join_is_checked_before_preparation_and_commit() {
    let mut actor = actor();
    let room_external = RoomId::new("sdp-scope").unwrap();
    let room = room_external;
    let participant = crate::entity::fixture_participant_id();
    let external = ParticipantId::new("alice").unwrap();
    let now = pulsebeam_runtime::time::Instant::now();
    let wall = SystemTime::now();
    let previous = authorization_lease(i64::MAX as u64, wall, now, "sdp-scope", room, participant);
    assert!(previous.allows_current_room());
    let transport = actor.core.reserve_transport(ShardId::new(0), now);
    actor
        .commit_reserved_candidate(
            participant,
            room,
            ShardId::new(0),
            0,
            transport,
            connection_id(1),
            Some(previous.clone()),
            SignalingProtocol::State,
            wall,
        )
        .unwrap();
    let grant_set = |rooms: &str, action| {
        GrantSet::new(vec![
            Grant::new(vec![rooms.into()], vec![action], None, None, None).unwrap(),
        ])
        .unwrap()
    };
    for permissions in [
        ResourcePermissions::Denied,
        ResourcePermissions::Grants {
            joined_room: Some(room_external),
            grants: GrantSet::default(),
        },
        ResourcePermissions::Grants {
            joined_room: Some(room_external),
            grants: grant_set("sdp-scope", Action::Publish),
        },
        ResourcePermissions::Grants {
            joined_room: Some(room_external),
            grants: grant_set("other", Action::Join),
        },
        ResourcePermissions::Grants {
            joined_room: Some(RoomId::new("other").unwrap()),
            grants: grant_set("*", Action::Join),
        },
    ] {
        let mut lease = previous.clone();
        lease.permissions = permissions;
        assert!(!lease.allows_current_room());
        assert!(matches!(
            actor.begin_create_participant(
                ParticipantState {
                    room_id: Some(room),
                    participant_id: participant,
                    connection_id: connection_id(2),
                    old_connection_id: None,
                    authorization: Some(lease.clone()),
                    selection: crate::control::sdp::Selection::Native,
                    profile: SignalingProtocol::State,
                    rtp_capabilities: None,
                },
                "not an offer".into()
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        let candidate_transport = actor.core.reserve_transport(ShardId::new(0), now);
        assert!(matches!(
            actor.commit_reserved_candidate(
                participant,
                room,
                ShardId::new(0),
                0,
                candidate_transport,
                connection_id(2),
                Some(lease),
                SignalingProtocol::State,
                wall
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        actor.core.release_transport(candidate_transport, now);
        let current = actor.core.registry.get_participant(&participant).unwrap();
        assert_eq!(current.connection_id, connection_id(1));
        assert_eq!(current.authorization.as_ref(), Some(&previous));
        assert_eq!(actor.authorization_expiries.len(), 1);
        assert!(actor.command_backlog.is_empty());
        assert!(actor.native_connections.is_empty());
    }
}

pub(super) fn publishing_offer() -> String {
    sdp_offer(str0m::media::Direction::SendOnly)
}

fn sdp_offer(direction: str0m::media::Direction) -> String {
    let mut rtc = str0m::RtcConfig::new().build(std::time::Instant::now());
    rtc.add_local_candidate(
        str0m::Candidate::builder()
            .udp()
            .host("127.0.0.1:5000".parse().unwrap())
            .build()
            .unwrap(),
    );
    let mut change = rtc.sdp_api();
    change.add_media(str0m::media::MediaKind::Audio, direction, None, None, None);
    change.add_media(str0m::media::MediaKind::Video, direction, None, None, None);
    change.apply().unwrap().0.to_sdp_string()
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn whip_checks_all_canonical_publications_before_materialization_and_retains_scope_on_refresh()
 {
    let mut actor = actor();
    actor.negotiator = Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
        "127.0.0.1:6000".parse().unwrap(),
    )]);
    let room = RoomId::new("sdp-scope").unwrap();
    let participant = crate::entity::fixture_participant_id();
    let now = pulsebeam_runtime::time::Instant::now();
    let wall = SystemTime::now();
    let mut lease = authorization_lease(i64::MAX as u64, wall, now, "sdp-scope", room, participant);
    lease.permissions = permissions(
        vec![Action::Publish],
        None,
        Some(GrantKind::Video),
        Some("video"),
    );
    let refreshed = lease.refresh_at(wall, now).unwrap();
    assert_eq!(refreshed.permissions(), lease.permissions());
    let (_, names) =
        crate::control::sdp::whip_query(Some("room=sdp-scope&track=video.camera&track=audio.mic"))
            .unwrap();
    let state = |lease| ParticipantState {
        selection: crate::control::sdp::Selection::Whip(names.clone()),
        room_id: Some(room),
        participant_id: participant,
        connection_id: connection_id(1),
        old_connection_id: None,
        authorization: Some(lease),
        profile: SignalingProtocol::Whip,
        rtp_capabilities: None,
    };
    let rejected = actor
        .begin_create_participant(state(refreshed), publishing_offer())
        .err()
        .expect("disallowed offer must not materialize");
    assert!(
        matches!(rejected, ControllerError::AuthorizationRejected),
        "{rejected:?}"
    );
    assert!(actor.core.registry.get_participant(&participant).is_none());
    assert!(
        actor
            .core
            .registry
            .get_room_shard(&ProjectRoomKey::fixture(room))
            .is_none()
    );
    assert_eq!(
        actor
            .lifecycle
            .sources_in_room(ProjectRoomKey::fixture(room))
            .count(),
        0
    );
    assert!(actor.command_backlog.is_empty());
    lease.permissions = ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("sdp-scope").unwrap()),
        grants: GrantSet::new(
            [
                (GrantKind::Audio, "mic"),
                (GrantKind::Video, "camera"),
                (GrantKind::Data, "v2/latest/chat"),
            ]
            .into_iter()
            .map(|(kind, label)| {
                Grant::new(
                    vec!["sdp-scope".into()],
                    vec![Action::Publish],
                    None,
                    Some(kind),
                    Some(label.into()),
                )
                .unwrap()
            })
            .chain(std::iter::once(
                Grant::new(
                    vec!["sdp-scope".into()],
                    vec![Action::Join],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            ))
            .collect(),
        )
        .unwrap(),
    };
    let pending = actor
        .begin_create_participant(state(lease), publishing_offer())
        .unwrap();
    assert!(actor.core.registry.partition_reserved(
        pending.scope.joined_room().unwrap(),
        participant,
        connection_id(1),
        pending.shard,
        pending.partition_index.unwrap()
    ));
    let Some(ShardCommand::MaterializeParticipant { config, .. }) = pending.command.as_ref() else {
        panic!("authorized materialization")
    };
    assert_eq!(config.resources.media_sections().len(), 2);
    assert!(config.topic_publications.allows("v2/latest/chat"));
    for denied in [
        "v2/latest/other",
        "v2/ordered/chat",
        "v2/latest/pub/chat",
        "audio",
        "video",
    ] {
        assert!(!config.topic_publications.allows(denied));
    }
    assert!(
        ResourcePermissions::RoomBound
            .topic_publications()
            .allows("v2/latest/other")
    );
    assert!(
        !ResourcePermissions::Denied
            .topic_publications()
            .allows("v2/latest/chat")
    );
    for media in config.resources.media_sections() {
        assert!(matches!(
            media
                .publish_name
                .as_ref()
                .map(crate::entity::TrackId::as_str),
            Some("mic" | "camera")
        ));
    }
    actor.core.registry.release_partition(
        pending.scope.joined_room().unwrap(),
        participant,
        connection_id(1),
    );
    actor.core.release_transport(pending.transport, now);
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn whep_authorizes_the_complete_roomless_selection_and_routes_only_exact_publications() {
    use crate::control::sdp::{self, Selection};
    let mut actor = actor();
    let shard = ShardId::new(0);
    let (writer, mut updates) = new_shard_update(shard);
    actor.updates[0] = writer;
    actor.negotiator = Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
        "127.0.0.1:6000".parse().unwrap(),
    )]);
    let audio_room = RoomId::new("studio").unwrap();
    let video_room = RoomId::new("stage").unwrap();
    let audio_owner = ParticipantId::new("audio-owner").unwrap();
    let video_owner = ParticipantId::new("video-owner").unwrap();
    let listener = ParticipantId::new("listener").unwrap();
    let listener_connection = connection_id(3);
    let now = pulsebeam_runtime::time::Instant::now();
    let wall = SystemTime::now();
    let offer = sdp_offer(str0m::media::Direction::RecvOnly);
    let codec_probe = actor.core.reserve_transport(shard, now);
    let (rtc, _, _) = actor
        .negotiator
        .create_answer(
            offer.clone(),
            IceUfrag::new(0, 0, codec_probe.route, codec_probe.epoch).into_ice_creds(),
            pulsebeam_runtime::net::ReceiveTime::now(),
            None,
        )
        .unwrap();
    actor.core.release_transport(codec_probe, now);

    let mut publications = Vec::new();
    for (index, room, owner, kind, labels) in [
        (
            1,
            audio_room,
            audio_owner,
            TrackKind::Audio,
            ["ambience", "mic"],
        ),
        (
            2,
            video_room,
            video_owner,
            TrackKind::Video,
            ["backdrop", "camera"],
        ),
    ] {
        let address = actor.core.reserve_transport(shard, now);
        let connection = connection_id(index);
        let expiry = wall.duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
        let publisher = authorization_lease(expiry, wall, now, room.as_str(), room, owner);
        actor
            .commit_reserved_candidate(
                owner,
                room,
                shard,
                0,
                address,
                connection,
                Some(publisher),
                SignalingProtocol::State,
                wall,
            )
            .unwrap();
        actor.core.registry.mark_materialized(&owner);
        let media_kind = if kind == TrackKind::Audio {
            pulsebeam_rtc::MediaKind::Audio
        } else {
            pulsebeam_rtc::MediaKind::Video
        };
        let format = sdp::codec_format(
            rtc.session
                .senders
                .iter()
                .find(|sender| sender.kind == media_kind)
                .unwrap()
                .codec,
        )
        .unwrap();
        for label in labels {
            let mut track = source(room, owner, connection, kind, label);
            match &mut track {
                Track::Audio(audio) => audio.format = Some(format.clone()),
                Track::Video(video) => video.format = Some(format.clone()),
                Track::Data(_) => unreachable!(),
            }
            publications.push(track.id());
            actor.handle_shard_event((shard, ShardEvent::TrackPublished(track)));
        }
    }
    while updates.try_recv().is_ok() {}

    let grant = |room: RoomId, publisher: ParticipantId, kind, label: &str| {
        Grant::new(
            vec![room.as_str().to_owned()],
            vec![Action::Subscribe],
            Some(publisher.as_str().to_owned()),
            Some(kind),
            Some(label.to_owned()),
        )
        .unwrap()
    };
    let state = |selector: &str| {
        let authorization = VerifiedAuthorization {
            token_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
            grants: GrantSet::new(vec![
                grant(audio_room, audio_owner, GrantKind::Audio, "mic"),
                grant(video_room, video_owner, GrantKind::Video, "camera"),
            ])
            .unwrap(),
            project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room_id: None,
            participant_id: listener,
            expiry: AuthorizationExpiry::from_unix_seconds(
                wall.duration_since(UNIX_EPOCH).unwrap().as_secs() + 60,
            ),
        };
        ParticipantState {
            room_id: None,
            participant_id: listener,
            connection_id: listener_connection,
            old_connection_id: None,
            authorization: Some(
                AuthorizationLease::from_verified(authorization, wall, now).unwrap(),
            ),
            selection: Selection::Whep(sdp::whep_query(Some(selector)).unwrap()),
            profile: SignalingProtocol::Whep,
            rtp_capabilities: None,
        }
    };
    let selection = "track=studio.audio-owner.audio.mic&track=stage.video-owner.video.camera";
    assert!(matches!(
        actor.begin_create_participant(
            state("track=studio.audio-owner.audio.mic&track=stage.video-owner.video.secret"),
            offer.clone(),
        ),
        Err(ControllerError::AuthorizationRejected)
    ));
    assert!(
        actor
            .core
            .registry
            .get_connection(listener_connection)
            .is_none()
    );
    let pending = actor
        .begin_create_participant(state(selection), offer)
        .unwrap();
    assert!(pending.scope.room.is_none());
    let Some(ShardCommand::MaterializeParticipant { config, .. }) = pending.command.as_ref() else {
        panic!("authorized WHEP materialization")
    };
    for (media, expected) in config
        .resources
        .media_sections()
        .iter()
        .zip([publications[1], publications[3]])
    {
        assert_eq!(media.receive_publication, Some(expected));
    }
    actor.commit_materialization(pending, wall).unwrap();
    let mut forwarded = Vec::new();
    while let Ok(update) = updates.try_recv() {
        forwarded.extend(update.plans.iter().filter_map(|entry| {
            entry
                .plan
                .as_ref()?
                .local
                .contains(&listener_connection)
                .then_some(entry.publication_id)
        }));
    }
    assert_eq!(forwarded, [publications[1], publications[3]]);
    let (_, receiver) = actor
        .core
        .registry
        .get_connection(listener_connection)
        .unwrap();
    assert!(receiver.scope.room.is_none());
    assert_eq!(receiver.profile, SignalingProtocol::Whep);
}

pub(super) fn source(
    room: RoomId,
    origin: ParticipantId,
    connection: ConnectionId,
    kind: TrackKind,
    label: &str,
) -> Track {
    let meta = TrackMeta {
        room: crate::entity::ProjectRoomKey::fixture(room),
        id: connection.fixture_publication(kind, label),
        origin,
        connection_id: connection,
        track_id: Some(crate::entity::TrackId::new(&label).unwrap()),
    };
    match kind {
        TrackKind::Audio => Track::audio(meta, None),
        TrackKind::Video => Track::video(meta, Vec::new(), None),
        TrackKind::Data => Track::data_publication(meta, crate::track::DataLane::Ordered),
    }
}

fn subscriber_effects(
    outcome: &TrackLifecycleOutcome,
    subscriber: ParticipantId,
) -> Vec<&ParticipantEffect> {
    outcome
        .operations
        .iter()
        .filter_map(|operation| match operation {
            TrackLifecycleOperation::ParticipantEffect {
                participant,
                effect,
                ..
            } if *participant == subscriber => Some(effect),
            _ => None,
        })
        .collect()
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn default_subscriptions_filter_candidates_and_routes_without_destroying_broad_intent() {
    let mut actor = actor_with_shards(2);
    let room = RoomId::new("sdp-scope").unwrap();
    let publisher = ParticipantId::new("publisher").unwrap();
    let other = ParticipantId::new("other").unwrap();
    let subscriber = ParticipantId::new("alice").unwrap();
    let now = pulsebeam_runtime::time::Instant::now();
    let wall = SystemTime::now();
    let mut lease = authorization_lease(i64::MAX as u64, wall, now, "sdp-scope", room, subscriber);
    lease.permissions = permissions(
        vec![Action::Subscribe],
        Some("publisher"),
        Some(GrantKind::Audio),
        None,
    );
    for (index, (participant, shard)) in [(publisher, 0), (other, 0), (subscriber, 1)]
        .into_iter()
        .enumerate()
    {
        let shard = ShardId::new(shard);
        let transport = actor.core.reserve_transport(shard, now);
        actor
            .commit_reserved_candidate(
                participant,
                room,
                shard,
                index,
                transport,
                connection_id(u8::try_from(index + 1).unwrap()),
                (participant == subscriber).then(|| lease.clone()),
                if participant == subscriber {
                    SignalingProtocol::Whep
                } else {
                    SignalingProtocol::State
                },
                wall,
            )
            .unwrap();
        actor.core.registry.mark_materialized(&participant);
    }
    for (selector, selection) in [
        (TrackSelector::audio(), SelectionPolicy::All),
        (TrackSelector::video(), SelectionPolicy::Allocated),
    ] {
        assert!(
            actor
                .lifecycle
                .subscribe(
                    ProjectRoomKey::fixture(room),
                    subscriber,
                    selector,
                    selection,
                    &actor.core.registry,
                    now
                )
                .is_empty()
        );
    }
    let connection = connection_id(1);
    let sources = [
        source(room, publisher, connection, TrackKind::Audio, "mic"),
        source(room, publisher, connection, TrackKind::Audio, "fx"),
        source(room, other, connection_id(2), TrackKind::Audio, "mic"),
        source(room, publisher, connection, TrackKind::Video, "camera"),
    ];
    for (index, track) in sources.iter().enumerate() {
        let outcome = actor
            .lifecycle
            .publish(track.clone(), &actor.core.registry, now)
            .unwrap();
        let effects = subscriber_effects(&outcome, subscriber);
        assert_eq!(
            effects
                .iter()
                .any(|effect| matches!(effect, ParticipantEffect::TrackCandidateAdded { .. })),
            index < 2
        );
        assert_eq!(
            effects
                .iter()
                .any(|effect| matches!(effect, ParticipantEffect::TrackSubscribed { .. })),
            index < 2
        );
        if index >= 2 {
            assert!(outcome.operations.iter().all(|operation| match operation {
                TrackLifecycleOperation::Update { shard, .. }
                | TrackLifecycleOperation::Plans { shard, .. }
                | TrackLifecycleOperation::ParticipantEffect { shard, .. } =>
                    *shard != ShardId::new(1),
            }));
        }
    }
    lease.permissions = permissions(
        vec![Action::Subscribe],
        Some("publisher"),
        Some(GrantKind::Audio),
        Some("mic"),
    );
    lease.revision += 1;
    actor
        .core
        .registry
        .update_authorization(&subscriber, connection_id(3), lease.clone());
    assert_eq!(
        actor
            .core
            .registry
            .get_participant(&subscriber)
            .unwrap()
            .authorization
            .as_ref(),
        Some(&lease)
    );
    let removed = actor.lifecycle.reconcile(
        TrackAncestry::from_track(&sources[1]),
        &actor.core.registry,
        now,
    );
    let effects = subscriber_effects(&removed, subscriber);
    let cutoff = effects
        .iter()
        .position(|effect| matches!(effect, ParticipantEffect::TrackUnsubscribed { .. }))
        .unwrap();
    let removal = effects
        .iter()
        .position(|effect| matches!(effect, ParticipantEffect::TrackCandidateRemoved { .. }))
        .unwrap();
    assert!(cutoff < removal);
    let preserved = actor.lifecycle.reconcile(
        TrackAncestry::from_track(&sources[0]),
        &actor.core.registry,
        now,
    );
    assert!(subscriber_effects(&preserved, subscriber).is_empty());
    let denied = actor.lifecycle.activate(
        ProjectRoomKey::fixture(room),
        publisher,
        sources[3].id(),
        EndpointKey {
            room: ProjectRoomKey::fixture(room),
            participant: subscriber,
        },
        &actor.core.registry,
        now,
    );
    assert!(subscriber_effects(&denied, subscriber).is_empty());
    lease.permissions = permissions(
        vec![Action::Subscribe],
        Some("publisher"),
        Some(GrantKind::Audio),
        None,
    );
    lease.revision += 1;
    actor
        .core
        .registry
        .update_authorization(&subscriber, connection_id(3), lease.clone());
    assert_eq!(
        actor
            .core
            .registry
            .get_participant(&subscriber)
            .unwrap()
            .authorization
            .as_ref(),
        Some(&lease)
    );
    let restored = actor.lifecycle.reconcile(
        TrackAncestry::from_track(&sources[1]),
        &actor.core.registry,
        now,
    );
    let effects = subscriber_effects(&restored, subscriber);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, ParticipantEffect::TrackCandidateAdded { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, ParticipantEffect::TrackSubscribed { .. }))
    );
}
