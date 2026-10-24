use pulsebeam_webrtc_sys::*;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Events {
    done: HashMap<
        (SessionPeerId, OperationId),
        Result<Option<SessionDescriptionBytes>, PeerErrorKind>,
    >,
    added: Vec<(SessionPeerId, SessionMediaStreamId)>,
    removed: Vec<(SessionPeerId, SessionMediaStreamId)>,
    tracks: Vec<(SessionPeerId, SessionReceiverId, Vec<SessionMediaStreamId>)>,
    retired: Vec<(SessionPeerId, SessionReceiverId)>,
}
impl Events {
    fn drain(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        for (index, peer) in peers.into_iter().enumerate() {
            while let Some(event) = session.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::IceCandidate(candidate) => {
                        session
                            .add_ice_candidate(peers[1 - index], candidate)
                            .unwrap();
                    }
                    SessionEvent::IceCandidateBytes(candidate) => {
                        session
                            .add_ice_candidate_bytes(peers[1 - index], candidate)
                            .unwrap();
                    }
                    SessionEvent::OperationComplete(done) => {
                        assert!(
                            self.done
                                .insert(
                                    (peer, done.operation_id),
                                    done.result.map(|v| v.map(Into::into)).map_err(|e| e.kind)
                                )
                                .is_none(),
                            "duplicate terminal result"
                        );
                    }
                    SessionEvent::OperationCompleteBytes {
                        operation_id,
                        description,
                    } => {
                        assert!(
                            self.done
                                .insert((peer, operation_id), Ok(Some(description)))
                                .is_none()
                        );
                    }
                    SessionEvent::OperationFailedBytes {
                        operation_id,
                        error,
                    } => {
                        assert!(
                            self.done
                                .insert((peer, operation_id), Err(error.kind))
                                .is_none()
                        );
                    }
                    SessionEvent::StreamAdded { stream } => self.added.push((peer, stream)),
                    SessionEvent::StreamRemoved { stream } => self.removed.push((peer, stream)),
                    SessionEvent::TrackAdded { receiver, streams } => {
                        self.tracks.push((peer, receiver, streams))
                    }
                    SessionEvent::TrackRemoved { receiver } => self.retired.push((peer, receiver)),
                    _ => {}
                }
            }
        }
    }
    fn finish(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        id: OperationId,
    ) -> Result<Option<SessionDescriptionBytes>, PeerErrorKind> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain(session, peers);
            if let Some(value) = self.done.remove(&(peer, id)) {
                return value;
            }
            assert!(Instant::now() < deadline, "native operation timeout");
            std::thread::yield_now();
        }
    }
    fn negotiate(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        offer: SessionDescriptionBytes,
    ) {
        let id = session
            .set_local_description_legacy(peers[0], offer.clone())
            .unwrap();
        assert!(self.finish(session, peers, peers[0], id).unwrap().is_none());
        let id = session
            .set_remote_description_legacy(peers[1], offer)
            .unwrap();
        assert!(self.finish(session, peers, peers[1], id).unwrap().is_none());
        let id = session.create_answer(peers[1]).unwrap();
        let answer = self.finish(session, peers, peers[1], id).unwrap().unwrap();
        let id = session
            .set_local_description_legacy(peers[1], answer.clone())
            .unwrap();
        self.finish(session, peers, peers[1], id).unwrap();
        let id = session
            .set_remote_description_legacy(peers[0], answer)
            .unwrap();
        self.finish(session, peers, peers[0], id).unwrap();
    }
}

