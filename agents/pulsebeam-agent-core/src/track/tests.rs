use super::*;
use crate::TopicMessage;

fn video(label: &str) -> TrackSpec {
    TrackSpec::Video {
        label: label.into(),
    }
}

fn data(delivery: TopicMode) -> TrackSpec {
    TrackSpec::Data {
        label: "events".into(),
        delivery,
    }
}

fn message(
    delivery: TopicMode,
    participant: &str,
    track: &str,
    sequence: u64,
    bytes: &[u8],
) -> TopicNotification {
    TopicNotification::Message(TopicMessage {
        topic: "events".into(),
        mode: delivery,
        data_track_id: track.into(),
        sequence,
        participant_id: participant.into(),
        payload: bytes.into(),
    })
}

#[test]
fn descriptors_preserve_wire_identity_and_delivery() {
    assert_eq!(video("camera"), video("camera"));
    assert_ne!(
        video("camera"),
        TrackSpec::Audio {
            label: "camera".into()
        }
    );
    assert_ne!(data(TopicMode::Latest), data(TopicMode::Ordered));
    for label in ["", "with/slash", "control\n", &"x".repeat(65)] {
        assert!(video(label).validate().is_err(), "{label:?}");
    }
    assert!(video("é").validate().is_ok());
    assert!(
        TrackSpec::Data {
            label: "x".repeat(85),
            delivery: TopicMode::Ordered
        }
        .validate()
        .is_ok()
    );
    assert!(
        TrackSpec::Data {
            label: "with/slash".into(),
            delivery: TopicMode::Ordered
        }
        .validate()
        .is_err()
    );
}

#[test]
fn publication_claim_is_atomic_exclusive_and_reuses_released_capacity() {
    let topology = MediaTopology {
        local_video: 1,
        ..MediaTopology::default()
    };
    let mut resources = TrackResources::default();
    let first = TrackOwner(1);
    let second = TrackOwner(2);
    let spec = video("camera");
    assert_eq!(
        resources
            .claim_publication(first, spec.clone(), &topology, &BTreeSet::new())
            .unwrap(),
        "v0"
    );
    resources.update_publication(first, &spec, true).unwrap();
    assert_eq!(
        resources.claim_publication(second, spec.clone(), &topology, &BTreeSet::new()),
        Err(TrackResourceError::AlreadyOwned)
    );
    assert_eq!(
        resources.claim_publication(second, video("screen"), &topology, &BTreeSet::new()),
        Err(TrackResourceError::Capacity {
            kind: MediaKind::Video
        })
    );
    assert_eq!(resources.release_publication(second, &spec), None);
    assert_eq!(resources.publications()[0].label, "camera");
    assert_eq!(
        resources.release_publication(first, &spec).as_deref(),
        Some("v0")
    );
    assert_eq!(
        resources.update_publication(first, &spec, true),
        Err(TrackResourceError::Released)
    );
    assert_eq!(
        resources
            .claim_publication(second, video("screen"), &topology, &BTreeSet::new())
            .unwrap(),
        "v0"
    );
    assert!(resources.publications().is_empty());
}

#[test]
fn new_lifetime_fences_stale_source_updates_and_cleanup_after_reacquisition() {
    let mut resources = TrackResources::default();
    let topology = MediaTopology::default();
    let spec = video("camera");
    let previous = resources.new_owner().unwrap();
    resources
        .claim_publication(previous, spec.clone(), &topology, &BTreeSet::new())
        .unwrap();
    resources.release_publication(previous, &spec).unwrap();
    let current = resources.new_owner().unwrap();
    resources
        .claim_publication(current, spec.clone(), &topology, &BTreeSet::new())
        .unwrap();
    assert_eq!(
        resources.update_publication(previous, &spec, true),
        Err(TrackResourceError::Released)
    );
    assert_eq!(resources.release_publication(previous, &spec), None);
    assert!(resources.publications().is_empty());
    resources.update_publication(current, &spec, true).unwrap();
    assert_eq!(resources.publications().len(), 1);
}

#[test]
fn source_withdrawal_keeps_the_declaration_and_audio_cleanup_is_owner_scoped() {
    let mut resources = TrackResources::default();
    let topology = MediaTopology {
        local_video: 1,
        ..MediaTopology::default()
    };
    let first = TrackOwner(1);
    let second = TrackOwner(2);
    resources
        .claim_publication(first, video("camera"), &topology, &BTreeSet::new())
        .unwrap();
    resources
        .update_publication(first, &video("camera"), true)
        .unwrap();
    resources
        .update_publication(first, &video("camera"), false)
        .unwrap();
    assert!(resources.publications().is_empty());
    assert!(matches!(
        resources.claim_publication(second, video("screen"), &topology, &BTreeSet::new()),
        Err(TrackResourceError::Capacity { .. })
    ));
    resources.claim_audio(first).unwrap();
    assert_eq!(
        resources.claim_audio(second),
        Err(TrackResourceError::AlreadyOwned)
    );
    assert!(!resources.release_audio(second));
    assert!(resources.release_audio(first));
    resources.claim_audio(second).unwrap();
    assert!(!resources.release_audio(first));
}

