use pulsebeam_webrtc_sys::*;
use std::{
    thread,
    time::{Duration, Instant},
};

fn finish(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    id: OperationId,
) -> Option<SessionDescription> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(SessionEvent::OperationComplete(done)) = session.try_peer_event(peer).unwrap() {
            if done.operation_id == id {
                return done.result.unwrap();
            }
        }
        assert!(Instant::now() < deadline, "actor SDP timeout");
        thread::yield_now();
    }
}
#[test]
fn migrated_actor_selects_actual_foreign_same_id_track_and_retires_pending_work() {
    let mut session = ProductionSession::builder(Default::default())
        .encoded_audio()
        .build()
        .unwrap();
    let target = session.create_peer(Default::default()).unwrap();
    let origin = session.create_peer(Default::default()).unwrap();
    let input = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            target,
            input,
            "same-track-id",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let track = session.sender_track_reference(sender).unwrap().unwrap();
    assert_eq!(session.sender_track_reference(sender).unwrap(), Some(track));
    assert_eq!(session.media_track_id(track).unwrap(), b"same-track-id");
    assert_eq!(
        session.media_track_kind(track).unwrap(),
        MediaTrackKind::Audio
    );
    let receiver = session.receiver(transceiver).unwrap();
    let received = session.receiver_track_reference(receiver).unwrap().unwrap();
    assert_ne!(track, received);
    assert_eq!(
        session.receiver_track_reference(receiver).unwrap(),
        Some(received)
    );
    assert_eq!(
        session.media_track_kind(received).unwrap(),
        MediaTrackKind::Audio
    );
    let id = session.create_offer(target, false).unwrap();
    let offer = finish(&mut session, target, id).unwrap();
    let id = session.set_local_description(target, offer).unwrap();
    finish(&mut session, target, id);
    let source = session.create_opus_source(1).unwrap();
    let foreign = session
        .publish_opus(
            origin,
            source,
            "same-track-id",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let foreign_sender = session.sender(foreign).unwrap();
    let foreign_track = session
        .sender_track_reference(foreign_sender)
        .unwrap()
        .unwrap();
    assert_ne!(
        track, foreign_track,
        "textual ID must not canonicalize different SDK tracks"
    );
    assert_eq!(
        session.media_track_id(foreign_track).unwrap(),
        session.media_track_id(track).unwrap()
    );
    let id = session
        .request_legacy_stats_with_track(target, Some(foreign_track), LegacyStatsOutputLevel::Debug)
        .unwrap();
    let (session, snapshot) = thread::spawn(move || {
        let mut session = session;
        assert_eq!(
            session.sender_track_reference(foreign_sender).unwrap(),
            Some(foreign_track)
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            match session.try_peer_event(target).unwrap() {
                Some(SessionEvent::LegacyStats(snapshot)) if snapshot.operation_id == id => {
                    break snapshot;
                }
                Some(SessionEvent::OperationComplete(done)) if done.operation_id == id => {
                    panic!("accepted selector failed: {done:?}")
                }
                _ => {}
            }
            assert!(Instant::now() < deadline, "actor legacy timeout");
            thread::yield_now();
        };
        assert!(
            snapshot
                .reports
                .iter()
                .any(|r| r.id.kind() == LegacyStatsType::Track)
        );
        (session, snapshot)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        let id = session
            .request_legacy_stats_with_track(
                target,
                Some(foreign_track),
                LegacyStatsOutputLevel::Standard,
            )
            .unwrap();
        session
            .release_media_track_reference(foreign_track)
            .unwrap();
        assert!(session.media_track_id(foreign_track).is_err());
        session.close_peer(target).unwrap();
        let mut terminals = 0;
        while let Some(event) = session.try_peer_event(target).unwrap() {
            match event {
                SessionEvent::LegacyStats(value) if value.operation_id == id => terminals += 1,
                SessionEvent::OperationComplete(done) if done.operation_id == id => {
                    terminals += 1;
                    assert!(done.result.is_err());
                }
                _ => {}
            }
        }
        assert_eq!(terminals, 1);
        session.shutdown().unwrap();
        drop(session);
        assert!(!snapshot.reports.is_empty());
    })
    .join()
    .unwrap();
}
