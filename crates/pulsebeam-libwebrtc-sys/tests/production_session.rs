use std::{
    collections::HashSet,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

use pulsebeam_webrtc_sys::*;
#[path = "support/frame_crypto.rs"]
mod frame_crypto_support;

#[derive(Default)]
struct Notification {
    ready: Mutex<bool>,
    changed: Condvar,
}
impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        *self.ready.lock().unwrap() = true;
        self.changed.notify_one();
    }
}

fn wait(session: &mut ProductionSession) {
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(notification.clone());
    if session
        .poll_ready(&mut Context::from_waker(&waker))
        .is_ready()
    {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut notified = notification.ready.lock().unwrap();
    while !*notified {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "native readiness did not wake the actor"
        );
        notified = notification
            .changed
            .wait_timeout(notified, remaining)
            .unwrap()
            .0;
    }
}

fn finish(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    operation: OperationId,
) -> Option<SessionDescription> {
    loop {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            if let SessionEvent::OperationComplete(done) = event
                && done.operation_id == operation
            {
                return done.result.unwrap();
            }
        }
        wait(session);
    }
}

#[test]
fn migrated_actor_negotiates_trackless_receivers_and_full_audio_init() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .pcm_audio()
        .build()
        .unwrap();
    let receiver = session.create_peer(PeerConfiguration::default()).unwrap();
    let sender = session.create_peer(PeerConfiguration::default()).unwrap();
    let mut receive_init = RtpTransceiverInit::default();
    receive_init.set_direction(RtpTransceiverDirectionValue::ReceiveOnly);
    let receiving: Vec<_> = [MediaType::Audio, MediaType::Video]
        .into_iter()
        .map(|kind| {
            session
                .add_transceiver(receiver, kind, &receive_init)
                .unwrap()
        })
        .collect();
    let mut send_init = RtpTransceiverInit::default();
    send_init.set_direction(RtpTransceiverDirectionValue::SendOnly);
    send_init.set_stream_ids(&[b"pcm-stream".to_vec()]);
    let source = session.create_pcm_source().unwrap();
    let audio = session
        .publish_audio_with_init(sender, source, "pcm-track", &send_init)
        .unwrap();
    let audio_sender = session.sender(audio).unwrap();
    assert_eq!(
        session.sender_stream_ids(audio_sender).unwrap(),
        send_init.stream_ids()
    );
    drop(receive_init);
    drop(send_init);
    thread::spawn(move || {
        let operation = session.create_offer(receiver, false).unwrap();
        let offer = finish(&mut session, receiver, operation).unwrap();
        assert!(offer.sdp.contains("m=audio"));
        assert!(offer.sdp.contains("m=video"));
        assert_eq!(offer.sdp.matches("a=recvonly").count(), 2);
        let operation = session
            .set_local_description(receiver, offer.clone())
            .unwrap();
        finish(&mut session, receiver, operation);
        let operation = session.set_remote_description(sender, offer).unwrap();
        finish(&mut session, sender, operation);
        // Native remote-offer association does not reuse AddTransceiver-created
        // senders. Configure the actual newly associated audio/video endpoints,
        // without fabricating a local track on either receiving transceiver.
        let offered: Vec<_> = session
            .transceivers(sender)
            .unwrap()
            .into_iter()
            .filter(|&id| id != audio)
            .collect();
        assert_eq!(offered.len(), 2);
        for transceiver in offered {
            session
                .set_direction(transceiver, RtpTransceiverDirection::SendOnly)
                .unwrap();
        }
        let operation = session.create_answer(sender).unwrap();
        let answer = finish(&mut session, sender, operation).unwrap();
        assert_eq!(
            answer.sdp.matches("a=sendonly").count(),
            2,
            "{}",
            answer.sdp
        );
        let operation = session
            .set_local_description(sender, answer.clone())
            .unwrap();
        finish(&mut session, sender, operation);
        let operation = session.set_remote_description(receiver, answer).unwrap();
        finish(&mut session, receiver, operation);
        for transceiver in receiving {
            assert_eq!(
                session.current_direction_value(transceiver).unwrap(),
                Some(RtpTransceiverDirectionValue::ReceiveOnly)
            );
        }
        assert_eq!(
            session.sender_stream_ids(audio_sender).unwrap(),
            [b"pcm-stream".to_vec()]
        );
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn migrated_actor_transports_mapped_opus_capture_time_and_independent_levels() {
    const CAPTURE: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
    #[derive(Default)]
    struct Observations {
        connected: [bool; 2],
        gathered: [bool; 2],
        arrivals: Vec<(SessionTransceiverId, SessionReceiverId)>,
        completed:
            std::collections::HashMap<(SessionPeerId, OperationId), Option<SessionDescription>>,
    }
    impl Observations {
        fn drain(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
            for (index, peer) in peers.into_iter().enumerate() {
                while let Some(event) = session.try_peer_event(peer).unwrap() {
                    match event {
                        SessionEvent::OperationComplete(done) => {
                            self.completed
                                .insert((peer, done.operation_id), done.result.unwrap());
                        }
                        SessionEvent::Track {
                            transceiver,
                            receiver,
                        } if index == 1 => {
                            self.arrivals.push((transceiver, receiver));
                        }
                        SessionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                            self.connected[index] = true
                        }
                        SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                            self.gathered[index] = true
                        }
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
            operation: OperationId,
        ) -> Option<SessionDescription> {
            loop {
                self.drain(session, peers);
                if let Some(result) = self.completed.remove(&(peer, operation)) {
                    return result;
                }
                wait(session);
            }
        }
        fn gather(
            &mut self,
            session: &mut ProductionSession,
            peers: [SessionPeerId; 2],
            index: usize,
            description: SessionDescription,
        ) -> SessionDescription {
            let operation = session
                .set_local_description(peers[index], description)
                .unwrap();
            self.finish(session, peers, peers[index], operation);
            while !self.gathered[index] {
                self.drain(session, peers);
                if !self.gathered[index] {
                    wait(session);
                }
            }
            let descriptions = session.descriptions(peers[index]).unwrap();
            descriptions
                .current_local
                .or(descriptions.pending_local)
                .unwrap()
        }
    }
    fn capture_extension(session: &mut ProductionSession, transceiver: SessionTransceiverId) {
        let mut extensions = session.header_extensions_to_negotiate(transceiver).unwrap();
        extensions
            .iter_mut()
            .find(|e| e.uri() == CAPTURE)
            .expect("provided native capture extension")
            .direction = RtpHeaderExtensionDirection::SendReceive;
        session
            .set_header_extensions_to_negotiate(transceiver, &extensions)
            .unwrap();
    }
    let crypto_trace = Arc::new(frame_crypto_support::Trace::default());
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let mut encrypted_configuration = RtcConfiguration::default();
    encrypted_configuration
        .options
        .crypto_options
        .require_frame_encryption = true;
    let peers = [
        session
            .create_peer_with_configuration(encrypted_configuration.clone())
            .unwrap(),
        session
            .create_peer_with_configuration(encrypted_configuration)
            .unwrap(),
    ];
    let sources = [
        session.create_opus_source(1).unwrap(),
        session.create_opus_source(1).unwrap(),
    ];
    let mut outgoing = Vec::new();
    for (index, source) in sources.into_iter().enumerate() {
        let transceiver = session
            .publish_opus(
                peers[0],
                source,
                &format!("capture-{index}"),
                RtpTransceiverDirection::SendOnly,
            )
            .unwrap();
        capture_extension(&mut session, transceiver);
        let sender = session.sender(transceiver).unwrap();
        let encryptor = session
            .create_frame_encryptor(frame_crypto_support::Cipher::encoder(crypto_trace.clone()))
            .unwrap();
        session
            .set_sender_frame_encryptor(sender, Some(encryptor))
            .unwrap();
        assert_eq!(
            session.sender_frame_encryptor(sender).unwrap(),
            Some(encryptor)
        );
        session.release_frame_encryptor(encryptor).unwrap();
        outgoing.push(transceiver);
    }
    let receive_crypto_trace = crypto_trace.clone();
    let (session, receivers) = thread::spawn(move || {
        let mut session = session;
        let mut observed = Observations::default();
        let op = session.create_offer(peers[0], false).unwrap();
        let offer = observed.finish(&mut session, peers, peers[0], op).unwrap();
        let offer = observed.gather(&mut session, peers, 0, offer);
        let op = session.set_remote_description(peers[1], offer).unwrap();
        observed.finish(&mut session, peers, peers[1], op);
        assert_eq!(observed.arrivals.len(), 2);
        for (transceiver, _) in &observed.arrivals {
            capture_extension(&mut session, *transceiver);
        }
        let op = session.create_answer(peers[1]).unwrap();
        let answer = observed.finish(&mut session, peers, peers[1], op).unwrap();
        let answer = observed.gather(&mut session, peers, 1, answer);
        let op = session.set_remote_description(peers[0], answer).unwrap();
        observed.finish(&mut session, peers, peers[0], op);
        while observed.connected != [true; 2] {
            observed.drain(&mut session, peers);
            if observed.connected != [true; 2] {
                wait(&mut session);
            }
        }
        for (_, receiver) in &observed.arrivals {
            let snapshot = session.get_receiver_parameters(*receiver).unwrap();
            assert!(!snapshot.codecs().is_empty());
            assert_eq!(
                session.set_receiver_parameters(*receiver, &snapshot),
                Ok(false)
            );
            for delay in [Some(0.0), Some(f64::INFINITY), None] {
                session
                    .set_receiver_jitter_buffer_minimum_delay(*receiver, delay)
                    .unwrap();
            }
            assert_eq!(
                session.set_receiver_jitter_buffer_minimum_delay(*receiver, Some(f64::NAN)),
                Err(pulsebeam_webrtc_sys::RtpReceiverOperationError::UnsafeDelayNaN)
            );
            session.receiver_sources(*receiver).unwrap();
            let decryptor = session
                .create_frame_decryptor(
                    *receiver,
                    frame_crypto_support::Cipher::decoder(receive_crypto_trace.clone()),
                )
                .unwrap();
            session
                .set_receiver_frame_decryptor(*receiver, Some(decryptor))
                .unwrap();
            assert_eq!(
                session.receiver_frame_decryptor(*receiver).unwrap(),
                Some(decryptor)
            );
            session.release_frame_decryptor(decryptor).unwrap();
            session.attach_encoded_audio(*receiver).unwrap();
        }
        for transceiver in outgoing
            .iter()
            .copied()
            .chain(observed.arrivals.iter().map(|v| v.0))
        {
            assert_eq!(
                session
                    .negotiated_header_extensions(transceiver)
                    .unwrap()
                    .iter()
                    .find(|e| e.uri() == CAPTURE)
                    .unwrap()
                    .direction,
                RtpHeaderExtensionDirection::SendReceive
            );
        }
        (
            session,
            observed.arrivals.iter().map(|v| v.1).collect::<Vec<_>>(),
        )
    })
    .join()
    .unwrap();
    let send_crypto_trace = crypto_trace.clone();
    let (session, frames) = thread::spawn(move || {
        let mut session = session;
        let media_caller_thread = thread::current().id();
        let mut packet = OpusInputFrame {
            data: vec![0xf8, 0xff, 0xfe],
            rtp_timestamp: 0,
            samples_per_channel: 960,
        };
        // Connected is not proof of first-packet delivery. Prime both native
        // carrier/send paths at audio cadence and require an actual received
        // warmup frame per receiver before the two independently marked probes.
        let warmup_level = OpusAudioLevel::new(0, true).unwrap();
        let mut warmed = [false; 2];
        let warmup_deadline = Instant::now() + Duration::from_secs(15);
        while warmed != [true; 2] {
            let warmup_time =
                session.opus_capture_time_now(sources[0]).unwrap() - Duration::from_secs(1);
            for source in sources {
                session
                    .push_opus_at_with_audio_level(source, &packet, warmup_time, warmup_level)
                    .unwrap();
            }
            packet.rtp_timestamp = packet.rtp_timestamp.wrapping_add(960);
            for (index, receiver) in receivers.iter().enumerate() {
                while let Some(frame) = session.try_audio_frame(*receiver).unwrap() {
                    assert_eq!(frame.data, packet.data);
                    assert_eq!(frame.audio_level_dbov, Some(warmup_level.level_dbov()));
                    assert_eq!(frame.voice_activity, Some(warmup_level.voice_activity()));
                    warmed[index] = true;
                }
            }
            assert!(
                Instant::now() < warmup_deadline,
                "Opus warmup receivers={warmed:?}"
            );
            if warmed != [true; 2] {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let capture_now = session.opus_capture_time_now(sources[0]).unwrap();
        let wall_ntp_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64
            + 2_208_988_800_000_000;
        assert!(
            session
                .push_opus_at(sources[0], &packet, capture_now + Duration::from_secs(60))
                .is_err()
        );
        let levels = [
            OpusAudioLevel::new(27, true).unwrap(),
            OpusAudioLevel::new(83, false).unwrap(),
        ];
        let offsets = [Duration::from_millis(400), Duration::from_millis(200)];
        for index in 0..2 {
            session
                .push_opus_at_with_audio_level(
                    sources[index],
                    &packet,
                    capture_now - offsets[index],
                    levels[index],
                )
                .unwrap();
        }
        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while frames.len() < 2 {
            for receiver in &receivers {
                while let Some(frame) = session.try_audio_frame(*receiver).unwrap() {
                    if frame.audio_level_dbov == Some(warmup_level.level_dbov()) {
                        assert_eq!(frame.data, packet.data);
                        assert_eq!(frame.voice_activity, Some(warmup_level.voice_activity()));
                        continue;
                    }
                    frames.push(frame);
                }
            }
            if frames.len() < 2 {
                // Actor readiness coalesces peer activity, not encoded sink
                // arrivals. Drain the actual media queues with a bounded wait.
                assert!(
                    Instant::now() < deadline,
                    "received only {} of two mapped Opus frames; crypto calls={}",
                    frames.len(),
                    send_crypto_trace.calls.lock().unwrap().len()
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(frames.len(), 2);
        assert_ne!(frames[0].ssrc, frames[1].ssrc);
        for frame in &frames {
            assert_eq!(frame.data, packet.data);
            let index = levels
                .iter()
                .position(|v| Some(v.level_dbov()) == frame.audio_level_dbov)
                .unwrap();
            assert_eq!(frame.voice_activity, Some(levels[index].voice_activity()));
            let capture = frame
                .capture_time_us
                .expect("actual negotiated received capture metadata");
            let expected = wall_ntp_us - offsets[index].as_micros() as i64;
            assert!(
                (capture - expected).abs() < 5_000,
                "capture={capture}, expected={expected}"
            );
        }
        let calls = send_crypto_trace.calls.lock().unwrap();
        assert!(calls.iter().filter(|call| call.encrypt).count() >= 2);
        assert!(calls.iter().filter(|call| !call.encrypt).count() >= 2);
        assert!(
            calls
                .iter()
                .all(|call| call.media == MediaType::Audio && call.thread != media_caller_thread)
        );
        assert!(
            calls
                .iter()
                .filter(|call| call.encrypt)
                .all(|call| call.ssrc != 0 && call.input == packet.data)
        );
        assert!(
            calls
                .iter()
                .filter(|call| !call.encrypt)
                .all(|call| call.input.first() == Some(&0xa5))
        );
        drop(calls);
        // Retire actor ownership and preserve already-returned owned output.
        for source in sources {
            session.close_source(source).unwrap();
        }
        assert!(session.opus_capture_time_now(sources[0]).is_err());
        (session, frames)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        session.shutdown().unwrap();
        drop(session);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, vec![0xf8, 0xff, 0xfe]);
        assert_eq!(
            crypto_trace
                .encoder_drops
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            crypto_trace
                .decoder_drops
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let _world = ControlledWorld::acquire(742, Duration::from_secs(1)).unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn session_owns_live_media_graph_across_caller_migration_and_final_drop() {
    fn assert_send<T: Send>() {}
    assert_send::<ProductionSession>();
    fn assert_send_future(_: impl std::future::Future + Send) {}
    assert_send_future(async {
        let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
        session.ready().await;
        session.shutdown().unwrap();
    });

    let h264 = VideoCodecFormat::new("H264")
        .with_parameter("level-asymmetry-allowed", "1")
        .with_parameter("packetization-mode", "1")
        .with_parameter("profile-level-id", "42e01f");
    let mut session = ProductionSession::builder(ProductionSessionConfig {
        video_format: Some(h264),
    })
    .encoded_audio()
    .build()
    .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let channel = session
        .create_channel(peer, "migrating", DataChannelConfiguration::default())
        .unwrap();
    let opus = session.create_opus_source(1).unwrap();
    session
        .publish_opus(peer, opus, "voice", RtpTransceiverDirection::SendOnly)
        .unwrap();
    let video = session.create_video_source().unwrap();
    let transceiver = session
        .publish_video(
            peer,
            video,
            "camera",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    session.register_sender_packet_observer(sender, 2).unwrap();
    session
        .register_receiver_packet_observer(receiver, 2)
        .unwrap();
    assert!(ControlledWorld::acquire(9, Duration::from_secs(1)).is_err());

    let capabilities = session
        .get_rtp_sender_capabilities(pulsebeam_webrtc_sys::MediaType::Audio)
        .unwrap();
    assert!(!capabilities.codecs().is_empty());
    let (session, offer) = thread::spawn(move || {
        assert_eq!(
            session
                .get_rtp_sender_capabilities(pulsebeam_webrtc_sys::MediaType::Audio)
                .unwrap(),
            capabilities
        );
        assert!(
            !session
                .get_rtp_receiver_capabilities(pulsebeam_webrtc_sys::MediaType::Audio)
                .unwrap()
                .codecs()
                .is_empty()
        );
        assert_eq!(
            session.sender_parameters(sender).unwrap().encodings.len(),
            1
        );
        assert_eq!(
            session
                .get_sender_parameters(sender)
                .unwrap()
                .encodings()
                .len(),
            1
        );
        assert!(session.register_sender_packet_observer(sender, 1).is_err());
        assert!(session.take_sender_packet_event(sender).unwrap().is_none());
        assert!(
            session
                .take_receiver_packet_event(receiver)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            session.take_sender_dropped_packet_events(sender).unwrap(),
            0
        );
        assert_eq!(
            session
                .take_receiver_dropped_packet_events(receiver)
                .unwrap(),
            0
        );
        let offer_id = session.create_offer(peer, false).unwrap();
        let offer = finish(&mut session, peer, offer_id).unwrap();
        assert!(offer.sdp.contains("H264/90000"));
        assert!(offer.sdp.contains("opus/48000"));
        (session, offer)
    })
    .join()
    .unwrap();

    let session = thread::spawn(move || {
        let mut session = session;
        let set_id = session.set_local_description(peer, offer).unwrap();
        finish(&mut session, peer, set_id);
        assert!(session.descriptions(peer).unwrap().pending_local.is_some());
        session.replace_video_source(sender, None, "").unwrap();
        session
            .replace_video_source(sender, Some(video), "replacement")
            .unwrap();
        session
            .push_video(
                video,
                EncodedVideoAccessUnit {
                    data: vec![
                        0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 0x88,
                    ],
                    width: 16,
                    height: 16,
                    timestamp_us: 1_000_000,
                    key_frame: true,
                    qp: None,
                    metadata: EncodedVideoMetadata {
                        codec: EncodedVideoCodec::H264 {
                            base_layer_sync: false,
                        },
                        simulcast_index: None,
                        spatial_index: None,
                        temporal_index: None,
                        end_of_picture: true,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            session
                .send(
                    channel,
                    DataChannelMessage {
                        kind: DataChannelMessageKind::Binary,
                        bytes: vec![1, 2, 3],
                    }
                )
                .unwrap(),
            DataChannelSendResult::NotOpen
        );
        session
    })
    .join()
    .unwrap();

    thread::spawn(move || {
        let mut session = session;
        session.close_channel(channel).unwrap();
        session.close_peer(peer).unwrap();
        session.unregister_sender_packet_observer(sender).unwrap();
        session
            .unregister_receiver_packet_observer(receiver)
            .unwrap();
        assert!(session.take_sender_packet_event(sender).unwrap().is_none());
        assert!(
            session
                .take_receiver_packet_event(receiver)
                .unwrap()
                .is_none()
        );
        session.close_peer(peer).unwrap();
        session.shutdown().unwrap();
        session.shutdown().unwrap();
        drop(session);
        let world = ControlledWorld::acquire(10, Duration::from_secs(1)).unwrap();
        assert_eq!(world.pump(0).dispatched, 0);
        drop(world);
    })
    .join()
    .unwrap();
}

#[test]
fn temporal_actor_keeps_native_extension_control_owned_across_migration() {
    const DD: &str =
        "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
    const VLA: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";
    use pulsebeam_webrtc_sys::RtpHeaderExtensionDirection;
    assert!(
        ProductionSession::new_l1t3(ProductionSessionConfig::default(), [64, 128, 255]).is_err()
    );
    let h264 = VideoCodecFormat::new("H264")
        .with_parameter("level-asymmetry-allowed", "1")
        .with_parameter("packetization-mode", "1")
        .with_parameter("profile-level-id", "42e01f");
    let mut session = ProductionSession::new_l1t3(
        ProductionSessionConfig {
            video_format: Some(h264),
        },
        [64, 128, 255],
    )
    .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_video_source().unwrap();
    let transceiver = session
        .publish_video(
            peer,
            source,
            "temporal",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let defaults = session.header_extensions_to_negotiate(transceiver).unwrap();
    let (mut session, configured) = thread::spawn(move || {
        let mut configured = defaults.clone();
        for uri in [DD, VLA] {
            let extension = configured
                .iter_mut()
                .find(|extension| extension.uri() == uri)
                .unwrap();
            assert_eq!(extension.direction, RtpHeaderExtensionDirection::Stopped);
            extension.direction = RtpHeaderExtensionDirection::SendReceive;
        }
        session
            .set_header_extensions_to_negotiate(transceiver, &configured)
            .unwrap();
        assert_eq!(
            session.header_extensions_to_negotiate(transceiver).unwrap(),
            configured
        );
        assert!(
            session
                .negotiated_header_extensions(transceiver)
                .unwrap()
                .iter()
                .all(|extension| extension.direction == RtpHeaderExtensionDirection::Stopped)
        );
        let mut parameters = session.sender_parameters(sender).unwrap();
        parameters.encodings[0].scalability_mode = Some("L1T3".into());
        assert!(session.set_sender_parameters(sender, parameters).is_err());
        assert_eq!(session.take_video_encoder_error(source).unwrap(), None);
        assert_eq!(session.video_feedback(source).unwrap(), (false, None));
        let operation = session.create_offer(peer, false).unwrap();
        let offer = finish(&mut session, peer, operation).unwrap();
        assert!(offer.sdp.contains("H264/90000"));
        assert!(offer.sdp.contains(DD));
        assert!(offer.sdp.contains(VLA));
        (session, configured)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        assert_eq!(
            session.header_extensions_to_negotiate(transceiver).unwrap(),
            configured
        );
        session.close_peer(peer).unwrap();
        assert!(session.header_extensions_to_negotiate(transceiver).is_err());
        session.shutdown().unwrap();
        drop(session);
        // Snapshots are independently owned, not handles into the native graph.
        assert!(configured.iter().any(|extension| extension.uri() == DD));
    })
    .join()
    .unwrap();
}

#[test]
fn migrated_temporal_actor_receives_native_dd_and_wakes_for_rejection() {
    const DD: &str =
        "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
    const KEY: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0,
        0x3c, 0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88,
        0x84, 0xf1, 0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
    ];
    fn outcome(
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        source: SessionSourceId,
        receiver: SessionReceiverId,
    ) -> Result<EncodedReceivedVideoFrame, CodecError> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            for peer in peers {
                while session.try_peer_event(peer).unwrap().is_some() {}
            }
            if let Some(error) = session.take_video_encoder_error(source).unwrap() {
                return Err(error);
            }
            if let Some(frame) = session.try_video_frame(receiver).unwrap() {
                return Ok(frame);
            }
            assert!(
                Instant::now() < deadline,
                "temporal media or rejection did not arrive"
            );
            wait(session);
        }
    }

    for selected in [false, true] {
        let format = VideoCodecFormat::new("H264")
            .with_parameter("level-asymmetry-allowed", "1")
            .with_parameter("packetization-mode", "1")
            .with_parameter("profile-level-id", "42e01f");
        let mut session = ProductionSession::new_l1t3(
            ProductionSessionConfig {
                video_format: Some(format),
            },
            [64, 128, 255],
        )
        .unwrap();
        let peers = [
            session.create_peer(PeerConfiguration::default()).unwrap(),
            session.create_peer(PeerConfiguration::default()).unwrap(),
        ];
        let source = session.create_video_source().unwrap();
        let outgoing = session
            .publish_video(
                peers[0],
                source,
                "temporal",
                RtpTransceiverDirection::SendOnly,
                &[],
            )
            .unwrap();
        for transceiver in [outgoing] {
            let mut extensions = session.header_extensions_to_negotiate(transceiver).unwrap();
            extensions
                .iter_mut()
                .find(|extension| extension.uri() == DD)
                .unwrap()
                .direction = RtpHeaderExtensionDirection::SendReceive;
            session
                .set_header_extensions_to_negotiate(transceiver, &extensions)
                .unwrap();
        }
        let session = thread::spawn(move || {
            let operation = session.create_offer(peers[0], false).unwrap();
            let offer = finish(&mut session, peers[0], operation).unwrap();
            let offer = set_local_and_gather(&mut session, peers[0], offer);
            let operation = session.set_remote_description(peers[1], offer).unwrap();
            let mut completed = false;
            let mut arrival = None;
            while !completed || arrival.is_none() {
                while let Some(event) = session.try_peer_event(peers[1]).unwrap() {
                    match event {
                        SessionEvent::OperationComplete(done) if done.operation_id == operation => {
                            done.result.unwrap();
                            completed = true;
                        }
                        SessionEvent::Track {
                            transceiver,
                            receiver,
                        } => {
                            arrival = Some((transceiver, receiver));
                        }
                        _ => {}
                    }
                }
                if !completed || arrival.is_none() {
                    wait(&mut session);
                }
            }
            let (incoming, receiver) = arrival.unwrap();
            // Native JSEP creates the offered receiver instead of reusing a
            // locally addTransceiver-created resource. Configure the actual
            // owned arrival after the offer, before creating the answer.
            let mut extensions = session.header_extensions_to_negotiate(incoming).unwrap();
            extensions
                .iter_mut()
                .find(|extension| extension.uri() == DD)
                .unwrap()
                .direction = RtpHeaderExtensionDirection::SendReceive;
            session
                .set_header_extensions_to_negotiate(incoming, &extensions)
                .unwrap();
            let operation = session.create_answer(peers[1]).unwrap();
            let answer = finish(&mut session, peers[1], operation).unwrap();
            let answer = set_local_and_gather(&mut session, peers[1], answer);
            let operation = session.set_remote_description(peers[0], answer).unwrap();
            finish(&mut session, peers[0], operation);
            for transceiver in [outgoing, incoming] {
                assert_eq!(
                    session
                        .negotiated_header_extensions(transceiver)
                        .unwrap()
                        .iter()
                        .find(|extension| extension.uri() == DD)
                        .unwrap()
                        .direction,
                    RtpHeaderExtensionDirection::SendReceive
                );
            }
            if selected {
                let sender = session.sender(outgoing).unwrap();
                let mut parameters = session.sender_parameters(sender).unwrap();
                parameters.encodings[0].scalability_mode = Some("L1T3".into());
                session.set_sender_parameters(sender, parameters).unwrap();
            }
            assert_eq!(session.receiver(incoming).unwrap(), receiver);
            // This explicitly encoded-only factory cannot supply decoded
            // output. Refusal must not burn the native encoded reservation.
            assert!(session.attach_raw_and_encoded_video(receiver).is_err());
            session.attach_encoded_video(receiver).unwrap();
            assert!(session.dropped_raw_video_frames(receiver).is_err());
            assert_eq!(session.dropped_encoded_video_frames(receiver).unwrap(), 0);
            (session, receiver)
        })
        .join()
        .unwrap();
        let (session, receiver) = session;
        let (session, owned) = thread::spawn(move || {
            let mut session = session;
            let mut unit = EncodedVideoAccessUnit {
                data: KEY.to_vec(),
                width: 16,
                height: 16,
                timestamp_us: i64::try_from(SystemClock.now().as_micros()).unwrap(),
                key_frame: true,
                qp: None,
                metadata: EncodedVideoMetadata {
                    codec: EncodedVideoCodec::H264 {
                        base_layer_sync: false,
                    },
                    simulcast_index: None,
                    spatial_index: None,
                    temporal_index: Some(0),
                    end_of_picture: true,
                },
            };
            session.push_video(source, unit.clone()).unwrap();
            let first = outcome(&mut session, peers, source, receiver);
            if !selected {
                assert_eq!(first.unwrap_err(), CodecError::InvalidConfiguration);
                assert!(session.try_video_frame(receiver).unwrap().is_none());
                return (session, None);
            }
            let first = first.unwrap();
            assert_eq!(first.data, KEY);
            assert!(first.key_frame);
            assert_eq!(first.temporal_index, Some(0));
            assert!(first.dependencies.is_empty());
            let mut base_id = first.frame_id.unwrap();
            assert_eq!(
                first.decode_target_indications,
                vec![DecodeTargetIndication::Switch; 4]
            );
            assert!(
                session
                    .video_feedback(source)
                    .unwrap()
                    .1
                    .unwrap()
                    .layer_bitrates_bps[0][2]
                    .is_some()
            );
            let mut delivered = None;
            for _ in 0..4 {
                unit.timestamp_us += 33_333;
                unit.data = vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1, 7];
                unit.key_frame = false;
                unit.metadata.temporal_index = Some(2);
                unit.metadata.codec = EncodedVideoCodec::H264 {
                    base_layer_sync: true,
                };
                session.push_video(source, unit.clone()).unwrap();
                match outcome(&mut session, peers, source, receiver) {
                    Ok(frame) => {
                        delivered = Some(frame);
                        break;
                    }
                    Err(error) => {
                        assert_eq!(error, CodecError::InvalidFrame);
                        assert!(session.video_feedback(source).unwrap().0);
                        let mut key = unit.clone();
                        key.timestamp_us += 33_333;
                        key.data = KEY.to_vec();
                        key.key_frame = true;
                        key.metadata.temporal_index = Some(0);
                        key.metadata.codec = EncodedVideoCodec::H264 {
                            base_layer_sync: false,
                        };
                        session.push_video(source, key.clone()).unwrap();
                        let frame = outcome(&mut session, peers, source, receiver).unwrap();
                        assert_eq!(frame.data, KEY);
                        base_id = frame.frame_id.unwrap();
                        session.video_feedback(source).unwrap();
                        unit.timestamp_us = key.timestamp_us;
                    }
                }
            }
            let delivered = delivered.expect("producer delta after native keyframe feedback");
            assert_eq!(delivered.data, unit.data);
            assert_eq!(delivered.temporal_index, Some(2));
            assert_eq!(delivered.ssrc, first.ssrc);
            assert!(delivered.frame_id.unwrap() > base_id);
            assert_eq!(delivered.dependencies, vec![base_id]);
            assert_eq!(
                delivered.decode_target_indications,
                vec![
                    DecodeTargetIndication::NotPresent,
                    DecodeTargetIndication::NotPresent,
                    DecodeTargetIndication::Switch,
                    DecodeTargetIndication::Switch
                ]
            );
            assert_eq!(session.dropped_frames(receiver).unwrap(), 0);
            (session, Some(delivered))
        })
        .join()
        .unwrap();
        thread::spawn(move || {
            let mut session = session;
            session.shutdown().unwrap();
            drop(session);
            if let Some(frame) = owned {
                assert_eq!(frame.data, vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1, 7]);
            }
            let world = ControlledWorld::acquire(741, Duration::from_secs(1)).unwrap();
            assert_eq!(world.pump(0).dispatched, 0);
            drop(world);
        })
        .join()
        .unwrap();
    }
}

fn set_local_and_gather(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    description: SessionDescription,
) -> SessionDescription {
    let operation = session.set_local_description(peer, description).unwrap();
    let mut complete = false;
    let mut gathered = false;
    while !complete || !gathered {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            match event {
                SessionEvent::OperationComplete(done) if done.operation_id == operation => {
                    done.result.unwrap();
                    complete = true;
                }
                SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                    gathered = true;
                }
                _ => {}
            }
        }
        if !complete || !gathered {
            wait(session);
        }
    }
    let descriptions = session.descriptions(peer).unwrap();
    descriptions
        .current_local
        .or(descriptions.pending_local)
        .unwrap()
}

#[test]
fn migrated_actor_preserves_native_signaling_choices() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let alice = session.create_peer(PeerConfiguration::default()).unwrap();
    let bob = session.create_peer(PeerConfiguration::default()).unwrap();
    assert_eq!(
        session.peer_signaling_state(alice).unwrap(),
        SignalingState::Stable
    );
    assert_eq!(
        session.peer_connection_state(alice).unwrap(),
        ConnectionState::New
    );
    assert_eq!(
        session.peer_ice_gathering_state(alice).unwrap(),
        IceGatheringState::New
    );
    session
        .create_channel(alice, "auto", DataChannelConfiguration::default())
        .unwrap();
    let operation = session.set_local_description_auto(alice).unwrap();
    let (session, offer) = thread::spawn(move || {
        let mut session = session;
        assert_eq!(finish(&mut session, alice, operation), None);
        let offer = session
            .descriptions(alice)
            .unwrap()
            .local()
            .unwrap()
            .clone();
        assert_eq!(offer.kind, SessionDescriptionType::Offer);
        assert_eq!(
            session.peer_signaling_state(alice).unwrap(),
            SignalingState::HaveLocalOffer
        );
        let operation = session.set_remote_description(bob, offer.clone()).unwrap();
        finish(&mut session, bob, operation);
        assert_eq!(
            session.peer_signaling_state(bob).unwrap(),
            SignalingState::HaveRemoteOffer
        );
        let candidate = IceCandidate {
            sdp_mid: offer
                .sdp
                .lines()
                .find_map(|line| line.strip_prefix("a=mid:"))
                .unwrap()
                .into(),
            sdp_mline_index: 0,
            candidate: "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host".into(),
        };
        assert_eq!(
            session
                .add_ice_candidate_sync(bob, candidate.clone())
                .unwrap(),
            IceCandidateDisposition::Accepted
        );
        assert_eq!(
            session.remove_ice_candidate(bob, candidate).unwrap(),
            IceCandidateDisposition::Accepted
        );
        let operation = session
            .create_answer_with_options(
                bob,
                OfferAnswerOptions {
                    use_rtp_mux: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let unbundled = finish(&mut session, bob, operation).unwrap();
        // Native answers preserve an offered BUNDLE group even when the
        // option excludes all MIDs, as required by RFC 5888.
        let bundle_groups: Vec<_> = unbundled
            .sdp
            .lines()
            .filter(|line| line.starts_with("a=group:BUNDLE"))
            .collect();
        assert_eq!(bundle_groups, ["a=group:BUNDLE"]);
        let operation = session.set_local_description_auto(bob).unwrap();
        assert_eq!(finish(&mut session, bob, operation), None);
        let answer = session.descriptions(bob).unwrap().local().unwrap().clone();
        assert_eq!(answer.kind, SessionDescriptionType::Answer);
        let operation = session.set_remote_description(alice, answer).unwrap();
        finish(&mut session, alice, operation);
        (session, offer)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        assert!(
            !session
                .should_fire_negotiation_needed(alice, u32::MAX)
                .unwrap()
        );
        session.restart_ice(alice).unwrap();
        let operation = session
            .create_offer_with_options(alice, OfferAnswerOptions::default())
            .unwrap();
        let restart = finish(&mut session, alice, operation).unwrap();
        let ufrag = |sdp: &str| {
            sdp.lines()
                .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
                .unwrap()
                .to_owned()
        };
        assert_ne!(ufrag(&offer.sdp), ufrag(&restart.sdp));
        session.shutdown().unwrap();
        assert_eq!(
            session.peer_signaling_state(alice).unwrap(),
            SignalingState::Closed
        );
        assert_eq!(
            session.peer_connection_state(alice).unwrap(),
            ConnectionState::Closed
        );
        assert_eq!(
            session.restart_ice(alice).unwrap_err().kind,
            PeerErrorKind::Closed
        );
    })
    .join()
    .unwrap();
}

#[test]
fn migrated_actor_is_woken_for_real_reliable_channel_delivery() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let alice = session.create_peer(PeerConfiguration::default()).unwrap();
    let bob = session.create_peer(PeerConfiguration::default()).unwrap();
    let config = DataChannelConfiguration {
        negotiated: true,
        id: Some(0),
        ..DataChannelConfiguration::default()
    };
    let outgoing = session
        .create_channel(alice, "actor", config.clone())
        .unwrap();
    let incoming = session.create_channel(bob, "actor", config).unwrap();
    let session = thread::spawn(move || {
        let mut session = session;
        let operation = session.create_offer(alice, false).unwrap();
        let offer = finish(&mut session, alice, operation).unwrap();
        let offer = set_local_and_gather(&mut session, alice, offer);
        let operation = session.set_remote_description(bob, offer).unwrap();
        finish(&mut session, bob, operation);
        let operation = session.create_answer(bob).unwrap();
        let answer = finish(&mut session, bob, operation).unwrap();
        let answer = set_local_and_gather(&mut session, bob, answer);
        let operation = session.set_remote_description(alice, answer).unwrap();
        finish(&mut session, alice, operation);
        session
    })
    .join()
    .unwrap();
    let session = thread::spawn(move || {
        let mut session = session;
        assert!(ProductionSession::channel_send_queue_capacity() > 0);
        assert_eq!(session.channel_error(outgoing).unwrap(), None);
        assert_eq!(session.channel_error(incoming).unwrap(), None);
        let mut opened = HashSet::new();
        while opened.len() != 2 {
            for channel in [outgoing, incoming] {
                while let Some(event) = session.try_channel_event(channel).unwrap() {
                    if let DataChannelEvent::StateChanged(DataChannelState::Open) = event {
                        opened.insert(channel);
                    }
                }
            }
            if opened.len() != 2 {
                wait(&mut session);
            }
        }
        assert_eq!(session.channel_label(outgoing).unwrap(), b"actor");
        assert_eq!(session.channel_configuration(outgoing).unwrap().id, Some(0));
        assert_eq!(
            session.channel_state(outgoing).unwrap(),
            DataChannelState::Open
        );
        assert!(session.channel_reliable(outgoing).unwrap());
        assert_eq!(
            session.channel_counters(outgoing).unwrap(),
            DataChannelCounters::default()
        );
        session
            .set_channel_event_observation(outgoing, false)
            .unwrap();
        let mut outstanding = HashSet::new();
        for index in 0..64u32 {
            outstanding.insert(
                session
                    .send_async(outgoing, DataChannelMessage::binary(index.to_be_bytes()))
                    .unwrap(),
            );
        }
        assert_eq!(
            session
                .send_async(outgoing, DataChannelMessage::binary([0]))
                .unwrap_err()
                .kind,
            PeerErrorKind::ResourceExhausted
        );
        let mut received = Vec::new();
        while received.len() < 64 || !outstanding.is_empty() {
            while let Some(event) = session.try_channel_event(outgoing).unwrap() {
                if let DataChannelEvent::SendComplete(done) = event {
                    assert!(outstanding.remove(&done.send_id));
                    assert_eq!(done.result, Ok(()));
                }
            }
            while let Some(event) = session.try_channel_event(incoming).unwrap() {
                if let DataChannelEvent::Message(message) = event {
                    received.push(message.bytes);
                }
            }
            if received.len() < 64 || !outstanding.is_empty() {
                wait(&mut session);
            }
        }
        assert_eq!(
            received,
            (0..64u32)
                .map(|n| n.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            session.channel_counters(outgoing).unwrap().messages_sent,
            64
        );
        assert_eq!(session.channel_counters(outgoing).unwrap().bytes_sent, 256);
        assert_eq!(
            session
                .channel_counters(incoming)
                .unwrap()
                .messages_received,
            64
        );
        assert_eq!(
            session.channel_counters(incoming).unwrap().bytes_received,
            256
        );
        assert_eq!(session.channel_buffered_amount(outgoing).unwrap(), 0);
        for peer in [alice, bob] {
            assert_eq!(
                session.peer_connection_state(peer).unwrap(),
                ConnectionState::Connected
            );
            assert_eq!(
                session.peer_ice_gathering_state(peer).unwrap(),
                IceGatheringState::Complete
            );
        }
        let limits = ProductionSession::channel_receive_queue_limits();
        assert_eq!(limits, DataChannel::receive_queue_limits());
        for index in 0..limits.max_messages + 2 {
            assert_eq!(
                session
                    .send(
                        outgoing,
                        DataChannelMessage::binary((index as u32).to_be_bytes())
                    )
                    .unwrap(),
                DataChannelSendResult::Sent,
            );
        }
        while session
            .channel_counters(incoming)
            .unwrap()
            .messages_received
            < 64 + limits.max_messages as u32 + 2
        {
            wait(&mut session);
        }
        // Migrate with the full inbox and overflow notification still pending.
        session
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        let limits = ProductionSession::channel_receive_queue_limits();
        let mut received = Vec::new();
        let mut dropped = (0, 0);
        while let Some(event) = session.try_channel_event(incoming).unwrap() {
            match event {
                DataChannelEvent::Message(message) => received.push(message.bytes),
                DataChannelEvent::MessagesDropped { messages, bytes } => {
                    dropped.0 += messages;
                    dropped.1 += bytes;
                }
                _ => {}
            }
        }
        assert_eq!(
            received,
            (0..limits.max_messages as u32)
                .map(|n| n.to_be_bytes().to_vec())
                .collect::<Vec<_>>()
        );
        assert_eq!(dropped, (2, 8));
        assert_eq!(
            session.channel_state(incoming).unwrap(),
            DataChannelState::Open
        );
        assert_eq!(session.channel_error(incoming).unwrap(), None);
        session.close_peer(bob).unwrap();
        assert_eq!(
            session.channel_state(incoming).unwrap(),
            DataChannelState::Closed
        );
        assert_eq!(
            session
                .send(incoming, DataChannelMessage::text("closed"))
                .unwrap(),
            DataChannelSendResult::NotOpen
        );
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn accepted_operations_are_bounded_until_outcomes_are_consumed_and_survive_close() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let mut accepted = HashSet::new();
    for _ in 0..64 {
        // Parse rejection still reserves and delivers exactly one terminal result.
        accepted.insert(
            session
                .add_ice_candidate(
                    peer,
                    IceCandidate {
                        sdp_mid: String::new(),
                        sdp_mline_index: 0,
                        candidate: "invalid".into(),
                    },
                )
                .unwrap(),
        );
    }
    assert_eq!(
        session.create_offer(peer, false).unwrap_err().kind,
        PeerErrorKind::ResourceExhausted
    );
    let session = thread::spawn(move || {
        let mut session = session;
        session.close_peer(peer).unwrap();
        let mut observed = HashSet::new();
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            if let SessionEvent::OperationComplete(done) = event {
                assert!(done.result.is_err());
                assert!(
                    observed.insert(done.operation_id),
                    "duplicate terminal outcome"
                );
            }
        }
        assert_eq!(observed, accepted);
        assert_eq!(
            session.create_offer(peer, false).unwrap_err().kind,
            PeerErrorKind::Closed
        );
        session
    })
    .join()
    .unwrap();
    drop(session);
}

#[test]
fn sframe_after_negotiated_disabled_preserves_native_invalid_modification() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let alice = session.create_peer(PeerConfiguration::default()).unwrap();
    let bob = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            alice,
            source,
            "sframe-disabled",
            RtpTransceiverDirection::SendReceive,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), None);
    let operation = session.create_offer(alice, false).unwrap();
    let offer = finish(&mut session, alice, operation).unwrap();
    let operation = session.set_local_description(alice, offer.clone()).unwrap();
    finish(&mut session, alice, operation);
    let operation = session.set_remote_description(bob, offer).unwrap();
    finish(&mut session, bob, operation);
    let operation = session.create_answer(bob).unwrap();
    let answer = finish(&mut session, bob, operation).unwrap();
    let operation = session.set_local_description(bob, answer.clone()).unwrap();
    finish(&mut session, bob, operation);
    let operation = session.set_remote_description(alice, answer).unwrap();
    finish(&mut session, alice, operation);
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(false));
    let init = SframeEncrypterInit::new(
        SframeMode::PerPacket,
        SframeCipherSuite::Aes128CtrHmacSha256_80,
    );
    for error in [
        session.create_sframe_encrypter(sender, &init).unwrap_err(),
        session
            .create_sframe_decrypter(receiver, init.cipher_suite())
            .unwrap_err(),
    ] {
        match error {
            SframeCreateError::Native(error) => {
                assert_eq!(error.kind(), RtcErrorType::InvalidModification);
                assert_eq!(
                    error.message(),
                    "Cannot enable Sframe after it has been disabled by a completed negotiation."
                );
            }
            error => panic!("expected native invalid modification, got {error:?}"),
        }
    }
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(false));
}

#[test]
fn foreign_ids_cannot_import_local_ownership_and_controlled_acquisition_is_exclusive() {
    let mut first = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let peer = first.create_peer(PeerConfiguration::default()).unwrap();
    let mut second = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    assert_eq!(
        second.peer_signaling_state(peer).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        second.peer_connection_state(peer).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        second.peer_ice_gathering_state(peer).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        second.create_offer(peer, false).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    drop(second);
    drop(first);
    let world = ControlledWorld::acquire(11, Duration::from_secs(1)).unwrap();
    assert_eq!(
        ProductionSession::new(ProductionSessionConfig::default())
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidState
    );
    drop(world);
}
