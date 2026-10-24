use std::{net::Ipv4Addr, thread, time::Duration};

#[path = "support/non_trickle.rs"]
mod non_trickle;

use pulsebeam_webrtc_sys::{
    AudioDecoderFactory, AudioEncoderFactory, AudioPcmFrame, ConnectionState, Environment,
    ManualClock, OperationId, OpusAudioLevel, OpusInputError, OpusInputFrame, PeerConfiguration,
    PeerConnection, PeerConnectionEvent, PeerConnectionFactory, PeerErrorKind,
    RtpHeaderExtensionDirection, RtpTransceiverDirection, SessionDescription, SimulatedNetwork,
};

fn finish(peer: &PeerConnection, id: OperationId) -> Option<SessionDescription> {
    for _ in 0..1_000_000 {
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(done) = event
                && done.operation_id == id
            {
                return done.result.unwrap();
            }
        }
        thread::yield_now();
    }
    panic!("encoded audio operation did not complete");
}

#[test]
fn encoded_opus_receiver_gets_packets_without_decoded_sink() {
    for include_level in [false, true] {
        received_audio_extension_profile(include_level);
    }
}

fn received_audio_extension_profile(include_level: bool) {
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let alice_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 8, 0, 1).into())
        .unwrap();
    let bob_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 8, 0, 2).into())
        .unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let factory = |endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint| {
        PeerConnectionFactory::builder()
            .environment(environment.clone())
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .audio_decoder_factory(decoder.clone())
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
    let transceiver = alice
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    // The observed real decoder deliberately supports only Opus; do not offer
    // the builtin encoder's other codecs to an Opus-only receive factory.
    let opus = alice
        .audio_sender_capabilities()
        .unwrap()
        .into_iter()
        .find(|codec| codec.name().eq_ignore_ascii_case("opus"))
        .expect("native Opus encoder capability");
    transceiver.set_audio_codec_preferences(&[opus]).unwrap();
    if !include_level {
        let mut extensions = transceiver.header_extensions_to_negotiate().unwrap();
        let level = extensions
            .iter_mut()
            .find(|extension| extension.uri() == "urn:ietf:params:rtp-hdrext:ssrc-audio-level")
            .expect("native audio-level extension capability");
        level.direction = RtpHeaderExtensionDirection::Stopped;
        transceiver
            .set_header_extensions_to_negotiate(&extensions)
            .unwrap();
    }

    let offer = finish(&alice, alice.create_offer().unwrap()).unwrap();
    finish(&alice, alice.set_local_description(offer).unwrap());
    let gathered =
        non_trickle::gathered_local_description(&alice, &clock, &network, &mut Vec::new());
    finish(&bob, bob.set_remote_description(gathered).unwrap());
    let answer = finish(&bob, bob.create_answer().unwrap()).unwrap();
    finish(&bob, bob.set_local_description(answer).unwrap());
    let gathered = non_trickle::gathered_local_description(&bob, &clock, &network, &mut Vec::new());
    finish(&alice, alice.set_remote_description(gathered).unwrap());

    let receivers = bob.audio_receivers().unwrap();
    let sent_observer = transceiver.sender().packet_observer();
    let received_observer = receivers[0].packet_observer();
    sent_observer.register_observer(8).unwrap();
    received_observer.register_observer(8).unwrap();
    let mut sink = receivers[0].attach_encoded_audio_sink().unwrap();
    assert_eq!(
        receivers[0].attach_audio_sink().err().unwrap().kind,
        PeerErrorKind::InvalidState
    );
    assert_eq!(
        receivers[0].attach_encoded_audio_sink().err().unwrap().kind,
        PeerErrorKind::InvalidState
    );
    let frame = AudioPcmFrame::i16_interleaved(48_000, 1, 1_000_000, vec![1000; 480]).unwrap();
    let mut received = None;
    let mut first_sent = false;
    let mut first_received = false;
    for tick in 0..100_000 {
        if tick % 10 == 0 {
            source.push_frame(&frame).unwrap();
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while alice.try_next_event().is_some() {}
        while bob.try_next_event().is_some() {}
        if received.is_none() {
            received = sink.try_next_frame();
        }
        while let Some(event) = sent_observer.take_event() {
            first_sent |= event
                == pulsebeam_webrtc_sys::RtpPacketEvent::FirstPacketSent(
                    pulsebeam_webrtc_sys::MediaType::Audio,
                );
        }
        while let Some(event) = received_observer.take_event() {
            first_received |= event
                == pulsebeam_webrtc_sys::RtpPacketEvent::FirstPacketReceived(
                    pulsebeam_webrtc_sys::MediaType::Audio,
                );
        }
        if received.is_some() && first_sent && first_received {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert!(
        first_sent && first_received,
        "actual SDK first-media notifications missing"
    );
    assert_eq!(sent_observer.take_dropped_events(), 0);
    assert_eq!(received_observer.take_dropped_events(), 0);
    // Retire through one alias and register through another. SDK stock proxies
    // synchronously replay actual first-packet history on late registration.
    sent_observer.unregister_observer();
    received_observer.unregister_observer();
    let sender_alias = transceiver.sender().packet_observer();
    let receiver_alias = receivers[0].packet_observer();
    assert!(sender_alias.same_resource(&sent_observer));
    assert!(receiver_alias.same_resource(&received_observer));
    sender_alias.register_observer(1).unwrap();
    receiver_alias.register_observer(1).unwrap();
    assert_eq!(
        sender_alias.take_event(),
        Some(pulsebeam_webrtc_sys::RtpPacketEvent::FirstPacketSent(
            pulsebeam_webrtc_sys::MediaType::Audio
        ))
    );
    assert_eq!(
        receiver_alias.take_event(),
        Some(pulsebeam_webrtc_sys::RtpPacketEvent::FirstPacketReceived(
            pulsebeam_webrtc_sys::MediaType::Audio
        ))
    );
    sender_alias.unregister_observer();
    receiver_alias.unregister_observer();
    let received = received
        .unwrap_or_else(|| panic!("missing Opus packets, dropped={}", sink.dropped_frames()));
    assert!(!received.data.is_empty());
    assert!(received.samples_per_channel > 0);
    assert!(received.samples_per_channel <= 5760);
    assert_ne!(received.ssrc, 0);
    assert!(received.sequence_number.is_some());
    if include_level {
        assert!(received.audio_level_dbov.is_some_and(|level| level <= 127));
        assert_eq!(received.voice_activity, Some(true));
    } else {
        // Native Type is CN when the extension is absent. Do not report a
        // false V bit from that default or infer activity from the PCM.
        assert_eq!(received.audio_level_dbov, None);
        assert_eq!(received.voice_activity, None);
    }
    assert_eq!(decoder.decoder_statistics().unwrap().decode_calls, 0);
    assert_eq!(decoder.decoder_statistics().unwrap().input_packets, 0);
    sink.close().unwrap();
    sink.close().unwrap();
    assert!(sink.try_next_frame().is_none());
    let mut decoded = receivers[0].attach_audio_sink().unwrap();
    // The closed encoded handle must not release the new sink's reservation.
    drop(sink);
    assert_eq!(
        receivers[0].attach_encoded_audio_sink().err().unwrap().kind,
        PeerErrorKind::InvalidState
    );
    let mut got_decoded = false;
    for tick in 0..100_000 {
        if tick % 10 == 0 {
            source.push_frame(&frame).unwrap();
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while alice.try_next_event().is_some() {}
        while bob.try_next_event().is_some() {}
        got_decoded |= decoded.try_next_frame().is_some();
        if got_decoded && decoder.decoder_statistics().unwrap().decode_calls > 0 {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert!(got_decoded);
    assert!(
        decoder.decoder_statistics().unwrap().decode_calls > 0,
        "encoded close must resume real native decoding, not just PLC output"
    );
    decoded.close().unwrap();
    decoded.close().unwrap();
    assert!(decoded.try_next_frame().is_none());
    let mut replacement_decoded = receivers[0].attach_audio_sink().unwrap();
    drop(decoded);
    assert_eq!(
        receivers[0].attach_audio_sink().err().unwrap().kind,
        PeerErrorKind::InvalidState
    );
    replacement_decoded.close().unwrap();
    let replacement_encoded = receivers[0].attach_encoded_audio_sink().unwrap();
    drop(replacement_decoded);
    assert_eq!(
        receivers[0].attach_audio_sink().err().unwrap().kind,
        PeerErrorKind::InvalidState
    );
    let before = decoder.decoder_statistics().unwrap().input_packets;
    let mut got_encoded = false;
    for tick in 0..100_000 {
        if tick % 10 == 0 {
            source.push_frame(&frame).unwrap();
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while alice.try_next_event().is_some() {}
        while bob.try_next_event().is_some() {}
        if replacement_encoded.try_next_frame().is_some() {
            got_encoded = true;
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert!(
        got_encoded,
        "encoded interception must resume after mode switching"
    );
    assert_eq!(decoder.decoder_statistics().unwrap().input_packets, before);
    alice.close().unwrap();
    bob.close().unwrap();
    assert!(replacement_encoded.try_next_frame().is_none());
}

#[test]
fn mono_and_stereo_opus_sources_preserve_distinct_payloads_without_encoding() {
    for receiver_stereo in [true, false] {
        run_mono_and_stereo_opus_sources(receiver_stereo);
    }
}

fn run_mono_and_stereo_opus_sources(receiver_stereo: bool) {
    #[cfg(feature = "native")]
    assert_eq!(
        PeerConnectionFactory::builder()
            .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
            .native_audio(true)
            .build()
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let a = network
        .register_endpoint(Ipv4Addr::new(10, 9, 0, 1).into())
        .unwrap();
    let b = network
        .register_endpoint(Ipv4Addr::new(10, 9, 0, 2).into())
        .unwrap();
    let encoder = AudioEncoderFactory::with_opus_frames().unwrap();
    let alice_factory = PeerConnectionFactory::builder()
        .environment(environment.clone())
        .network_manager(a.network_manager().unwrap())
        .packet_socket_factory(a.packet_socket_factory().unwrap())
        .audio_encoder_factory(encoder.clone())
        .build()
        .unwrap();
    let bob_factory = PeerConnectionFactory::builder()
        .environment(environment)
        .network_manager(b.network_manager().unwrap())
        .packet_socket_factory(b.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    assert_eq!(
        bob_factory
            .create_encoded_audio_source(1)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::UnsupportedOperation
    );
    let mut alice = alice_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut bob = bob_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    assert_eq!(
        alice_factory.create_audio_source().err().unwrap().kind,
        PeerErrorKind::UnsupportedOperation
    );
    let first = alice_factory.create_encoded_audio_source(1).unwrap();
    let second = alice_factory.create_encoded_audio_source(2).unwrap();
    // This threaded fixture has a custom ManualClock but no process clock hook.
    // Do not silently claim that its epoch maps to native TimeMillis capture.
    assert_eq!(
        first.capture_time_now(),
        Err(OpusInputError::InvalidTimestamp)
    );
    assert_eq!(
        first.push_opus_at(
            &OpusInputFrame {
                data: vec![0xf8, 0xff, 0xfe],
                rtp_timestamp: 0,
                samples_per_channel: 960,
            },
            clock.now()
        ),
        Err(OpusInputError::InvalidTimestamp)
    );
    assert_eq!((first.channels(), second.channels()), (1, 2));
    assert_eq!(
        alice_factory
            .create_encoded_audio_source(3)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    for (data, duration, expected) in [
        (
            vec![0xfc, 0x01],
            960,
            pulsebeam_webrtc_sys::OpusInputError::InvalidPacket,
        ),
        (
            vec![0xf8, 0x01],
            480,
            pulsebeam_webrtc_sys::OpusInputError::InvalidDuration,
        ),
        (
            vec![0xf8; 1201],
            960,
            pulsebeam_webrtc_sys::OpusInputError::InvalidPacket,
        ),
    ] {
        assert_eq!(
            first.push_opus(&OpusInputFrame {
                data,
                rtp_timestamp: 0,
                samples_per_channel: duration,
            }),
            Err(expected)
        );
    }
    assert_eq!(
        first.push_opus(&OpusInputFrame {
            data: vec![0xf8, 0xff, 0xfe],
            rtp_timestamp: 0,
            samples_per_channel: 960,
        }),
        Err(pulsebeam_webrtc_sys::OpusInputError::Backpressure)
    );
    let track_a = alice_factory
        .create_encoded_audio_track("opus-a", &first)
        .unwrap();
    let track_b = alice_factory
        .create_encoded_audio_track("opus-b", &second)
        .unwrap();
    let capabilities = alice.audio_sender_capabilities().unwrap();
    // Mono/stereo share one RFC 7587 RTP codec identity. Stereo is a
    // receiver preference, not a second codec/payload type advertisement.
    let opus: Vec<_> = capabilities
        .iter()
        .filter(|codec| codec.name().eq_ignore_ascii_case("opus"))
        .cloned()
        .collect();
    assert_eq!(opus.len(), 1, "duplicate Opus RTP advertisements");
    let opus = &opus[0];
    alice
        .add_audio_transceiver(&track_a, RtpTransceiverDirection::SendOnly)
        .unwrap()
        .set_audio_codec_preferences(&[opus.clone()])
        .unwrap();
    alice
        .add_audio_transceiver(&track_b, RtpTransceiverDirection::SendOnly)
        .unwrap()
        .set_audio_codec_preferences(&[opus.clone()])
        .unwrap();

    let offer = finish(&alice, alice.create_offer().unwrap()).unwrap();
    finish(&alice, alice.set_local_description(offer).unwrap());
    let gathered =
        non_trickle::gathered_local_description(&alice, &clock, &network, &mut Vec::new());
    finish(&bob, bob.set_remote_description(gathered).unwrap());
    let mut answer = finish(&bob, bob.create_answer().unwrap()).unwrap();
    // Exercise both the explicit stereo preference and the native mono
    // default. The genuine stereo payload must survive either negotiated
    // encoder input layout without passing opaque carrier bytes through a
    // PCM channel remix.
    if receiver_stereo {
        // Bundled m-lines sharing one payload type must share its fmtp.
        answer.sdp = answer
            .sdp
            .replace("useinbandfec=1", "useinbandfec=1;stereo=1");
    } else {
        assert!(!answer.sdp.contains("stereo=1"));
    }
    finish(&bob, bob.set_local_description(answer).unwrap());
    let gathered = non_trickle::gathered_local_description(&bob, &clock, &network, &mut Vec::new());
    finish(&alice, alice.set_remote_description(gathered).unwrap());

    let receivers = bob.audio_receivers().unwrap();
    assert_eq!(receivers.len(), 2);
    let sinks: Vec<_> = receivers
        .iter()
        .map(|r| r.attach_encoded_audio_sink().unwrap())
        .collect();
    let mut connected = [false; 2];
    for _ in 0..100_000 {
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        for (index, peer) in [&alice, &bob].into_iter().enumerate() {
            while let Some(event) = peer.try_next_event() {
                if let PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) =
                    event
                {
                    connected[index] = true;
                }
            }
        }
        if connected == [true; 2] {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert_eq!(
        connected, [true; 2],
        "peers must connect before pushing Opus"
    );
    let packets = [vec![0xf8, 0xff, 0xfe], vec![0xfc, 0x12, 0x34, 0x56]];
    let levels = [
        OpusAudioLevel::new(127, true).unwrap(),
        OpusAudioLevel::new(0, false).unwrap(),
    ];
    let mut seen = [false; 2];
    let mut sink_stream = [None; 2];
    let mut last_push_timestamp = [0; 2];
    let mut pending: [Option<OpusInputFrame>; 2] = [None, None];
    for tick in 0..100_000u32 {
        for (index, source) in [&first, &second].into_iter().enumerate() {
            if tick % 20 == 0 && pending[index].is_none() {
                pending[index] = Some(OpusInputFrame {
                    data: packets[index].clone(),
                    rtp_timestamp: tick * 48,
                    samples_per_channel: 960,
                });
            }
            if let Some(frame) = &pending[index] {
                match source.push_opus_with_audio_level(frame, levels[index]) {
                    Ok(()) => {
                        last_push_timestamp[index] = frame.rtp_timestamp;
                        pending[index] = None;
                    }
                    // A threaded encoder may lag virtual capture time. Retry
                    // the same unadmitted packet after transport/clock progress,
                    // without duplicating successful sends or expanding credits.
                    Err(OpusInputError::Backpressure) => {}
                    Err(error) => panic!(
                        "Opus push failed at tick={tick} channels={} error={error:?}",
                        source.channels()
                    ),
                }
            }
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while alice.try_next_event().is_some() {}
        while bob.try_next_event().is_some() {}
        for (sink_index, sink) in sinks.iter().enumerate() {
            while let Some(frame) = sink.try_next_frame() {
                let stream = packets
                    .iter()
                    .position(|payload| frame.data == *payload)
                    .expect("foreign or re-encoded payload");
                if let Some((previous, ssrc)) = sink_stream[sink_index] {
                    assert_eq!((stream, frame.ssrc), (previous, ssrc));
                } else {
                    sink_stream[sink_index] = Some((stream, frame.ssrc));
                }
                seen[stream] = true;
                assert_eq!(frame.samples_per_channel, 960);
                assert_eq!(frame.audio_level_dbov, Some(levels[stream].level_dbov()));
                assert_eq!(frame.voice_activity, Some(levels[stream].voice_activity()));
            }
        }
        if seen == [true; 2] {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert_eq!(
        seen, [true; 2],
        "both sources must arrive byte-identically (receiver_stereo={receiver_stereo})"
    );
    assert_ne!(sink_stream[0], sink_stream[1], "tracks must stay isolated");

    let variants = [
        (vec![0xf0, 0x11], 480),
        (vec![0xff, 3, 0x11, 0x12, 0x13], 2880),
    ];
    for (index, source) in [&first, &second].into_iter().enumerate() {
        let (data, samples_per_channel) = &variants[index];
        let frame = OpusInputFrame {
            data: data.clone(),
            rtp_timestamp: last_push_timestamp[index] + 960,
            samples_per_channel: *samples_per_channel,
        };
        let mut admitted = false;
        for _ in 0..100_000 {
            match source.push_opus_with_audio_level(&frame, levels[index]) {
                Ok(()) => {
                    admitted = true;
                    break;
                }
                Err(OpusInputError::Backpressure) => {}
                Err(error) => panic!("Opus variant admission failed: {error:?}"),
            }
            while let Some(packet) = network.next_packet() {
                network.deliver(packet.id).unwrap();
            }
            while alice.try_next_event().is_some() {}
            while bob.try_next_event().is_some() {}
            clock.advance(Duration::from_millis(1)).unwrap();
            thread::yield_now();
        }
        assert!(admitted, "Opus variant never admitted");
    }
    let mut variant_seen = [false; 2];
    for _ in 0..20_000 {
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        while alice.try_next_event().is_some() {}
        while bob.try_next_event().is_some() {}
        for (sink_index, sink) in sinks.iter().enumerate() {
            while let Some(frame) = sink.try_next_frame() {
                if frame.data == variants[0].0 {
                    assert_eq!(frame.samples_per_channel, 480);
                    assert_eq!(sink_stream[sink_index].unwrap().0, 0);
                    assert_eq!(frame.audio_level_dbov, Some(127));
                    assert_eq!(frame.voice_activity, Some(true));
                    variant_seen[0] = true;
                } else if frame.data == variants[1].0 {
                    assert_eq!(frame.samples_per_channel, 2880);
                    assert_eq!(sink_stream[sink_index].unwrap().0, 1);
                    assert_eq!(frame.audio_level_dbov, Some(0));
                    assert_eq!(frame.voice_activity, Some(false));
                    variant_seen[1] = true;
                }
            }
        }
        if variant_seen == [true; 2] {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert_eq!(
        variant_seen, [true; 2],
        "10 ms and 60 ms Opus must arrive intact"
    );
    assert_eq!(encoder.opus_frame_handoff_failures(), 0);
    alice.close().unwrap();
    bob.close().unwrap();
}