#[test]
fn video_views_share_capacity_aggregate_visible_demand_and_preserve_incumbents() {
    let mut resources = TrackResources::default();
    let first = resources.new_owner().unwrap();
    let second = resources.new_owner().unwrap();
    let competing = resources.new_owner().unwrap();
    let selector = TrackSelector {
        participant_external_id: "alice".into(),
        label: "camera".into(),
    };
    let other = TrackSelector {
        participant_external_id: "bob".into(),
        label: "camera".into(),
    };
    let tracks = BTreeMap::from([
        (
            "alice-track".into(),
            RemoteTrack {
                id: "alice-track".into(),
                participant_id: "alice".into(),
                spec: video("camera"),
            },
        ),
        (
            "bob-track".into(),
            RemoteTrack {
                id: "bob-track".into(),
                participant_id: "bob".into(),
                spec: video("camera"),
            },
        ),
    ]);
    resources.claim_video(first, selector.clone()).unwrap();
    resources.claim_video(second, selector.clone()).unwrap();
    let mut observation = VideoObservation {
        visible: true,
        height: 360,
        min_height: 180,
        min_fps: 15,
        priority: 2,
        playout_delay: PlayoutDelay::Fixed {
            min_ms: 20,
            max_ms: 80,
        },
    };
    resources.observe_video(first, observation.clone()).unwrap();
    observation.height = 720;
    observation.min_height = 240;
    observation.min_fps = 30;
    observation.priority = 1;
    observation.playout_delay = PlayoutDelay::Fixed {
        min_ms: 30,
        max_ms: 120,
    };
    resources
        .observe_video(second, observation.clone())
        .unwrap();
    let projection = resources.video_projection(&tracks, 1);
    assert_eq!(
        projection.subscriptions,
        vec![VideoSubscription {
            slot: 0,
            track_id: String::new(),
            selector: Some(selector.clone()),
            height: 720,
            min_height: 240,
            min_fps: 30,
            priority: 2,
            playout_delay: PlayoutDelay::Fixed {
                min_ms: 20,
                max_ms: 120
            },
        }]
    );
    resources.claim_video(competing, other.clone()).unwrap();
    resources
        .observe_video(competing, observation.clone())
        .unwrap();
    let projection = resources.video_projection(&tracks, 1);
    assert_eq!(projection.subscriptions[0].selector, Some(selector));
    assert_eq!(projection.unmapped, vec![other.clone()]);
    observation.visible = false;
    resources.observe_video(first, observation.clone()).unwrap();
    resources
        .observe_video(second, observation.clone())
        .unwrap();
    let projection = resources.video_projection(&tracks, 1);
    assert_eq!(projection.subscriptions[0].selector, Some(other));
    assert!(projection.unmapped.is_empty());
    resources.release_video(competing);
    assert_eq!(
        resources.observe_video(competing, observation),
        Err(TrackResourceError::Released)
    );
    assert!(
        resources
            .video_projection(&tracks, 1)
            .subscriptions
            .is_empty()
    );
}

#[test]
fn absent_and_hidden_video_demand_does_not_allocate_and_republication_uses_logical_identity() {
    let mut resources = TrackResources::default();
    let owner = resources.new_owner().unwrap();
    let selector = TrackSelector {
        participant_external_id: "alice".into(),
        label: "camera".into(),
    };
    resources.claim_video(owner, selector.clone()).unwrap();
    let mut observation = VideoObservation {
        visible: false,
        height: 720,
        min_height: 1080,
        ..VideoObservation::default()
    };
    let mut tracks = BTreeMap::from([(
        "first".into(),
        RemoteTrack {
            id: "first".into(),
            participant_id: "alice".into(),
            spec: video("camera"),
        },
    )]);
    resources.observe_video(owner, observation.clone()).unwrap();
    assert!(
        resources
            .video_projection(&tracks, 1)
            .subscriptions
            .is_empty()
    );
    observation.visible = true;
    resources.observe_video(owner, observation).unwrap();
    assert_eq!(
        resources.video_projection(&tracks, 1).subscriptions.len(),
        1
    );
    tracks.clear();
    assert!(
        resources
            .video_projection(&tracks, 1)
            .subscriptions
            .is_empty()
    );
    tracks.insert(
        "replacement".into(),
        RemoteTrack {
            id: "replacement".into(),
            participant_id: "alice".into(),
            spec: video("camera"),
        },
    );
    assert_eq!(
        resources.video_projection(&tracks, 1).subscriptions[0].selector,
        Some(selector)
    );
}

