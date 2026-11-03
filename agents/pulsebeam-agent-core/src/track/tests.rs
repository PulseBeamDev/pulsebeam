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
    for label in ["", &"x".repeat(257), &"é".repeat(129)] {
        assert!(video(label).validate().is_err(), "{label:?}");
    }
    for label in ["é", "with/slash", "control\n", &"é".repeat(128)] {
        assert!(video(label).validate().is_ok(), "{label:?}");
        assert_eq!(video(label).label(), label);
    }
    assert!(
        TrackSpec::Data {
            label: "x".repeat(256),
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
        .is_ok()
    );
}

#[test]
fn publication_claim_is_atomic_exclusive_and_reuses_released_coordinates() {
    for kind in [MediaKind::Video, MediaKind::Audio] {
        let mut resources = TrackResources::default();
        let spec = |label: &str| match kind {
            MediaKind::Video => TrackSpec::Video {
                label: label.into(),
            },
            MediaKind::Audio => TrackSpec::Audio {
                label: label.into(),
            },
        };
        let prefix = if kind == MediaKind::Video { 'v' } else { 'a' };
        let first = resources.new_owner().unwrap();
        let second = resources.new_owner().unwrap();
        let camera = spec("camera");
        assert_eq!(
            resources
                .claim_publication(first, camera.clone(), &BTreeSet::new())
                .unwrap(),
            format!("{prefix}0")
        );
        resources.update_publication(first, &camera, true).unwrap();
        assert_eq!(
            resources.claim_publication(second, camera.clone(), &BTreeSet::new()),
            Err(TrackResourceError::AlreadyOwned)
        );
        for index in 1..=256 {
            let owner = resources.new_owner().unwrap();
            let input = spec(&format!("input-{index}"));
            assert_eq!(
                resources
                    .claim_publication(owner, input.clone(), &BTreeSet::new())
                    .unwrap(),
                format!("{prefix}{index}")
            );
            resources.update_publication(owner, &input, true).unwrap();
        }
        assert_eq!(resources.publications().len(), 257);
        assert!(
            resources
                .publications()
                .iter()
                .all(|value| value.kind == kind)
        );
        assert_eq!(resources.release_publication(second, &camera), None);
        assert_eq!(
            resources.release_publication(first, &camera),
            Some(format!("{prefix}0"))
        );
        assert_eq!(
            resources.update_publication(first, &camera, true),
            Err(TrackResourceError::Released)
        );
        assert_eq!(
            resources
                .claim_publication(second, spec("replacement"), &BTreeSet::new())
                .unwrap(),
            format!("{prefix}0")
        );
        assert_eq!(resources.publications().len(), 256);
    }
}