#[test]
fn migrated_plan_b_uses_real_stream_callbacks_media_and_legacy_description_outcomes() {
    let mut session = ProductionSession::builder(Default::default())
        .encoded_audio()
        .build()
        .unwrap();
    let config = RtcConfiguration {
        options: PeerConnectionOptions {
            sdp_semantics: SdpSemantics::PlanB,
            ..Default::default()
        },
        ..Default::default()
    };
    let peers = [
        session
            .create_peer_with_configuration(config.clone())
            .unwrap(),
        session.create_peer_with_configuration(config).unwrap(),
    ];
    let stream = session
        .create_local_media_stream(peers[0], b"real-plan-b-stream")
        .unwrap();
    let source = session.create_opus_source(1).unwrap();
    let track = session
        .create_opus_track(peers[0], source, "real-plan-b-track")
        .unwrap();
    assert!(session.add_media_stream_track(stream, track).unwrap());
    assert!(!session.add_media_stream_track(stream, track).unwrap());
    assert_eq!(session.media_stream_audio_tracks(stream).unwrap(), [track]);
    assert!(
        session
            .media_stream_video_tracks(stream)
            .unwrap()
            .is_empty()
    );
    assert!(session.add_stream(peers[0], stream).unwrap());
    assert!(!session.add_stream(peers[0], stream).unwrap());
    assert_eq!(session.local_streams(peers[0]).unwrap(), [stream]);
    let senders = session.senders(peers[0]).unwrap();
    assert_eq!(senders.len(), 1);
    assert_eq!(session.senders(peers[0]).unwrap(), senders);
    assert_eq!(
        session.sender_track_reference(senders[0]).unwrap(),
        Some(track)
    );
    assert_eq!(
        session.transceivers(peers[0]).unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    // A peer-held stream and source lease survive release of application IDs.
    session.release_media_track_reference(track).unwrap();
    session.release_media_stream(stream).unwrap();
    let stream = session.local_streams(peers[0]).unwrap()[0];
    let track = session.media_stream_audio_tracks(stream).unwrap()[0];
    assert_eq!(session.media_track_id(track).unwrap(), b"real-plan-b-track");
    assert_eq!(
        session
            .media_stream_find_audio_track(stream, b"real-plan-b-track")
            .unwrap(),
        Some(track)
    );
    let mut events = Events::default();
    let id = session.create_offer(peers[0], false).unwrap();
    let offer = events
        .finish(&mut session, peers, peers[0], id)
        .unwrap()
        .unwrap();
    assert!(
        offer
            .sdp
            .windows(b"real-plan-b-stream real-plan-b-track".len())
            .any(|v| v == b"real-plan-b-stream real-plan-b-track")
    );
    // This parses, then fails in the native deprecated observer's OnFailure.
    let wrong_state = SessionDescriptionBytes {
        kind: SessionDescriptionType::Answer,
        sdp: offer.sdp.clone(),
    };
    let id = session
        .set_remote_description_legacy(peers[1], wrong_state)
        .unwrap();
    assert_eq!(
        events
            .finish(&mut session, peers, peers[1], id)
            .unwrap_err(),
        PeerErrorKind::InvalidState
    );
    let (mut session, mut events, receiver, remote_stream) = std::thread::spawn(move || {
        events.negotiate(&mut session, peers, offer);
        events.drain(&mut session, peers);
        let remote = session.remote_streams(peers[1]).unwrap();
        assert_eq!(remote.len(), 1);
        let remote_stream = remote[0];
        assert_eq!(events.added, [(peers[1], remote_stream)]);
        assert_eq!(events.tracks.len(), 1);
        let (peer, receiver, ref streams) = events.tracks[0];
        assert_eq!(peer, peers[1]);
        assert_eq!(session.receivers(peers[1]).unwrap(), [receiver]);
        assert_eq!(streams, &[remote_stream]);
        assert_eq!(session.receiver_streams(receiver).unwrap(), remote);
        assert_eq!(
            session.media_stream_id(remote_stream).unwrap(),
            b"real-plan-b-stream"
        );
        let received_track = session.receiver_track_reference(receiver).unwrap().unwrap();
        assert_eq!(
            session.media_stream_audio_tracks(remote_stream).unwrap(),
            [received_track]
        );
        session
            .register_receiver_packet_observer(receiver, 4)
            .unwrap();
        session.attach_encoded_audio(receiver).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // SDP completion and sender readiness do not prove that the receiving
        // peer's ICE/DTLS transport is ready for the first media packet.
        while peers
            .iter()
            .any(|&peer| session.peer_connection_state(peer).unwrap() != ConnectionState::Connected)
        {
            events.drain(&mut session, peers);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        (session, events, receiver, remote_stream)
    })
    .join()
    .unwrap();
    let (session, mut events) = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let packet = OpusInputFrame {
            data: vec![0xf8, 0xff, 0xfe],
            rtp_timestamp: 0,
            samples_per_channel: 960,
        };
        session.push_opus(source, &packet).unwrap();
        loop {
            events.drain(&mut session, peers);
            if let Some(frame) = session.try_audio_frame(receiver).unwrap() {
                assert_eq!(frame.data, packet.data);
                break;
            }
            assert!(Instant::now() < deadline, "no native Plan B media");
            std::thread::yield_now();
        }
        // Packet-observer delivery is queued separately from media delivery;
        // receiving a frame is not a barrier for the observer callback.
        while session
            .take_receiver_packet_event(receiver)
            .unwrap()
            .is_none()
        {
            events.drain(&mut session, peers);
            assert!(
                Instant::now() < deadline,
                "no native Plan B packet observation"
            );
            std::thread::yield_now();
        }
        session.remove_stream(peers[0], stream).unwrap();
        assert!(session.local_streams(peers[0]).unwrap().is_empty());
        assert_eq!(session.media_stream_audio_tracks(stream).unwrap(), [track]);
        let id = session.create_offer(peers[0], false).unwrap();
        let offer = events
            .finish(&mut session, peers, peers[0], id)
            .unwrap()
            .unwrap();
        events.negotiate(&mut session, peers, offer);
        events.drain(&mut session, peers);
        assert!(session.remote_streams(peers[1]).unwrap().is_empty());
        assert_eq!(events.removed, [(peers[1], remote_stream)]);
        assert_eq!(events.retired, [(peers[1], receiver)]);
        assert_eq!(
            session.media_stream_id(remote_stream).unwrap(),
            b"real-plan-b-stream"
        );
        let id = session.set_local_description_auto_legacy(peers[0]).unwrap();
        session.close_peer(peers[0]).unwrap();
        let result = events.finish(&mut session, peers, peers[0], id);
        assert!(matches!(result, Ok(None) | Err(PeerErrorKind::Closed)));
        events.drain(&mut session, peers);
        assert!(
            !events.done.contains_key(&(peers[0], id)),
            "late duplicate result"
        );
        assert_eq!(
            session.add_stream(peers[0], stream).unwrap_err().kind,
            PeerErrorKind::Closed
        );
        session.close_peer(peers[1]).unwrap();
        session.shutdown().unwrap();
        (session, events)
    })
    .join()
    .unwrap();
    // Move final native retirement once more, not only active operations.
    std::thread::spawn(move || {
        events.done.clear();
        drop(session);
    })
    .join()
    .unwrap();
}

