use super::*;
use crate::control::lifecycle::{TrackLifecycleOperation, TrackLifecycleOutcome};
use crate::control::permissions::ResourcePermissions;
use crate::control::topology::TrackAncestry;
use crate::entity::{ParticipantExternalId, TrackKind};
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
        room: RoomExternalId::new("sdp-scope").unwrap(),
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
    let room_external = RoomExternalId::new("sdp-scope").unwrap();
    let room = RoomId::from_external(&room_external);
    let participant = ParticipantId::new();
    let external = ParticipantExternalId::new("alice").unwrap();
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
            external.clone(),
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
            room: room_external.clone(),
            grants: GrantSet::default(),
        },
        ResourcePermissions::Grants {
            room: room_external.clone(),
            grants: grant_set("sdp-scope", Action::Publish),
        },
        ResourcePermissions::Grants {
            room: room_external,
            grants: grant_set("other", Action::Join),
        },
        ResourcePermissions::Grants {
            room: RoomExternalId::new("other").unwrap(),
            grants: grant_set("*", Action::Join),
        },
    ] {
        let mut lease = previous.clone();
        lease.permissions = permissions;
        assert!(!lease.allows_current_room());
        assert!(matches!(
            actor.begin_create_participant(
                ParticipantState {
                    manual_sub: true,
                    room_id: room,
                    participant_id: participant,
                    participant_external_id: external.clone(),
                    connection_id: connection_id(2),
                    old_connection_id: None,
                    authorization: Some(lease.clone()),
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
                external.clone(),
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

fn publishing_offer() -> String {
    let mut rtc = str0m::RtcConfig::new().build(std::time::Instant::now());
    rtc.add_local_candidate(
        str0m::Candidate::builder()
            .udp()
            .host("127.0.0.1:5000".parse().unwrap())
            .build()
            .unwrap(),
    );
    let mut change = rtc.sdp_api();
    change.add_media(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    change.add_media(
        str0m::media::MediaKind::Video,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    change.apply().unwrap().0.to_sdp_string()
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn whip_checks_all_canonical_publications_before_materialization_and_retains_scope_on_refresh()
 {
    let mut actor = actor();
    actor.negotiator = Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
        "127.0.0.1:6000".parse().unwrap(),
    )]);
    let room = RoomId::from_external(&RoomExternalId::new("sdp-scope").unwrap());
    let participant = ParticipantId::new();
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
    let state = |lease| ParticipantState {
        manual_sub: false,
        room_id: room,
        participant_id: participant,
        participant_external_id: ParticipantExternalId::new("alice").unwrap(),
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
    assert!(actor.core.registry.get_room_shard(&room).is_none());
    assert_eq!(actor.lifecycle.sources_in_room(room).count(), 0);
    assert!(actor.command_backlog.is_empty());
    lease.permissions = ResourcePermissions::Grants {
        room: RoomExternalId::new("sdp-scope").unwrap(),
        grants: GrantSet::new(
            [
                (GrantKind::Audio, "audio"),
                (GrantKind::Video, "video"),
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
        room,
        participant,
        connection_id(1),
        pending.shard,
        pending.partition_index
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
            crate::participant::whip_media_label(SignalingProtocol::Whip, media.kind),
            Some("audio" | "video")
        ));
    }
    actor
        .core
        .registry
        .release_partition(room, participant, connection_id(1));
    actor.core.release_transport(pending.transport, now);
}

pub(super) fn source(
    room: RoomId,
    origin: ParticipantId,
    connection: ConnectionId,
    kind: TrackKind,
    label: &str,
) -> Track {
    let meta = TrackMeta {
        room_id: room,
        shard_id: ShardId::new(0),
        id: connection.derive_track_id(kind, label),
        origin,
        connection_id: connection,
        label: Some(label.into()),
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
    let room = RoomId::from_external(&RoomExternalId::new("sdp-scope").unwrap());
    let publisher = ParticipantId::new();
    let other = ParticipantId::new();
    let subscriber = ParticipantId::new();
    let now = pulsebeam_runtime::time::Instant::now();
    let wall = SystemTime::now();
    let mut lease = authorization_lease(i64::MAX as u64, wall, now, "sdp-scope", room, subscriber);
    lease.permissions = permissions(
        vec![Action::Subscribe],
        Some("publisher"),
        Some(GrantKind::Audio),
        None,
    );
    for (index, (participant, external, shard)) in [
        (publisher, "publisher", 0),
        (other, "other", 0),
        (subscriber, "alice", 1),
    ]
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
                ParticipantExternalId::new(external).unwrap(),
                wall,
            )
            .unwrap();
        actor.core.registry.mark_materialized(&participant);
    }
    assert!(
        actor
            .lifecycle
            .subscribe_defaults(
                room,
                subscriber,
                [
                    (TrackSelector::audio(), SelectionPolicy::All),
                    (TrackSelector::video(), SelectionPolicy::Allocated)
                ],
                &actor.core.registry,
                now
            )
            .is_empty()
    );
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
        room,
        publisher,
        sources[3].id(),
        subscriber,
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