#[test]
fn new_lifetime_fences_stale_source_updates_and_cleanup_after_reacquisition() {
    let mut resources = TrackResources::default();
    let spec = video("camera");
    let previous = resources.new_owner().unwrap();
    resources
        .claim_publication(previous, spec.clone(), &BTreeSet::new())
        .unwrap();
    resources.release_publication(previous, &spec).unwrap();
    let current = resources.new_owner().unwrap();
    resources
        .claim_publication(current, spec.clone(), &BTreeSet::new())
        .unwrap();
    assert_eq!(
        resources.update_publication(previous, &spec, true),
        Err(TrackResourceError::Released)
    );
    assert_eq!(resources.release_publication(previous, &spec), None);
    assert!(resources.publications().is_empty());
    resources.update_publication(current, &spec, true).unwrap();
    let (obsolete, paused) = resources
        .begin_publication_input(current, PublicationInputIntent::Replace, true, false)
        .unwrap();
    let input = obsolete.input().unwrap();
    assert_eq!(paused.input, Some(input));
    assert!(!paused.enabled);
    assert!(!resources.publications()[0].active);
    let (latest, _) = resources
        .begin_publication_input(
            current,
            PublicationInputIntent::ConfigureCurrent,
            true,
            true,
        )
        .unwrap();
    assert_eq!(latest.input(), Some(input));
    assert_ne!(obsolete, latest);
    assert!(!resources.publication_input_current(&obsolete));
    assert!(
        resources
            .complete_publication_input(&obsolete, true)
            .is_none()
    );
    assert!(resources.publication_input_current(&latest));
    assert!(
        !resources
            .complete_publication_input(&latest, true)
            .unwrap()
            .enabled
    );
    assert!(resources.publication_input_current(&latest));
    assert!(
        resources
            .complete_publication_input(&latest, true)
            .is_none()
    );
    assert!(
        resources.publications()[0].active,
        "mute is not source withdrawal"
    );
    assert!(
        resources
            .set_publication_muted(current, false)
            .unwrap()
            .enabled
    );
    let mut ids = crate::id::IdGenerator::new();
    let mut control = resources.publication_inputs().next().unwrap();
    control.effect = Some(ids.operation());
    resources.update_input_control(&control);
    assert!(resources.complete_input_control(&control));
    assert!(
        !resources.complete_input_control(&control),
        "duplicate target completion"
    );
    resources.set_publication_muted(current, true).unwrap();
    let mut paused = resources.publication_inputs().next().unwrap();
    assert!(resources.input_control_changed(&paused));
    paused.effect = Some(ids.operation());
    resources.update_input_control(&paused);
    assert!(!resources.complete_input_control(&control));
    resources.set_publication_muted(current, false).unwrap();
    let mut resumed = resources.publication_inputs().next().unwrap();
    assert_eq!(resumed.operation, control.operation);
    assert_eq!(resumed.enabled, control.enabled);
    assert!(resources.input_control_changed(&resumed));
    resumed.effect = Some(ids.operation());
    resources.update_input_control(&resumed);
    assert_ne!(
        resumed, control,
        "returning to the same target is fresh work"
    );
    assert!(!resources.complete_input_control(&control));
    assert!(resources.complete_input_control(&resumed));
    let (failed, _) = resources
        .begin_publication_input(current, PublicationInputIntent::Retain(input), true, false)
        .unwrap();
    assert_eq!(failed.input(), Some(input));
    assert!(
        !resources
            .complete_publication_input(&failed, false)
            .unwrap()
            .enabled
    );
    assert!(!resources.publications()[0].active);
    let (detached, state) = resources
        .begin_publication_input(current, PublicationInputIntent::Retain(input), false, false)
        .unwrap();
    assert_eq!(state.input, None);
    assert_eq!(detached.input(), None);
    assert!(!resources.publication_input_current(&failed));
    assert!(
        !resources
            .complete_publication_input(&detached, true)
            .unwrap()
            .enabled
    );
    assert_eq!(
        resources.begin_publication_input(
            current,
            PublicationInputIntent::Retain(input),
            true,
            false
        ),
        Err(TrackResourceError::Released)
    );
    let (canceled, _) = resources
        .begin_publication_input(current, PublicationInputIntent::Replace, true, false)
        .unwrap();
    assert_ne!(canceled.input(), Some(input));
    assert_eq!(
        resources.begin_publication_input(
            current,
            PublicationInputIntent::Retain(input),
            true,
            false
        ),
        Err(TrackResourceError::Released)
    );
    assert!(resources.publication_input_current(&canceled));
    resources.update_publication(current, &spec, false).unwrap();
    assert!(
        resources
            .complete_publication_input(&canceled, true)
            .is_none()
    );
    resources.release_publication(current, &spec).unwrap();
    let replacement = resources.new_owner().unwrap();
    resources
        .claim_publication(replacement, spec, &BTreeSet::new())
        .unwrap();
    assert!(!resources.publication_input_current(&latest));
    assert!(
        resources
            .complete_publication_input(&latest, true)
            .is_none()
    );
    assert!(resources.publications().is_empty());
}