#[test]
fn receive_owners_filter_participants_rebind_and_keep_independent_bytes() {
    let mut resources = TrackResources::default();
    let all = TrackOwner(1);
    let alice = TrackOwner(2);
    let spec = data(TopicMode::Ordered);
    resources.receive(all, &spec, None).unwrap();
    resources
        .receive(alice, &spec, Some("alice".into()))
        .unwrap();
    resources.retain_producer(&spec).unwrap();
    assert_eq!(resources.registrations().subscribers.len(), 1);
    resources.notify(&message(TopicMode::Ordered, "bob", "old", 1, &[255, 0]));
    assert_eq!(resources.read(alice).unwrap(), None);
    resources.read(all).unwrap();
    for track in ["old", "republished"] {
        let original = message(TopicMode::Ordered, "alice", track, 1, &[255, 0]);
        resources.notify(&original);
        let mut first = resources.read(all).unwrap().unwrap();
        if let DataEvent::Message { data, .. } = &mut first {
            data[0] = 1;
        }
        assert_eq!(
            resources.read(alice).unwrap(),
            Some(DataEvent::Message {
                participant_id: "alice".into(),
                track_id: track.into(),
                sequence: 1,
                data: vec![255, 0],
            })
        );
    }
    resources.release_receiver(alice);
    assert_eq!(resources.registrations().subscribers.len(), 1);
    resources.release_receiver(all);
    assert!(resources.registrations().subscribers.is_empty());
    assert_eq!(resources.registrations().publishers.len(), 1);
}

#[test]
fn latest_coalesces_per_track_and_lossy_drops_oldest() {
    for mode in [TopicMode::Latest, TopicMode::Lossy] {
        let mut resources = TrackResources::default();
        let owner = TrackOwner(1);
        resources.receive(owner, &data(mode), None).unwrap();
        for sequence in 0..300 {
            resources.notify(&message(mode, "alice", "track", sequence, &[]));
        }
        let mut sequences = Vec::new();
        while let Some(DataEvent::Message { sequence, .. }) = resources.read(owner).unwrap() {
            sequences.push(sequence);
        }
        if mode == TopicMode::Latest {
            assert_eq!(sequences, vec![299]);
        } else {
            assert_eq!(sequences, (44..300).collect::<Vec<_>>());
        }
    }
}

#[test]
fn ordered_overflow_reports_loss_inline_instead_of_implying_continuity() {
    let mut resources = TrackResources::default();
    let owner = TrackOwner(1);
    resources
        .receive(owner, &data(TopicMode::Ordered), None)
        .unwrap();
    for sequence in 0..300 {
        resources.notify(&message(
            TopicMode::Ordered,
            "alice",
            "track",
            sequence,
            &[],
        ));
    }
    assert_eq!(
        resources.read(owner).unwrap(),
        Some(DataEvent::Gap {
            participant_id: "alice".into(),
            track_id: "track".into(),
            from: 0,
            to: 45,
        })
    );
    for sequence in 45..300 {
        assert!(
            matches!(resources.read(owner).unwrap(), Some(DataEvent::Message { sequence: observed, .. }) if observed == sequence)
        );
    }
    assert_eq!(resources.read(owner).unwrap(), None);
}

#[test]
fn unrepresentable_overflow_and_channel_failure_end_only_affected_owners() {
    for mode in [TopicMode::Latest, TopicMode::Ordered] {
        let mut resources = TrackResources::default();
        let all = TrackOwner(1);
        let bob = TrackOwner(2);
        let lossy = TrackOwner(3);
        resources.receive(all, &data(mode), None).unwrap();
        resources
            .receive(bob, &data(mode), Some("bob".into()))
            .unwrap();
        resources
            .receive(lossy, &data(TopicMode::Lossy), None)
            .unwrap();
        for index in 0..=DATA_RECEIVE_CAPACITY {
            resources.notify(&message(mode, "alice", &format!("track-{index}"), 0, &[]));
        }
        assert_eq!(
            resources.read(all),
            Err(TrackResourceError::ReceiveOverflow)
        );
        assert_eq!(resources.read(bob).unwrap(), None);
        resources.notify(&TopicNotification::ChannelFailed {
            channel: TopicChannel::Subscriber(TopicSubscriber {
                topic: "events".into(),
                mode,
            }),
            message: "closed".into(),
        });
        assert_eq!(
            resources.read(bob),
            Err(TrackResourceError::ReceiveChannelFailed("closed".into()))
        );
        assert_eq!(resources.read(lossy).unwrap(), None);
        assert_eq!(
            resources.registrations().subscribers,
            vec![TopicSubscriber {
                topic: "events".into(),
                mode: TopicMode::Lossy
            }]
        );
    }
}

#[test]
fn failed_data_admission_does_not_partially_claim_capacity() {
    let mut resources = TrackResources::default();
    for index in 0..crate::MAX_TOPIC_CHANNELS {
        resources
            .retain_producer(&TrackSpec::Data {
                label: format!("topic-{index}"),
                delivery: TopicMode::Lossy,
            })
            .unwrap();
    }
    assert!(
        resources
            .receive(TrackOwner(1), &data(TopicMode::Lossy), None)
            .is_err()
    );
    assert!(resources.registrations().subscribers.is_empty());
    assert_eq!(
        resources.registrations().publishers.len(),
        crate::MAX_TOPIC_CHANNELS
    );
    assert_eq!(
        resources.read(TrackOwner(1)),
        Err(TrackResourceError::Released)
    );
}
