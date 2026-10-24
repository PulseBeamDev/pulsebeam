use std::{net::Ipv4Addr, thread, time::Duration};

#[path = "support/non_trickle.rs"]
mod non_trickle;

use pulsebeam_webrtc_sys::{
    AudioFrameError, AudioPcmFrame, Environment, ManualClock, OperationId, PeerConfiguration,
    PeerConnection, PeerConnectionEvent, PeerConnectionFactory, PeerErrorKind,
    RtpTransceiverDirection, SessionDescription, SimulatedNetwork,
};

fn finish(
    peer: &PeerConnection,
    id: OperationId,
    events: &mut Vec<PeerConnectionEvent>,
) -> Option<SessionDescription> {
    for _ in 0..1_000_000 {
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == id => {
                    return done.result.unwrap();
                }
                event => events.push(event),
            }
        }
        thread::yield_now();
    }
    panic!("audio operation did not complete");
}

#[test]
fn caller_fed_audio_track_rejects_foreign_sources_and_survives_source_owner_drop() {
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let endpoint_a = network
        .register_endpoint(Ipv4Addr::new(10, 6, 0, 1).into())
        .unwrap();
    let endpoint_b = network
        .register_endpoint(Ipv4Addr::new(10, 6, 0, 2).into())
        .unwrap();
    let build = |endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint| {
        PeerConnectionFactory::builder()
            .environment(environment.clone())
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap()
    };
    let factory = build(&endpoint_a);
    let other = build(&endpoint_b);
    let capabilities = factory.get_rtp_sender_capabilities(pulsebeam_webrtc_sys::MediaType::Audio);
    assert!(!capabilities.codecs().is_empty());
    assert!(
        capabilities
            .codecs()
            .iter()
            .all(|codec| codec.base_codec().kind() == pulsebeam_webrtc_sys::MediaType::Audio)
    );
    let receiver_capabilities =
        factory.get_rtp_receiver_capabilities(pulsebeam_webrtc_sys::MediaType::Audio);
    assert!(!receiver_capabilities.codecs().is_empty());
    for kind in [
        pulsebeam_webrtc_sys::MediaType::Data,
        pulsebeam_webrtc_sys::MediaType::Unsupported,
        pulsebeam_webrtc_sys::MediaType::Any,
        pulsebeam_webrtc_sys::MediaType::Other(i32::MIN),
    ] {
        assert_eq!(
            factory.get_rtp_sender_capabilities(kind),
            pulsebeam_webrtc_sys::RtpCapabilities::default()
        );
        assert_eq!(
            factory.get_rtp_receiver_capabilities(kind),
            pulsebeam_webrtc_sys::RtpCapabilities::default()
        );
    }
    let retained_capabilities =
        factory.get_rtp_sender_capabilities(pulsebeam_webrtc_sys::MediaType::Audio);
    let mut changed_capabilities = capabilities.clone();
    changed_capabilities.set_codecs(&[]);
    assert_eq!(retained_capabilities, capabilities);
    let mut source = factory.create_audio_source().unwrap();
    assert_eq!(
        other
            .create_audio_track("foreign", &source)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        factory.create_audio_track("", &source).err().unwrap().kind,
        PeerErrorKind::InvalidParameter
    );
    let frame = AudioPcmFrame::i16_interleaved(48_000, 1, 1_000_000, vec![0; 480]).unwrap();
    source.push_frame(&frame).unwrap();
    let mut malformed = frame.clone();
    malformed.samples.pop();
    assert_eq!(
        source.push_frame(&malformed),
        Err(AudioFrameError::InvalidSampleCount)
    );
    let track = factory.create_audio_track("mic", &source).unwrap();
    assert_eq!(track.id(), "mic");
    assert!(track.enabled());
    track.set_enabled(false);
    assert!(!track.enabled());
    track.set_enabled(true);

    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    assert_eq!(
        other
            .create_peer_connection(PeerConfiguration::default())
            .unwrap()
            .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    let transceiver = peer
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    assert!(!transceiver.sender().id().is_empty());
    let sender = transceiver.sender();
    assert!(sender.same_resource(&transceiver.sender()));
    assert!(
        transceiver
            .receiver()
            .same_resource(&transceiver.receiver())
    );
    assert!(transceiver.same_resource(&peer.audio_transceivers().unwrap()[0]));
    let observer = sender.packet_observer();
    let alias = transceiver.sender().packet_observer();
    assert!(observer.same_resource(&alias));
    assert_eq!(
        observer.register_observer(0),
        Err(pulsebeam_webrtc_sys::RtpObserverError::ZeroCapacity)
    );
    observer.register_observer(2).unwrap();
    assert_eq!(
        alias.register_observer(1),
        Err(pulsebeam_webrtc_sys::RtpObserverError::AlreadyRegistered)
    );
    assert!(alias.take_event().is_none());
    alias.unregister_observer();
    observer.register_observer(1).unwrap();
    assert_eq!(observer.take_dropped_events(), 0);
    observer.unregister_observer();
    let receiver_observer = transceiver.receiver().packet_observer();
    assert!(receiver_observer.same_resource(&transceiver.receiver().packet_observer()));
    receiver_observer.register_observer(1).unwrap();
    receiver_observer.unregister_observer();
    assert_eq!(sender.media_type(), pulsebeam_webrtc_sys::MediaType::Audio);
    assert_eq!(sender.ssrc(), 0);
    let keyframe_error = sender.generate_keyframe(&[vec![0xff, 0]]).unwrap_err();
    let pulsebeam_webrtc_sys::RtpSenderOperationError::Native(keyframe_error) = keyframe_error
    else {
        panic!("actual audio SDK error required")
    };
    assert_eq!(
        keyframe_error.kind(),
        pulsebeam_webrtc_sys::RtcErrorType::UnsupportedOperation
    );
    assert!(!keyframe_error.message_bytes().is_empty());
    let mut initial = sender.init_send_encodings();
    assert_eq!(
        initial,
        vec![pulsebeam_webrtc_sys::RtpEncodingParameters::default()]
    );
    initial[0].set_min_bitrate_bps(Some(-7));
    assert_eq!(sender.init_send_encodings()[0].min_bitrate_bps(), None);
    let mut parameters = sender.get_parameters();
    assert!(!parameters.transaction_id_bytes().is_empty());
    assert_eq!(parameters.encodings().len(), 1);
    assert_eq!(parameters.encodings()[0].min_bitrate_bps(), None);
    let first_transaction = parameters.transaction_id_bytes();
    let next = sender.get_parameters();
    assert_ne!(first_transaction, next.transaction_id_bytes());
    assert_eq!(parameters, next); // SDK value equality excludes transaction ID.
    let snapshot_encodings = parameters.encodings();
    parameters.set_encodings(&[]);
    assert_eq!(snapshot_encodings.len(), 1);
    assert_eq!(next.encodings().len(), 1);
    assert!(sender.dtls_transport().is_none());
    assert!(peer.sctp_transport().is_none());
    sender.set_streams(&["first".into(), "".into(), "first".into(), "second".into()]);
    assert_eq!(sender.stream_ids(), vec!["first", "", "second"]);
    assert_eq!(
        sender.stream_id_bytes(),
        vec![b"first".to_vec(), vec![], b"second".to_vec()]
    );
    sender.set_streams(&[]);
    assert!(sender.stream_ids().is_empty());
    let dtmf = transceiver
        .sender()
        .dtmf_sender()
        .expect("audio native DTMF proxy");
    let same_dtmf = transceiver.sender().dtmf_sender().unwrap();
    assert!(dtmf.same_resource(&same_dtmf));
    assert_eq!(dtmf.tones(), "");
    assert_eq!(dtmf.duration_ms(), 100);
    assert_eq!(dtmf.inter_tone_gap_ms(), 50);
    assert_eq!(dtmf.comma_delay_ms(), 2000);
    assert!(!dtmf.can_insert_tones());
    let capacity = std::num::NonZeroUsize::new(4).unwrap();
    assert!(dtmf.register_observer(capacity));
    assert!(!same_dtmf.register_observer(capacity));
    assert!(same_dtmf.take_tone_change().is_none());
    assert_eq!(same_dtmf.take_dropped_tone_changes(), 0);
    assert_eq!(dtmf.insert_tones("1", 39, i32::MAX), Ok(false));
    assert_eq!(dtmf.insert_tones("1", 40, i32::MAX - 40), Ok(false));
    assert_eq!(
        dtmf.insert_tones("1", 40, i32::MAX - 39),
        Err(pulsebeam_webrtc_sys::DtmfInsertError::DelayOverflow)
    );
    // No non-comma tone consumes duration + gap in these inputs.
    assert_eq!(dtmf.insert_tones("x,", 40, i32::MAX), Ok(false));
    assert_eq!(
        dtmf.insert_tones_with_comma_delay("1", 40, i32::MAX, 29),
        Ok(false)
    );
    same_dtmf.unregister_observer();
    assert!(dtmf.register_observer(capacity));
    dtmf.unregister_observer();
    drop(dtmf);
    drop(same_dtmf);
    source.push_frame(&frame).unwrap();
    source.close().unwrap();
    assert_eq!(source.push_frame(&frame), Err(AudioFrameError::Released));
    drop(source);
    peer.remove_track(&transceiver.sender()).unwrap();
    peer.close().unwrap();
    drop(track);
    drop(peer);
    drop(factory);
}

#[test]
fn decoded_audio_sink_receives_headless_pcm_over_negotiated_peer() {
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let alice_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 7, 0, 1).into())
        .unwrap();
    let bob_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 7, 0, 2).into())
        .unwrap();
    let factory = |endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint| {
        PeerConnectionFactory::builder()
            .environment(environment.clone())
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap()
    };
    let alice_factory = factory(&alice_endpoint);
    let bob_factory = factory(&bob_endpoint);
    let mut alice = alice_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut bob = bob_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let source = alice_factory.create_audio_source().unwrap();
    let track = alice_factory.create_audio_track("voice", &source).unwrap();
    let sender = alice
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap()
        .sender();
    sender.set_streams(&["voice-stream".into()]);
    assert_eq!(sender.stream_ids(), vec!["voice-stream"]);
    assert_eq!(alice.audio_transceivers().unwrap().len(), 1);
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = finish(&alice, alice.create_offer().unwrap(), &mut alice_events).unwrap();
    assert!(offer.sdp.contains("m=audio"));
    finish(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let gathered =
        non_trickle::gathered_local_description(&alice, &clock, &network, &mut alice_events);
    finish(
        &bob,
        bob.set_remote_description(gathered).unwrap(),
        &mut bob_events,
    );
    let answer = finish(&bob, bob.create_answer().unwrap(), &mut bob_events).unwrap();
    finish(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let gathered = non_trickle::gathered_local_description(&bob, &clock, &network, &mut bob_events);
    finish(
        &alice,
        alice.set_remote_description(gathered).unwrap(),
        &mut alice_events,
    );
    let receivers = bob.audio_receivers().unwrap();
    assert_eq!(receivers.len(), 1);
    assert_eq!(
        receivers[0].media_type(),
        pulsebeam_webrtc_sys::MediaType::Audio
    );
    assert_eq!(receivers[0].stream_ids(), vec!["voice-stream"]);
    let remote_track = receivers[0].audio_track().unwrap();
    let remote_source = remote_track.source().unwrap();
    let remote_alias = receivers[0].audio_track().unwrap().source().unwrap();
    assert!(remote_source.is_remote());
    assert!(remote_source.same_resource(&remote_alias));
    let volume_events = remote_source
        .observe_volume(std::num::NonZeroUsize::new(2).unwrap())
        .unwrap();
    remote_source.set_volume(0.25).unwrap();
    assert_eq!(remote_alias.volume(), 0.25);
    assert_eq!(
        volume_events.try_next_event(),
        Some(pulsebeam_webrtc_sys::AudioObserverEvent::Volume(0.25))
    );
    remote_alias.set_volume(1.0).unwrap();
    volume_events.close();
    assert_eq!(
        volume_events.try_next_event(),
        Some(pulsebeam_webrtc_sys::AudioObserverEvent::Volume(1.0))
    );
    assert_eq!(volume_events.try_next_event(), None);
    assert_eq!(
        bob.add_audio_transceiver(&remote_track, RtpTransceiverDirection::SendOnly)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    let bob_sender = bob.audio_transceivers().unwrap().remove(0).sender();
    assert!(
        bob_sender
            .set_audio_track(Some(&remote_track))
            .unwrap_err()
            .message
            .contains("dependency cycle")
    );
    let receiver_snapshot = receivers[0].get_parameters();
    assert!(!receiver_snapshot.codecs().is_empty());
    assert!(!receivers[0].set_parameters(&receiver_snapshot));
    let mut unsupported = receiver_snapshot.clone();
    unsupported.set_transaction_id(b"receiver-tx\xff\0");
    unsupported.set_degradation_preference(Some(
        pulsebeam_webrtc_sys::DegradationPreference::Other(i32::MIN),
    ));
    let mut encodings = unsupported.encodings();
    for encoding in &mut encodings {
        encoding.set_bitrate_priority(f64::NAN);
    }
    unsupported.set_encodings(&encodings);
    assert!(!receivers[0].set_parameters(&unsupported));
    assert_eq!(receivers[0].get_parameters(), receiver_snapshot);
    for delay in [
        Some(0.0),
        Some(-1.0),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(f64::MAX),
        None,
    ] {
        receivers[0].set_jitter_buffer_minimum_delay(delay).unwrap();
    }
    assert_eq!(
        receivers[0].set_jitter_buffer_minimum_delay(Some(f64::NAN)),
        Err(pulsebeam_webrtc_sys::RtpReceiverOperationError::UnsafeDelayNaN)
    );
    // Rejected NaN must not poison the receiver's cached delay or later media.
    assert_ne!(sender.ssrc(), 0);
    let mut sink = receivers[0].attach_audio_sink().unwrap();
    let samples = (0..480)
        .map(|index| if index % 48 < 24 { 12000 } else { -12000 })
        .collect();
    let frame = AudioPcmFrame::i16_interleaved(48_000, 1, 1_000_000, samples).unwrap();
    let mut received = None;
    let mut receiver_sources = Vec::new();
    let mut alice_connected = false;
    let mut bob_connected = false;
    for tick in 0..100_000 {
        if tick % 10 == 0 {
            source.push_frame(&frame).unwrap();
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while let Some(event) = alice.try_next_event() {
            alice_connected |= matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(
                    pulsebeam_webrtc_sys::ConnectionState::Connected
                )
            );
        }
        while let Some(event) = bob.try_next_event() {
            bob_connected |= matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(
                    pulsebeam_webrtc_sys::ConnectionState::Connected
                )
            );
        }
        if let Some(frame) = sink.try_next_frame() {
            if frame
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 2000)
            {
                received = Some(frame);
            }
        }
        // A playout pull may produce concealment before the first RTP packet.
        // Wait for an actual native source observation, not merely any PCM frame.
        receiver_sources = receivers[0].get_sources();
        if received.is_some()
            && receiver_sources.iter().any(|source| {
                source.source_type() == pulsebeam_webrtc_sys::RtpSourceType::Ssrc
                    && source.source_id() == sender.ssrc()
            })
        {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    let received = received.unwrap_or_else(|| {
        let snapshot = |peer: &PeerConnection| {
            let operation = peer.request_stats().unwrap();
            for _ in 0..1_000_000 {
                if let Some(PeerConnectionEvent::Stats(stats)) = peer.try_next_event()
                    && stats.operation_id == operation
                {
                    return Some(stats);
                }
                thread::yield_now();
            }
            None
        };
        panic!(
            "headless audio missing: alice_connected={alice_connected}, bob_connected={bob_connected}, dropped={}, alice={:?}, bob={:?}",
            sink.dropped_frames(), snapshot(&alice), snapshot(&bob)
        )
    });
    assert!(receiver_sources.iter().any(|source| source.source_type()
        == pulsebeam_webrtc_sys::RtpSourceType::Ssrc
        && source.source_id() == sender.ssrc()));
    assert_eq!(received.channels, 1);
    assert_eq!(received.sample_rate_hz, 48_000);
    assert_eq!(
        received.samples.len(),
        received.samples_per_channel as usize
    );
    let dtls = sender.dtls_transport().unwrap();
    let alias = alice.audio_transceivers().unwrap()[0]
        .sender()
        .dtls_transport()
        .unwrap();
    assert!(dtls.same_resource(&alias));
    assert_eq!(
        dtls.information().state(),
        pulsebeam_webrtc_sys::DtlsTransportState::Connected
    );
    assert_eq!(
        receivers[0].dtls_transport().unwrap().information().state(),
        pulsebeam_webrtc_sys::DtlsTransportState::Connected
    );
    let dtmf = alice.audio_transceivers().unwrap()[0]
        .sender()
        .dtmf_sender()
        .unwrap();
    assert!(dtmf.can_insert_tones());
    assert!(dtmf.register_observer(std::num::NonZeroUsize::new(16).unwrap()));
    assert_eq!(
        dtmf.insert_tones_with_comma_delay("a,x#", 40, 30, 30),
        Ok(true)
    );
    assert_eq!(dtmf.duration_ms(), 40);
    assert_eq!(dtmf.inter_tone_gap_ms(), 30);
    assert_eq!(dtmf.comma_delay_ms(), 30);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut tones = Vec::new();
    while tones.len() < 8 && std::time::Instant::now() < deadline {
        while let Some(event) = dtmf.take_tone_change() {
            tones.push((event.tone, event.remaining_tones));
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        tones,
        vec![
            ("a".into(), Some(",x#".into())),
            ("a".into(), None),
            (",".into(), Some("x#".into())),
            (",".into(), None),
            ("#".into(), Some("".into())),
            ("#".into(), None),
            ("".into(), Some("".into())),
            ("".into(), None),
        ]
    );
    assert_eq!(dtmf.tones(), "");
    assert_eq!(dtmf.take_dropped_tone_changes(), 0);
    dtmf.unregister_observer();
    assert!(dtmf.register_observer(std::num::NonZeroUsize::new(1).unwrap()));
    assert_eq!(dtmf.insert_tones("", 40, 30), Ok(true));
    assert_eq!(dtmf.comma_delay_ms(), 2000);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut dropped = 0;
    while dropped == 0 && std::time::Instant::now() < deadline {
        dropped += dtmf.take_dropped_tone_changes();
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(dropped, 1);
    assert_eq!(
        dtmf.take_tone_change().unwrap(),
        pulsebeam_webrtc_sys::DtmfToneChange {
            tone: "".into(),
            remaining_tones: None,
        }
    );
    assert_eq!(dtmf.take_dropped_tone_changes(), 0);
    dtmf.unregister_observer();
    drop(dtmf);
    sink.close().unwrap();
    sink.close().unwrap();
    assert!(sink.try_next_frame().is_none());
    alice.close().unwrap();
    bob.close().unwrap();
    assert!(!receiver_snapshot.codecs().is_empty());
    assert!(!receiver_sources.is_empty());
}