#[test]
fn source_withdrawal_keeps_the_declaration_and_audio_cleanup_is_owner_scoped() {
    let mut resources = TrackResources::default();
    let first = TrackOwner(1);
    let second = TrackOwner(2);
    resources
        .claim_publication(first, video("camera"), &BTreeSet::new())
        .unwrap();
    assert!(resources.publications().is_empty());
    resources
        .update_publication(first, &video("camera"), false)
        .unwrap();
    assert!(resources.publications().is_empty());
    resources
        .update_publication(first, &video("camera"), true)
        .unwrap();
    let mut expected = PublicationIntent {
        slot: "v0".into(),
        kind: MediaKind::Video,
        label: "camera".into(),
        active: true,
    };
    assert_eq!(resources.publications(), vec![expected.clone()]);
    resources
        .update_publication(first, &video("camera"), false)
        .unwrap();
    expected.active = false;
    assert_eq!(resources.publications(), vec![expected]);
    assert_eq!(
        resources
            .claim_publication(second, video("screen"), &BTreeSet::new())
            .unwrap(),
        "v1"
    );
    resources.claim_audio(first).unwrap();
    assert_eq!(
        resources.claim_audio(second),
        Err(TrackResourceError::AlreadyOwned)
    );
    assert!(!resources.release_audio(second));
    assert!(resources.release_audio(first));
    resources.claim_audio(second).unwrap();
    assert!(!resources.release_audio(first));
    let entry = resources.publications.get_mut(&video("camera")).unwrap();
    entry.input_revision = u64::MAX;
    let before = entry.input_state();
    assert_eq!(
        resources.begin_publication_input(first, PublicationInputIntent::Replace, true, true),
        Err(TrackResourceError::InputOperationSpaceExhausted)
    );
    assert_eq!(
        resources.publications[&video("camera")].input_state(),
        before
    );
    resources
        .release_publication(first, &video("camera"))
        .unwrap();
    assert!(resources.publications().is_empty());
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
    let mut tracks = BTreeMap::from([
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
    let projection = resources.video_projection(&tracks).unwrap();
    assert_eq!(
        projection,
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
    let mut unbounded = observation.clone();
    unbounded.height = 0;
    resources.observe_video(second, unbounded).unwrap();
    assert_eq!(resources.video_projection(&tracks).unwrap()[0].height, 0);
    let unbounded_request = resources.begin_media_observation(second).unwrap();
    assert!(
        resources
            .observation_requests()
            .contains(&(unbounded_request, Some(crate::MediaSlot::RemoteVideo(0))))
    );
    resources.release_observation_controls(second);
    resources
        .observe_video(second, observation.clone())
        .unwrap();
    assert_eq!(resources.video_projection(&tracks).unwrap()[0].height, 720);
    resources.claim_video(competing, other.clone()).unwrap();
    resources
        .observe_video(competing, observation.clone())
        .unwrap();
    let projection = resources.video_projection(&tracks).unwrap();
    assert_eq!(projection[0].selector, Some(selector));
    assert_eq!(projection.len(), 2);
    assert_eq!(projection[1].selector, Some(other.clone()));
    let mut added = Vec::new();
    for index in 0..256 {
        let participant = format!("extra-{index}");
        let owner = TrackOwner(1000 + index);
        tracks.insert(
            participant.clone(),
            RemoteTrack {
                id: participant.clone(),
                participant_id: participant.clone(),
                spec: video("camera"),
            },
        );
        resources
            .claim_video(
                owner,
                TrackSelector {
                    participant_external_id: participant,
                    label: "camera".into(),
                },
            )
            .unwrap();
        resources.observe_video(owner, observation.clone()).unwrap();
        added.push(owner);
    }
    let expanded = resources.video_projection(&tracks).unwrap();
    assert_eq!(expanded.len(), 258);
    assert_eq!(expanded.iter().map(|value| value.slot).max(), Some(257));
    for owner in added {
        assert!(resources.release_video(owner));
    }
    observation.visible = false;
    resources.observe_video(first, observation.clone()).unwrap();
    resources
        .observe_video(second, observation.clone())
        .unwrap();
    let projection = resources.video_projection(&tracks).unwrap();
    assert_eq!(projection[0].selector, Some(other));

    resources.release_video(competing);
    assert_eq!(
        resources.observe_video(competing, observation),
        Err(TrackResourceError::Released)
    );
    assert!(resources.video_projection(&tracks).unwrap().is_empty());
}

#[test]
fn hidden_video_demand_does_not_allocate_and_withdrawal_retains_logical_interest() {
    let mut resources = TrackResources::default();
    let owner = resources.new_owner().unwrap();
    let selector = TrackSelector {
        participant_external_id: "alice".into(),
        label: "camera".into(),
    };
    resources.claim_video(owner, selector.clone()).unwrap();
    let request = resources.begin_media_observation(owner).unwrap();
    let waiting = resources.begin_media_observation(owner).unwrap();
    let mut ids = crate::id::IdGenerator::new();
    let generation = ids.generation();
    let target = |source: &str, reference, mid: &str| MediaObservationTarget {
        generation,
        reference,
        source: source.into(),
        binding: crate::SlotBinding {
            slot: crate::MediaSlot::RemoteVideo(0),
            mid: mid.into(),
            media_index: 1,
            kind: MediaKind::Video,
            direction: crate::MediaDirection::ReceiveOnly,
        },
    };
    let mut issue = |resources: &mut TrackResources, request, target| {
        let mut control = resources.observation_control(request, target).unwrap();
        assert!(resources.observation_control_changed(&control));
        control.effect = Some(ids.operation());
        resources.update_observation_control(&control);
        control
    };
    let pending = issue(&mut resources, request, None);
    assert!(!pending.closed);
    assert!(!resources.complete_observation_control(&pending, true));
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
    assert!(resources.video_projection(&tracks).unwrap().is_empty());
    observation.visible = true;
    resources.observe_video(owner, observation).unwrap();
    assert_eq!(resources.video_projection(&tracks).unwrap().len(), 1);
    let first_target = target("first", 11, "receiver-a");
    let first = issue(&mut resources, request, Some(first_target.clone()));
    let unfinished = issue(&mut resources, waiting, Some(first_target));
    assert!(resources.complete_observation_control(&first, true));
    assert!(!resources.complete_observation_control(&first, false));
    tracks.clear();
    let ended = issue(&mut resources, request, None);
    assert!(ended.closed);
    assert!(!resources.observation_control_current(&first));
    assert!(!resources.end_observation_control(&first));
    let waiting_absent = issue(&mut resources, waiting, None);
    assert!(!waiting_absent.closed);
    assert!(!resources.complete_observation_control(&unfinished, true));
    let retained = resources.video_projection(&tracks).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].selector.as_ref(), Some(&selector));
    assert_eq!(retained[0].slot, 0);
    tracks.insert(
        "replacement".into(),
        RemoteTrack {
            id: "replacement".into(),
            participant_id: "alice".into(),
            spec: video("camera"),
        },
    );
    assert_eq!(
        resources.video_projection(&tracks).unwrap()[0].selector,
        Some(selector)
    );
    let replacement_target = target("replacement", 19, "receiver-b");
    let still_ended = resources
        .observation_control(request, Some(replacement_target.clone()))
        .unwrap();
    assert!(still_ended.closed);
    assert!(!resources.observation_control_changed(&still_ended));
    let replacement = issue(&mut resources, waiting, Some(replacement_target.clone()));
    // A pending acquisition can wait through an ABA, but its old work cannot complete it.
    issue(&mut resources, waiting, None);
    let replacement_current = issue(&mut resources, waiting, Some(replacement_target.clone()));
    assert!(!resources.complete_observation_control(&replacement, true));
    assert!(!resources.complete_observation_control(&unfinished, false));
    assert!(resources.complete_observation_control(&replacement_current, true));
    let fresh_request = resources.begin_media_observation(owner).unwrap();
    assert_ne!(fresh_request, request);
    let fresh = issue(&mut resources, fresh_request, Some(replacement_target));
    assert!(resources.complete_observation_control(&fresh, true));
    assert!(resources.end_observation_control(&replacement_current));
    let ended = issue(&mut resources, waiting, None);
    assert!(ended.closed);
    assert!(resources.observation_control_current(&fresh));
    let mut rebound = fresh.target.clone().unwrap();
    rebound.reference += 1;
    let retired_receiver = issue(&mut resources, fresh_request, Some(rebound));
    assert!(retired_receiver.closed);
    assert!(!resources.end_observation_control(&fresh));
    let released = resources.release_observation_controls(owner);
    assert_eq!(released.len(), 3);
    assert!(
        released
            .iter()
            .all(|control| control.closed && control.target.is_none())
    );
    resources.release(owner);
    assert!(!resources.complete_observation_control(&fresh, true));
    assert_eq!(
        resources.begin_media_observation(owner),
        Err(TrackResourceError::Released)
    );
    assert!(!crate::AudioSubscription::default().automatic);
    let playback = resources.new_owner().unwrap();
    resources.claim_audio(playback).unwrap();
    let audio = resources.new_owner().unwrap();
    let selector = TrackSelector {
        participant_external_id: "alice".into(),
        label: "microphone".into(),
    };
    resources
        .claim_audio_subscription(audio, selector.clone())
        .unwrap();
    let shared = resources.new_owner().unwrap();
    resources
        .claim_audio_subscription(shared, selector.clone())
        .unwrap();
    let excess = resources.new_owner().unwrap();
    resources
        .claim_audio_subscription(
            excess,
            TrackSelector {
                participant_external_id: "bob".into(),
                label: "microphone".into(),
            },
        )
        .unwrap();
    let mut added = Vec::new();
    for index in 0..256 {
        let owner = resources.new_owner().unwrap();
        resources
            .claim_audio_subscription(
                owner,
                TrackSelector {
                    participant_external_id: format!("extra-{index}"),
                    label: "microphone".into(),
                },
            )
            .unwrap();
        added.push(owner);
    }
    assert_eq!(resources.audio_projection().selected.len(), 258);
    for owner in added {
        resources.release(owner);
    }
    resources.release(excess);
    assert_eq!(
        resources.audio_projection().selected,
        vec![selector.clone()]
    );
    assert_eq!(
        resources.observe_video(audio, VideoObservation::default()),
        Err(TrackResourceError::WrongKind)
    );
    assert_eq!(
        resources.configure_audio(
            audio,
            PlayoutDelay::Fixed {
                min_ms: 2,
                max_ms: 1
            }
        ),
        Err(TrackResourceError::InvalidPlayoutDelay)
    );
    let fixed = PlayoutDelay::Fixed {
        min_ms: 0,
        max_ms: 0,
    };
    assert_eq!(
        resources.configure_audio(audio, fixed),
        Err(TrackResourceError::AlreadyOwned)
    );
    resources.release(shared);
    resources.configure_audio(audio, fixed).unwrap();
    assert_eq!(
        resources.audio_projection().selector_delays[&selector],
        fixed
    );
    let request = resources.begin_media_observation(audio).unwrap();
    let pending = issue(&mut resources, request, None);
    assert!(!pending.closed);
    assert_eq!(
        resources.audio_observation_selector(request),
        Some(&selector)
    );
    assert!(resources.has_audio_subscriptions());
    assert!(
        resources
            .release_observation_controls(audio)
            .iter()
            .all(|control| control.closed)
    );
    resources.release(audio);
    assert!(!resources.has_audio_subscriptions());
    assert_eq!(
        resources.begin_media_observation(audio),
        Err(TrackResourceError::Released)
    );
    assert!(resources.release_audio(playback));

    let policy_owner = resources.new_owner().unwrap();
    let all = crate::AudioSubscription {
        automatic: true,
        ..Default::default()
    };
    resources.claim_audio_policy(policy_owner, all).unwrap();
    assert_eq!(
        resources.begin_media_observation(policy_owner),
        Err(TrackResourceError::WrongKind)
    );
    let named = resources.new_owner().unwrap();
    resources
        .claim_audio_subscription(named, selector.clone())
        .unwrap();
    let projected = resources.audio_projection();
    assert!(projected.automatic);
    assert_eq!(projected.selected, vec![selector.clone()]);
    let exact = resources
        .begin_audio_source_observation(policy_owner, "audio-one".into())
        .unwrap();
    let pending_exact = resources
        .begin_audio_source_observation(policy_owner, "audio-one".into())
        .unwrap();
    assert_eq!(resources.audio_observation_source(exact), Some("audio-one"));
    assert_eq!(
        resources.audio_projection(),
        projected,
        "observing must not add demand"
    );
    assert!(resources.audio_source_authorized(policy_owner, "audio-one", None));
    assert!(!resources.audio_source_authorized(named, "audio-one", None));
    assert_eq!(
        resources.configure_audio(named, fixed),
        Err(TrackResourceError::AlreadyOwned)
    );
    let mut audio_target = target("audio-one", 55, "audio-receiver");
    audio_target.binding.slot = crate::MediaSlot::RemoteAudio(0);
    audio_target.binding.kind = MediaKind::Audio;
    let attached = issue(&mut resources, exact, Some(audio_target));
    assert!(resources.complete_observation_control(&attached, true));
    let not_attached = issue(&mut resources, pending_exact, None);
    resources.end_audio_source_observations("audio-one");
    assert!(issue(&mut resources, exact, None).closed);
    assert!(issue(&mut resources, pending_exact, None).closed);
    assert!(!resources.complete_observation_control(&attached, true));
    assert!(!resources.complete_observation_control(&not_attached, true));
    assert_eq!(
        resources.audio_projection(),
        projected,
        "source retirement must not change parent intent"
    );

    let invalid = crate::AudioSubscription {
        pinned: vec!["duplicate".into(), "duplicate".into()],
        ..Default::default()
    };
    assert!(
        resources
            .configure_audio_policy(policy_owner, invalid)
            .is_err()
    );
    assert_eq!(resources.audio_projection(), projected);
    let pins = crate::AudioSubscription {
        pinned: vec!["audio-two".into()],
        ..Default::default()
    };
    resources
        .configure_audio_policy(policy_owner, pins)
        .unwrap();
    assert!(resources.audio_source_authorized(policy_owner, "audio-two", None));
    assert!(!resources.audio_source_authorized(policy_owner, "audio-one", Some(&selector)));
    assert_eq!(
        resources.configure_audio(policy_owner, fixed),
        Err(TrackResourceError::WrongKind)
    );
    resources.release(policy_owner);
    let remaining = resources.audio_projection();
    assert!(!remaining.automatic);
    assert_eq!(remaining.selected, vec![selector]);
    resources.release(named);
    assert_eq!(
        resources.audio_projection(),
        crate::AudioSubscription::default()
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
fn data_admission_is_transactional_without_a_legacy_channel_count_limit() {
    let mut resources = TrackResources::default();
    for index in 0..65 {
        resources
            .retain_producer(&TrackSpec::Data {
                label: format!("topic-{index}"),
                delivery: TopicMode::Lossy,
            })
            .unwrap();
    }
    assert!(
        resources
            .receive(
                TrackOwner(1),
                &TrackSpec::Data {
                    label: "x".repeat(257),
                    delivery: TopicMode::Lossy
                },
                None
            )
            .is_err()
    );
    assert!(resources.registrations().subscribers.is_empty());
    assert_eq!(resources.registrations().publishers.len(), 65);
    assert_eq!(
        resources.read(TrackOwner(1)),
        Err(TrackResourceError::Released)
    );
    assert!(
        resources
            .retain_producer(&TrackSpec::Data {
                label: "topic-0".into(),
                delivery: TopicMode::Ordered
            })
            .is_err()
    );
    assert_eq!(resources.registrations().publishers.len(), 65);
    resources
        .receive(TrackOwner(1), &data(TopicMode::Lossy), None)
        .unwrap();
    assert_eq!(resources.registrations().subscribers.len(), 1);
}
