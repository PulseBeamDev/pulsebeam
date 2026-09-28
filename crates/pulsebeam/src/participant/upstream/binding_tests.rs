use super::*;
use crate::{entity, track};

fn publisher() -> (Upstream, entity::ParticipantId) {
    let room_id =
        entity::RoomId::from_external(&entity::RoomExternalId::new("binding-room").unwrap());
    let participant_id = entity::ParticipantId::new();
    let ctx = LogCtx {
        room_id,
        participant_id,
    };
    (Upstream::new(ctx), participant_id)
}

fn add_audio(upstream: &mut Upstream, participant: entity::ParticipantId, index: u32, mid: &str) {
    let mid = Mid::from(mid);
    let (track, descriptor) = track::test_utils::make_audio_track(participant, mid);
    assert!(upstream.add_published_track(index, mid, track, descriptor));
}

#[test]
fn sender_labels_replace_provisional_mid_identity_once() {
    let (mut upstream, participant) = publisher();
    add_audio(&mut upstream, participant, 0, "offer-mid");
    let provisional = upstream.track_for_sender_index(0).unwrap();
    let bound = upstream
        .bind_sender_label(0, TrackKind::Audio, "microphone")
        .unwrap();
    assert_ne!(bound, provisional);
    assert_eq!(
        bound,
        participant.derive_track_id(TrackKind::Audio, "microphone")
    );
    assert_eq!(upstream.track_for_sender_index(0), Some(bound));
    assert_eq!(
        upstream.bind_sender_label(0, TrackKind::Audio, "microphone"),
        Ok(bound)
    );
    assert_eq!(
        upstream.bind_sender_label(0, TrackKind::Audio, "renamed"),
        Err(SenderLabelError::AlreadyBound)
    );
    let (descriptor, in_topology) = upstream
        .announce_state_mut(Mid::from("offer-mid"), true)
        .unwrap();
    assert_eq!(descriptor.id(), bound);
    assert_eq!(descriptor.meta().label.as_deref(), Some("microphone"));
    *in_topology = true;
    let (_, in_topology) = upstream
        .announce_state_mut(Mid::from("offer-mid"), false)
        .unwrap();
    *in_topology = false;
    assert_eq!(
        upstream.bind_sender_label(0, TrackKind::Audio, "renamed"),
        Err(SenderLabelError::AlreadyBound)
    );
}

#[test]
fn sender_label_batch_does_not_commit_audio_when_video_rejects() {
    let (mut upstream, participant) = publisher();
    add_audio(&mut upstream, participant, 0, "a");
    let mid = Mid::from("v");
    let (track, descriptor) = track::test_utils::make_video_track(participant, mid, Vec::new());
    assert!(upstream.add_published_track(1, mid, track, descriptor));
    let audio_before = upstream.track_for_sender_index(0);
    upstream.announce_state_mut(mid, true).unwrap();
    assert_eq!(
        upstream.bind_sender_labels_atomically(&[
            (0, TrackKind::Audio, "mic"),
            (1, TrackKind::Video, "camera"),
        ]),
        Err(SenderLabelError::AlreadyPublished)
    );
    assert_eq!(upstream.track_for_sender_index(0), audio_before);
}

#[test]
fn sender_label_batch_rejects_all_on_late_failure_and_permits_identity_swap() {
    let (mut upstream, participant) = publisher();
    add_audio(&mut upstream, participant, 0, "a");
    add_audio(&mut upstream, participant, 1, "b");
    let provisional_a = upstream.track_for_sender_index(0);
    let provisional_b = upstream.track_for_sender_index(1);
    assert_eq!(
        upstream.bind_sender_labels_atomically(&[
            (0, TrackKind::Audio, "same"),
            (1, TrackKind::Audio, "same"),
        ]),
        Err(SenderLabelError::DuplicateLabel)
    );
    assert_eq!(upstream.track_for_sender_index(0), provisional_a);
    assert_eq!(upstream.track_for_sender_index(1), provisional_b);
    upstream
        .bind_sender_labels_atomically(&[(0, TrackKind::Audio, "b"), (1, TrackKind::Audio, "a")])
        .unwrap();
    assert_eq!(upstream.track_for_sender_index(0), provisional_b);
    assert_eq!(upstream.track_for_sender_index(1), provisional_a);
}

#[test]
fn duplicate_label_and_published_provisional_identity_are_rejected() {
    let (mut upstream, participant) = publisher();
    add_audio(&mut upstream, participant, 0, "a");
    add_audio(&mut upstream, participant, 1, "b");
    assert!(
        upstream
            .bind_sender_label(0, TrackKind::Audio, "same")
            .is_ok()
    );
    assert_eq!(
        upstream.bind_sender_label(1, TrackKind::Audio, "same"),
        Err(SenderLabelError::DuplicateLabel)
    );
    upstream.announce_state_mut(Mid::from("b"), true).unwrap();
    assert_eq!(
        upstream.bind_sender_label(1, TrackKind::Audio, "different"),
        Err(SenderLabelError::AlreadyPublished)
    );
    assert_eq!(
        upstream.bind_sender_label(9, TrackKind::Audio, "missing"),
        Err(SenderLabelError::UnknownSender)
    );
}
