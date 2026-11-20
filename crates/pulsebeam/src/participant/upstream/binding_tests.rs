use super::*;
use crate::{entity, track};

fn publisher() -> (Upstream, entity::ParticipantId, entity::ConnectionId) {
    let participant_id = crate::entity::fixture_participant_id();
    let connection = entity::ConnectionId::new();
    let ctx = LogCtx {
        connection_id: connection,
    };
    (Upstream::new(ctx), participant_id, connection)
}

fn add_audio(
    upstream: &mut Upstream,
    participant: entity::ParticipantId,
    connection: entity::ConnectionId,
    index: u32,
    mid: &str,
) {
    let mid = Mid::from(mid);
    let (track, descriptor) = track::test_utils::make_audio_track(participant, connection, mid);
    assert!(upstream.add_published_track(index, mid, TrackKind::Audio, || (track, descriptor)));
}

#[test]
fn managed_slots_share_storage_without_reusing_keys_or_consuming_sdp_capacity() {
    let (mut upstream, participant, connection) = publisher();
    let managed_mid = Mid::from("managed");
    let make_managed = || track::test_utils::make_audio_track(participant, connection, managed_mid);
    let (old, managed) = upstream
        .insert_managed_track(managed_mid, TrackKind::Audio, make_managed)
        .unwrap();
    let managed_id = managed.id();

    for index in 0..MAX_UPSTREAM_SLOT_PER_TYPE {
        add_audio(
            &mut upstream,
            participant,
            connection,
            u32::try_from(index).unwrap(),
            &format!("sdp{index}"),
        );
    }
    let first = upstream.slot_for_mid(Mid::from("sdp0")).unwrap();
    let overflow_mid = Mid::from("overflow");
    assert!(
        !upstream.add_published_track(32, overflow_mid, TrackKind::Audio, || {
            panic!("a rejected SDP slot must not construct a publication");
        })
    );
    assert!(upstream.track_mut(old).is_some());

    upstream.remove_managed_track(old, managed_id);
    assert!(upstream.track_mut(old).is_none());
    let (new, _) = upstream
        .insert_managed_track(managed_mid, TrackKind::Audio, make_managed)
        .unwrap();
    assert_ne!(old, new);
    assert_eq!(upstream.slot_for_mid(Mid::from("sdp0")), Some(first));
    upstream.remove_managed_track(old, managed_id);
    assert!(upstream.track_mut(new).is_some());

    upstream.audio.next_slot = usize::MAX;
    assert!(
        upstream
            .insert_managed_track(managed_mid, TrackKind::Audio, || {
                panic!("a rejected slot must not construct a publication");
            })
            .is_none()
    );
}

#[test]
fn fanout_lookup_distinguishes_track_kinds_with_equal_identity_bytes() {
    let (mut upstream, _, _) = publisher();
    let bytes = [7; 16];
    let audio = PublicationKey::Audio(entity::PublicationId::from_bytes(bytes));
    let video = PublicationKey::Video(entity::PublicationId::from_bytes(bytes));
    let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
    let audio_handle = handles.insert(());
    let video_handle = handles.insert(());

    upstream.bind_track_handle(audio, audio_handle);
    upstream.bind_track_handle(video, video_handle);
    assert_eq!(upstream.track_fanout(audio), Some(audio_handle));
    assert_eq!(upstream.track_fanout(video), Some(video_handle));
    assert_eq!(upstream.track_for_fanout(audio_handle), Some(audio));
    assert_eq!(upstream.track_for_fanout(video_handle), Some(video));
    upstream.unbind_track_handle(audio, audio_handle);
    assert_eq!(upstream.track_fanout(video), Some(video_handle));
}

#[test]
fn fanout_rebinding_and_stale_unbind_preserve_current_track() {
    let (mut upstream, _, connection) = publisher();
    let first = connection.fixture_publication(TrackKind::Audio, "first");
    let second = connection.fixture_publication(TrackKind::Audio, "second");
    let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
    let old = handles.insert(());
    let current = handles.insert(());

    upstream.bind_track_handle(first, old);
    upstream.bind_track_handle(first, current);
    upstream.unbind_track_handle(first, old);
    assert_eq!(upstream.track_for_fanout(old), None);
    assert_eq!(upstream.track_for_fanout(current), Some(first));

    upstream.bind_track_handle(second, current);
    upstream.unbind_track_handle(first, current);
    assert_eq!(upstream.track_fanout(first), None);
    assert_eq!(upstream.track_for_fanout(current), Some(second));
    upstream.unbind_track_handle(second, current);
    assert_eq!(upstream.track_for_fanout(current), None);
}
