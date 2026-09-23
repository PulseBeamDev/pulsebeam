#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the live authenticated standards-peer boundary fails at the first missing fact"
)]
#![allow(
    clippy::disallowed_types,
    reason = "test-only connection inputs require the public Arc and Bytes API"
)]
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command as ProcessCommand, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use base64::Engine;
use bytes::Bytes;
use pulsebeam_rtc::{
    Command, Connection, ConnectionConfig, ConnectionEntropy, Event, ForwardedMedia, FrameBoundary,
    FrameDependencies, FrameId, FrameMetadata, GlobalMediaTime, LocalCandidate,
    MediaPayloadBitrate, NetworkInput, Output, PacketFeedbackKind, PlayoutDelay, SdpOffer,
    TimePoint, TransmitTarget,
};
use tokio::net::UdpSocket;

fn point(start: Instant) -> TimePoint {
    let now = Instant::now();
    TimePoint {
        monotonic: now,
        global: GlobalMediaTime::from_micros(1_000_000_u64.saturating_add(
            u64::try_from(now.duration_since(start).as_micros()).unwrap_or(u64::MAX),
        )),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the repository-provisioned Pion CCFB peer"]
async fn authenticated_pion_ccfb_reaches_production_feedback() {
    let binary = std::env::var("PULSEBEAM_RFC8888_PEER").expect("provisioned Pion peer path");
    let mut peer = ProcessCommand::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start Pion peer");
    let mut offer_line = String::new();
    BufReader::new(peer.stdout.take().expect("offer pipe"))
        .read_line(&mut offer_line)
        .expect("read Pion offer");
    let offer = base64::engine::general_purpose::STANDARD
        .decode(offer_line.trim())
        .expect("base64 offer");
    let offer = String::from_utf8(offer).expect("UTF-8 SDP");
    assert!(offer.contains("a=rtcp-fb:111 ccfb"));
    assert!(!offer.contains("transport-cc"));
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("server UDP");
    let local = socket.local_addr().expect("server address");
    let start = Instant::now();
    let mut config = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(local)],
        ..ConnectionConfig::default()
    };
    config.default_audio_policy.desired_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
    config.default_audio_policy.playout_delay = PlayoutDelay::from_ticks(0, 50).expect("500 ms");
    let accepted = Connection::accept(
        config,
        SdpOffer::new(offer),
        point(start),
        ConnectionEntropy::new([0x88; 32]),
    )
    .expect("negotiate CCFB");
    let sender = accepted
        .session
        .senders
        .first()
        .expect("outbound audio sender")
        .id;
    assert_eq!(accepted.session.feedback, Some(PacketFeedbackKind::Rfc8888));
    let mut connection = accepted.connection;
    let answer = base64::engine::general_purpose::STANDARD.encode(accepted.answer.as_str());
    writeln!(peer.stdin.as_mut().expect("answer pipe"), "{answer}").expect("deliver SDP answer");
    let mut buffer = [0_u8; 2048];
    let deadline = start + Duration::from_secs(12);
    let mut media_received = 0_u64;
    let mut emitted_first = None;
    let mut acknowledged_before_expiry = false;
    while Instant::now() < deadline {
        loop {
            match connection.poll(point(start)) {
                Output::Transmit(transmit) => {
                    if let TransmitTarget::Udp { remote, .. } = transmit.target {
                        socket
                            .send_to(&transmit.payload, remote)
                            .await
                            .expect("send UDP");
                    }
                }
                Output::Event(Event::Media { packet, .. }) => {
                    media_received += 1;
                    match connection.command(
                        point(start),
                        Command::SendMedia {
                            sender,
                            media: ForwardedMedia {
                                packet,
                                frame: FrameMetadata {
                                    id: FrameId::from_value(media_received),
                                    boundary: FrameBoundary::Complete,
                                    random_access: true,
                                    discardable: false,
                                    dependencies: FrameDependencies::Known(Arc::from([])),
                                },
                            },
                        },
                    ) {
                        Ok(()) | Err(pulsebeam_rtc::CommandError::WouldBlock) => {}
                        Err(error) => panic!("forward Pion media: {error:?}"),
                    }
                }
                Output::Event(_) => {}
                Output::Closed(reason) => panic!("unexpected close: {reason:?}"),
                _ => break,
            }
        }
        let stats = connection.stats().connection;
        if stats.transmitted_rtp_bytes > 0 {
            emitted_first.get_or_insert_with(Instant::now);
        }
        if emitted_first.is_some_and(|at| at.elapsed() < Duration::from_secs(2))
            && stats.transmitted_rtp_bytes > 1_000
            && stats.rtp_bytes_in_flight.saturating_mul(2) < stats.transmitted_rtp_bytes
        {
            acknowledged_before_expiry = true;
            break;
        }
        if let Ok(Ok((size, remote))) =
            tokio::time::timeout(Duration::from_millis(5), socket.recv_from(&mut buffer)).await
        {
            connection
                .receive(
                    point(start),
                    NetworkInput::Udp {
                        local,
                        remote,
                        ecn: None,
                        payload: Bytes::copy_from_slice(&buffer[..size]),
                    },
                )
                .expect("authenticated UDP input");
        }
    }
    let stats = connection.stats().connection;
    let _ = peer.kill();
    let _ = peer.wait();
    assert_eq!(stats.feedback, Some(PacketFeedbackKind::Rfc8888));
    assert!(
        media_received > 2,
        "Pion source did not reach production ingress"
    );
    assert!(
        stats.transmitted_rtp_bytes > 1_000,
        "production egress did not reach Pion"
    );
    assert!(
        acknowledged_before_expiry,
        "authenticated CCFB did not acknowledge emitted RTP before history expiry: {stats:?}"
    );
    assert_eq!(stats.unknown_feedback, 0, "CCFB must match committed RTP");
}