#[test]
fn local_streams_preserve_bytes_and_native_membership_and_guard_unified_plan() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let stream = factory.create_local_media_stream(b"stream\0\xff").unwrap();
    let alias = stream.clone();
    assert!(stream.same_resource(&alias));
    assert_eq!(stream.id(), b"stream\0\xff");
    let source = factory.create_audio_source().unwrap();
    let track = factory
        .create_audio_track("local-stream-track", &source)
        .unwrap()
        .as_media_track();
    assert!(stream.add_track(&track).unwrap());
    assert!(!alias.add_track(&track).unwrap());
    drop(source);
    drop(track);
    let track = stream.audio_tracks().pop().unwrap();
    assert!(
        alias
            .find_audio_track(b"local-stream-track")
            .unwrap()
            .same_resource(&track)
    );
    let distinct = factory
        .create_audio_track(
            "local-stream-track",
            &factory.create_audio_source().unwrap(),
        )
        .unwrap()
        .as_media_track();
    assert!(!distinct.same_resource(&track));
    // SDK removal is by ID. It is not pointer-identity membership.
    assert!(alias.remove_track(&distinct));
    assert!(!alias.remove_track(&track));
    assert!(stream.audio_tracks().is_empty());
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    assert_eq!(
        peer.add_stream(&stream).unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    assert_eq!(
        peer.local_streams().unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    assert_eq!(
        peer.remote_streams().unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    peer.close().unwrap();
    let mut config = RtcConfiguration::default();
    config.options.sdp_semantics = SdpSemantics::PlanB;
    let mut plan_b = factory
        .create_peer_connection_with_configuration(config)
        .unwrap();
    assert_eq!(
        plan_b.audio_transceivers().unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    assert_eq!(
        plan_b.video_transceivers().unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    plan_b.close().unwrap();
}
