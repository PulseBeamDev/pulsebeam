use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use pulsebeam_rtc::{
    Connection, ConnectionConfig, ConnectionEntropy, Event, LocalCandidate, MediaPayloadBitrate,
    NetworkInput, Output, RtpWrite, SdpOffer, TransmitTarget,
};
use pulsebeam_webrtc_sys::*;

fn complete(
    world: &ControlledWorld,
    peer: &PeerConnection,
    operation: OperationId,
) -> Option<SessionDescription> {
    for _ in 0..2_000 {
        world.pump(512);
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(completion) = event {
                assert_eq!(completion.operation_id, operation);
                return completion.result.expect("native signaling operation");
            }
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    panic!("signaling operation stalled: {operation:?}");
}

fn is_twcc(payload: &[u8]) -> bool {
    // DTLS 1.3 ciphertext can match the PT/FMT bytes, but not RTP version 2.
    payload
        .first()
        .is_some_and(|byte| byte >> 6 == 2 && byte & 31 == 15)
        && payload.get(1) == Some(&205)
}

#[test]
fn feedback_classifier_excludes_dtls13_records() {
    assert!(is_twcc(&[0x8f, 205, 0, 5, 0, 0, 0, 0]));
    assert!(is_twcc(&[0xaf, 205, 0, 5, 0, 0, 0, 0]));
    assert!(!is_twcc(&[0x2f, 205, 0, 5, 0, 0, 0, 0]));
    assert!(!is_twcc(&[0x3f, 205, 0, 5, 0, 0, 0, 0]));
    assert!(!is_twcc(&[0x8f, 201, 0, 5, 0, 0, 0, 0]));
    assert!(!is_twcc(&[0x80, 205, 0, 5, 0, 0, 0, 0]));
    assert!(!is_twcc(&[]));
    assert!(!is_twcc(&[0x8f]));
}

#[allow(
    clippy::disallowed_types,
    reason = "single-threaded test retains immutable public API payloads and dependencies without cross-shard sharing"
)]
fn exercise(seed: u64, start: Instant) -> (u64, u64, usize) {
    use bytes::Bytes;

    let world = ControlledWorld::acquire(seed, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let client_endpoint = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        .unwrap();
    let server_endpoint = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)))
        .unwrap();
    let server_socket = server_endpoint.bind_udp(41000).unwrap();
    let video = EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8")).unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(client_endpoint.network_manager().unwrap())
        .packet_socket_factory(client_endpoint.packet_socket_factory().unwrap())
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
        .video_encoder_factory(video.encoder_factory())
        .video_decoder_factory(VideoDecoderFactoryHandle::builtin_vp8().unwrap())
        .build()
        .unwrap();
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let source = factory.create_encoded_audio_source(1).unwrap();
    let track = factory
        .create_encoded_audio_track("audio", &source)
        .unwrap();
    let transceiver = peer
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendReceive)
        .unwrap();
    let mono = peer
        .audio_sender_capabilities()
        .unwrap()
        .into_iter()
        .find(|codec| {
            codec.name().eq_ignore_ascii_case("opus")
                && codec
                    .parameters()
                    .iter()
                    .any(|p| p.key == "stereo" && p.value == "0")
        })
        .expect("mono Opus capability");
    transceiver.set_audio_codec_preferences(&[mono]).unwrap();
    let channel = peer
        .create_data_channel("coexistence", DataChannelConfiguration::default())
        .unwrap();
    let offer = complete(&world, &peer, peer.create_offer().unwrap()).unwrap();
    complete(&world, &peer, peer.set_local_description(offer).unwrap());
    let mut gathered = false;
    for _ in 0..2_000 {
        world.pump(512);
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete) =
                event
            {
                gathered = true;
            }
        }
        if gathered {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(gathered, "native ICE gathering stalled");
    let offer = peer.descriptions().unwrap().pending_local.unwrap();
    let origin = world.now();
    let point = |now: Duration| start + (now - origin);
    let local = SocketAddr::new(
        server_socket.local_address().ip(),
        server_socket.local_address().port(),
    );
    let config = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(local)],
        ..ConnectionConfig::default()
    };
    let accepted = Connection::accept(
        config,
        SdpOffer::new(offer.sdp),
        point(world.now()),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([11; 32]),
    )
    .expect("native offer accepted");
    let sender = accepted.session.senders[0].mid.to_string();
    assert_eq!(
        accepted.session.feedback,
        Some(pulsebeam_rtc::PacketFeedbackKind::TransportWide)
    );
    complete(
        &world,
        &peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: accepted.answer.as_str().to_owned(),
        })
        .unwrap(),
    );
    let mut connection = accepted.connection;

    let mut connected = false;
    let mut peer_connected = false;
    let mut inbound = 0;
    let mut outbound = 0;
    let mut pending = std::collections::VecDeque::new();
    let sink = transceiver.receiver().attach_encoded_audio_sink().unwrap();
    let mut feedback = 0;
    let mut accounted_feedback = false;
    let mut sent_data = false;
    let mut server_data = Vec::new();
    let mut client_data = Vec::new();
    const FRAME_TICKS: u32 = 20;
    const MEDIA_FRAMES: u32 = 10;
    let mut media_start_tick = None;
    for tick in 0..5_000 {
        world.pump(512);
        let mut received_twcc = false;
        while let Some(packet) = network.next_packet() {
            assert_eq!(packet.kind, OutboundKind::Udp);
            if packet.destination == server_socket.local_address() && is_twcc(&packet.payload) {
                feedback += 1;
                received_twcc = true;
            }
            match network.deliver(packet.id) {
                Ok(()) => {}
                // BUNDLE closes the native non-primary candidate socket after answering.
                Err(NetworkError::DestinationUnavailable)
                    if packet.source == server_socket.local_address()
                        && packet.payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]) => {}
                Err(error) => panic!("virtual delivery {packet:?}: {error:?}"),
            }
        }
        while let Some(packet) = server_socket.try_receive() {
            connection
                .receive(
                    point(world.now()),
                    point(world.now()),
                    NetworkInput::Udp {
                        local,
                        remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                        ecn: None,
                        payload: Bytes::from(packet.payload),
                    },
                )
                .unwrap();
        }
        let before = connection.stats().connection;
        for _ in 0..512 {
            match connection.poll(point(world.now())) {
                Output::Transmit(packet) => {
                    let TransmitTarget::Udp { remote, .. } = packet.target else {
                        panic!("unexpected TCP transport")
                    };
                    server_socket
                        .send_to(
                            NetworkAddress::new(remote.ip(), remote.port()).unwrap(),
                            packet.payload.to_vec(),
                        )
                        .unwrap();
                }
                Output::Event(Event::Connected) => connected = true,
                Output::Event(Event::Rtp { packet, .. }) => {
                    assert!(packet.payload.ends_with(&[0xf8, 0xff, 0xfe]));
                    inbound += 1;
                    pending.push_back(RtpWrite {
                        sequence: inbound,
                        timestamp: packet.header.timestamp,
                        wallclock: packet.arrival,
                        marker: packet.header.marker,
                        payload: packet.payload,
                        extensions: Vec::new(),
                    });
                }
                Output::Event(Event::DataChannel(pulsebeam_rtc::DataChannelEvent::Message {
                    channel,
                    message,
                })) => {
                    server_data.push(message.clone());
                    connection
                        .send_data(point(world.now()), channel, message)
                        .unwrap();
                }
                Output::Event(_) => {}
                Output::Idle { .. } => break,
                Output::Closed(reason) => panic!("server closed: {reason:?}"),
                _ => panic!("unknown output"),
            }
        }
        let after = connection.stats().connection;
        let committed = after.transmitted_rtp_bytes + after.transmitted_padding_bytes
            - before.transmitted_rtp_bytes
            - before.transmitted_padding_bytes;
        if received_twcc && before.rtp_bytes_in_flight + committed > after.rtp_bytes_in_flight {
            accounted_feedback = true;
        }
        while let Some(media) = pending.pop_front() {
            match connection.write_rtp(point(world.now()), &sender, media) {
                Ok(())
                | Err(pulsebeam_rtc::RejectedRtp {
                    reason: pulsebeam_rtc::WriteError::WouldBlock,
                    ..
                }) => {}
                Err(error) => panic!("media command: {error:?}"),
            }
        }
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                    peer_connected = true
                }
                PeerConnectionEvent::Track(_) => {}
                PeerConnectionEvent::IceCandidateError { message, .. } => {
                    panic!("ICE error: {message}")
                }
                PeerConnectionEvent::OperationComplete(result) => {
                    panic!("unexpected operation: {result:?}")
                }
                _ => {}
            }
        }
        while let Some(frame) = sink.try_next_frame() {
            assert_eq!(frame.data, [0xf8, 0xff, 0xfe]);
            outbound += 1;
        }
        while let Some(event) = channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                client_data.push(message);
            }
        }
        if channel.state() == DataChannelState::Open && !sent_data {
            assert_eq!(
                channel.send(DataChannelMessage::text("controlled text")),
                DataChannelSendResult::Sent
            );
            assert_eq!(
                channel.send(DataChannelMessage::binary(vec![0, 255, 17])),
                DataChannelSendResult::Sent
            );
            sent_data = true;
        }
        if connected && peer_connected && media_start_tick.is_none() {
            media_start_tick = Some(tick + FRAME_TICKS);
        }
        if let Some(start_tick) = media_start_tick
            && (start_tick..start_tick + MEDIA_FRAMES * FRAME_TICKS).contains(&tick)
            && (tick - start_tick).is_multiple_of(FRAME_TICKS)
        {
            assert!(
                connected && peer_connected,
                "scheduled media requires connection: server={connected} peer={peer_connected} stats={:?}",
                connection.stats()
            );
            source
                .push_opus_at(
                    &OpusInputFrame {
                        data: vec![0xf8, 0xff, 0xfe],
                        rtp_timestamp: tick * 48,
                        samples_per_channel: 960,
                    },
                    world.now(),
                )
                .unwrap();
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    eprintln!(
        "connected={connected}/{peer_connected} media_start_tick={media_start_tick:?} media={inbound}/{outbound} feedback={feedback}, stats={:?}",
        connection.stats()
    );
    assert!(connected && peer_connected);
    assert!(
        inbound >= 10 && outbound >= 10,
        "authenticated bidirectional Opus"
    );
    assert!(feedback > 0, "native client generated TWCC");
    assert!(
        accounted_feedback,
        "authenticated native TWCC reduced committed RTP bytes in flight before history expiry"
    );
    assert_eq!(
        server_data,
        vec![
            pulsebeam_rtc::DataMessage::Text(Bytes::from_static(b"controlled text")),
            pulsebeam_rtc::DataMessage::Binary(Bytes::from_static(&[0, 255, 17]))
        ]
    );
    assert_eq!(
        client_data,
        vec![
            DataChannelMessage::text("controlled text"),
            DataChannelMessage::binary(vec![0, 255, 17])
        ]
    );
    assert!(
        connection.stats().connection.rtp_bytes_in_flight
            < connection.stats().connection.transmitted_rtp_bytes
    );
    (inbound, outbound, feedback)
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "single-threaded native boundary test constructs immutable public API payloads without cross-shard sharing"
)]
fn controlled_libwebrtc_acknowledges_rtx_padding_before_primary_media() {
    use bytes::Bytes;

    let start = Instant::now();
    let world = ControlledWorld::acquire(8324, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let client = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        .unwrap();
    let server = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)))
        .unwrap();
    let socket = server.bind_udp(41000).unwrap();
    let video = EncodedVideoInput::new().unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(client.network_manager().unwrap())
        .packet_socket_factory(client.packet_socket_factory().unwrap())
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
        .video_encoder_factory(video.encoder_factory())
        .video_decoder_factory(video.encoded_receive_factory().unwrap())
        .build()
        .unwrap();
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let source = video.create_source(&factory).unwrap();
    let track = source.create_track(&factory, "video").unwrap();
    let transceiver = peer
        .add_video_transceiver(&track, RtpTransceiverDirection::SendReceive)
        .unwrap();
    let sink = transceiver.receiver().attach_encoded_sink().unwrap();
    let offer = complete(&world, &peer, peer.create_offer().unwrap()).unwrap();
    complete(&world, &peer, peer.set_local_description(offer).unwrap());
    let mut gathered = false;
    for _ in 0..2_000 {
        world.pump(512);
        while let Some(event) = peer.try_next_event() {
            if matches!(
                event,
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
            ) {
                gathered = true;
            }
        }
        if gathered {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(gathered, "native video ICE gathering stalled");
    let offer = peer.descriptions().unwrap().pending_local.unwrap();
    let origin = world.now();
    let point = |now: Duration| start + (now - origin);
    let local = SocketAddr::new(socket.local_address().ip(), socket.local_address().port());
    let config = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(local)],
        ..ConnectionConfig::default()
    };
    let accepted = Connection::accept(
        config,
        SdpOffer::new(offer.sdp),
        point(world.now()),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([19; 32]),
    )
    .unwrap();
    assert_eq!(accepted.session.senders.len(), 1);

    assert!(
        accepted.session.senders[0]
            .codec
            .retransmission_payload_type
            .is_some()
    );
    assert_eq!(
        accepted.session.feedback,
        Some(pulsebeam_rtc::PacketFeedbackKind::TransportWide)
    );
    let rtx_payload_type = accepted
        .answer
        .as_str()
        .lines()
        .find_map(|line| {
            let mapping = line.strip_prefix("a=rtpmap:")?;
            let (payload_type, codec) = mapping.split_once(' ')?;
            codec
                .eq_ignore_ascii_case("rtx/90000")
                .then(|| payload_type.parse::<u8>().unwrap())
        })
        .expect("negotiated RTX payload type");
    complete(
        &world,
        &peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: accepted.answer.as_str().to_owned(),
        })
        .unwrap(),
    );
    let mut connection = accepted.connection;
    connection.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(1_000_000)));
    let mut connected = false;
    let mut peer_connected = false;
    let mut probes = 0;
    let mut feedback = 0;
    let mut accounted_feedback = false;
    for _ in 0..2_000 {
        world.pump(512);
        let mut received_twcc = false;
        while let Some(packet) = network.next_packet() {
            assert_eq!(packet.kind, OutboundKind::Udp);
            if packet.destination == socket.local_address() && is_twcc(&packet.payload) {
                feedback += 1;
                received_twcc = true;
            }
            match network.deliver(packet.id) {
                Ok(()) => {}
                Err(NetworkError::DestinationUnavailable)
                    if packet.source == socket.local_address()
                        && packet.payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]) => {}
                Err(error) => panic!("virtual video delivery {packet:?}: {error:?}"),
            }
        }
        while let Some(packet) = socket.try_receive() {
            connection
                .receive(
                    point(world.now()),
                    point(world.now()),
                    NetworkInput::Udp {
                        local,
                        remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                        ecn: None,
                        payload: Bytes::from(packet.payload),
                    },
                )
                .unwrap();
        }
        let before = connection.stats().connection;
        for _ in 0..512 {
            match connection.poll(point(world.now())) {
                Output::Transmit(packet) => {
                    if packet.payload.first().is_some_and(|byte| byte >> 6 == 2)
                        && packet
                            .payload
                            .get(1)
                            .is_some_and(|byte| byte & 0x7f == rtx_payload_type)
                    {
                        assert_ne!(packet.payload[0] & 0x20, 0, "RTX probe is RTP padding");
                        let ssrc = u32::from_be_bytes(packet.payload[8..12].try_into().unwrap());
                        assert_ne!(ssrc, 0);
                        probes += 1;
                    }
                    let TransmitTarget::Udp { remote, .. } = packet.target else {
                        panic!("unexpected video TCP transport")
                    };
                    socket
                        .send_to(
                            NetworkAddress::new(remote.ip(), remote.port()).unwrap(),
                            packet.payload.to_vec(),
                        )
                        .unwrap();
                }
                Output::Event(Event::Connected) => connected = true,
                Output::Event(Event::Rtp { .. }) => {
                    panic!("padding must not deliver bogus media")
                }
                Output::Event(_) => {}
                Output::Idle { .. } => break,
                Output::Closed(reason) => panic!("video server closed: {reason:?}"),
                _ => panic!("unknown video output"),
            }
        }
        let after = connection.stats().connection;
        let committed = after.transmitted_padding_bytes - before.transmitted_padding_bytes;
        if received_twcc && before.rtp_bytes_in_flight + committed > after.rtp_bytes_in_flight {
            accounted_feedback = true;
        }
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                    peer_connected = true
                }
                PeerConnectionEvent::IceCandidateError { message, .. } => {
                    panic!("video ICE error: {message}")
                }
                PeerConnectionEvent::OperationComplete(result) => {
                    panic!("unexpected video operation: {result:?}")
                }
                _ => {}
            }
        }
        assert!(
            sink.try_next_frame().is_none(),
            "padding is not a native video frame"
        );
        world.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(connected && peer_connected);
    assert!(probes > 0, "wire-visible negotiated RTX padding");
    assert!(
        feedback > 0,
        "native video peer generated TWCC before primary media"
    );
    assert!(
        accounted_feedback,
        "authenticated TWCC retired RTX padding before history expiry"
    );
    assert_eq!(connection.stats().connection.transmitted_rtp_bytes, 0);
}

#[test]
fn controlled_libwebrtc_reaches_production_connection() {
    let start = Instant::now();
    assert_eq!(
        exercise(731, start),
        exercise(731, start),
        "same-seed controlled replay"
    );
}
