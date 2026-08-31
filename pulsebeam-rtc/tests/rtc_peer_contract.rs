#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    reason = "contract fixtures fail immediately when a required protocol invariant is absent"
)]

use std::{
    net::SocketAddr,
    time::{Duration, Instant, SystemTime},
};

use pulsebeam_rtc::{
    DataPayload, DatagramProtocol, ExtendedMediaSequence, ExtendedRtpTimestamp, IngressDatagram,
    MediaRewrite, NegotiatedMedia, RtcEvent, RtcPeer,
};
use str0m_upstream::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive},
};

const CLIENT_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 10_000);
const SERVER_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 10_001);
const PROTOCOL_BUDGET: usize = 10_000;

struct ContractHarness {
    epoch: Instant,
    now: Instant,
    client_deadline: Instant,
    client: Rtc,
    peer: RtcPeer,
    media: Box<[NegotiatedMedia]>,
    upstream_mid: Option<Mid>,
    downstream_mid: Mid,
    events: Vec<RtcEvent>,
    client_events: Vec<Event>,
    peer_transmits: usize,
    client_transmits: usize,
}

impl ContractHarness {
    fn new() -> Self {
        Self::with_upstream(true)
    }

    fn viewer_only() -> Self {
        Self::with_upstream(false)
    }

    fn with_upstream(include_upstream: bool) -> Self {
        let epoch = Instant::now();
        let mut builder = Rtc::builder().clear_codecs();
        builder.codec_config().enable_opus(true);
        builder.codec_config().enable_h264(true);
        let mut client = builder.build(epoch);
        let candidate = Candidate::builder()
            .udp()
            .host(CLIENT_ADDRESS)
            .build()
            .expect("client candidate");
        assert!(client.add_local_candidate(candidate).is_some());
        let mut changes = client.sdp_api();
        let upstream_mid = include_upstream.then(|| {
            changes.add_media(
                MediaKind::Video,
                Direction::SendOnly,
                Some("source".to_owned()),
                Some("camera".to_owned()),
                None,
            )
        });
        let downstream_mid = changes.add_media(
            MediaKind::Video,
            Direction::RecvOnly,
            Some("viewer".to_owned()),
            Some("slot".to_owned()),
            None,
        );
        let _ = changes.add_channel("contract".to_owned());
        let (offer, pending) = changes.apply().expect("initial offer");
        let (peer, negotiation) = RtcPeer::accept(
            epoch,
            1,
            &offer.to_sdp_string(),
            "serverufrag".to_owned(),
            "server-contract-password".to_owned(),
            Box::new([format!(
                "candidate:1 1 udp 2130706431 {} {} typ host",
                SERVER_ADDRESS.ip(),
                SERVER_ADDRESS.port()
            )]),
        )
        .expect("facade accepts offer");
        let answer = SdpAnswer::from_sdp_string(negotiation.answer()).expect("answer parses");
        client
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("client accepts answer");
        let media = negotiation.media().to_vec().into_boxed_slice();
        let mut harness = Self {
            epoch,
            now: epoch,
            client_deadline: epoch,
            client,
            peer,
            media,
            upstream_mid,
            downstream_mid,
            events: Vec::new(),
            client_events: Vec::new(),
            peer_transmits: 0,
            client_transmits: 0,
        };
        harness.drain_client_outputs();
        harness
    }

    fn connect(&mut self) {
        self.run_until(|harness| {
            harness.events.iter().any(|event| {
                matches!(
                    event,
                    RtcEvent::ConnectionStateChanged(pulsebeam_rtc::RtcConnectionState::Connected)
                )
            }) && harness
                .client_events
                .iter()
                .any(|event| matches!(event, Event::Connected))
        });
    }

    fn run_until(&mut self, complete: impl Fn(&Self) -> bool) {
        for _ in 0..PROTOCOL_BUDGET {
            if complete(self) {
                return;
            }
            self.step();
        }
        panic!(
            "RTC contract exceeded its deterministic protocol budget at {:?}: peer_tx={} client_tx={} events={:?}",
            self.now.saturating_duration_since(self.epoch),
            self.peer_transmits,
            self.client_transmits,
            self.events
        );
    }

    fn step(&mut self) {
        if self.now >= self.client_deadline {
            self.client
                .handle_input(Input::Timeout(self.now))
                .expect("client timeout");
            self.drain_client_outputs();
        }
        if self
            .peer
            .next_deadline()
            .is_some_and(|deadline| self.now >= deadline)
        {
            self.peer.handle_timeout(self.now).expect("peer timeout");
        }
        while let Some(transmit) = self.peer.poll_transmit(self.now) {
            self.peer_transmits = self.peer_transmits.saturating_add(1);
            let protocol = match transmit.protocol() {
                DatagramProtocol::Udp => Protocol::Udp,
                DatagramProtocol::Tcp => Protocol::Tcp,
            };
            let contents = transmit
                .bytes()
                .try_into()
                .expect("contract datagram fits str0m input");
            self.client
                .handle_input(Input::Receive(
                    self.now,
                    Receive {
                        proto: protocol,
                        source: transmit.source(),
                        destination: transmit.destination(),
                        contents,
                    },
                ))
                .expect("client ingress");
            self.peer
                .confirm_departure(transmit.receipt(), self.now)
                .expect("departure confirmation");
            self.drain_client_outputs();
        }
        while let Some(event) = self.peer.poll_event() {
            self.events.push(event);
        }
        let peer_deadline = self.peer.next_deadline();
        let next = peer_deadline.map_or(self.client_deadline, |deadline| {
            deadline.min(self.client_deadline)
        });
        self.now = if next > self.now {
            next
        } else {
            self.now
                .checked_add(Duration::from_millis(1))
                .expect("contract time remains representable")
        };
    }

    fn drain_client_outputs(&mut self) {
        loop {
            match self.client.poll_output().expect("client output") {
                Output::Transmit(transmit) => {
                    self.client_transmits = self.client_transmits.saturating_add(1);
                    let protocol = match transmit.proto {
                        Protocol::Udp => DatagramProtocol::Udp,
                        Protocol::Tcp => DatagramProtocol::Tcp,
                        _ => continue,
                    };
                    self.peer
                        .handle_datagram(
                            self.now,
                            IngressDatagram::new(
                                protocol,
                                transmit.source,
                                transmit.destination,
                                transmit.contents.to_vec(),
                                SystemTime::UNIX_EPOCH,
                            ),
                        )
                        .expect("peer ingress");
                }
                Output::Event(event) => self.client_events.push(event),
                Output::Timeout(deadline) => {
                    self.client_deadline = deadline;
                    break;
                }
            }
        }
    }

    fn send_h264_frame(&mut self) {
        let parameters = self
            .client
            .codec_config()
            .find(|parameters| parameters.spec().codec == str0m_upstream::format::Codec::H264)
            .cloned()
            .expect("H.264 parameters");
        self.client
            .writer(self.upstream_mid.expect("upstream media negotiated"))
            .expect("upstream writer")
            .write(
                parameters.pt(),
                self.now,
                Duration::from_millis(33).into(),
                vec![0, 0, 0, 1, 0x65, 0x80, 1, 2, 3],
            )
            .expect("H.264 frame");
    }

    fn request_downstream_keyframe(&mut self) {
        self.client
            .direct_api()
            .stream_rx_by_mid(self.downstream_mid, None)
            .expect("downstream stream")
            .request_keyframe(str0m_upstream::media::KeyframeRequestKind::Pli);
        self.drain_client_outputs();
    }
}

#[test]
fn facade_negotiates_connects_authenticates_media_and_closes() {
    let mut harness = ContractHarness::new();
    harness.connect();
    harness.send_h264_frame();
    harness.run_until(|harness| {
        harness
            .events
            .iter()
            .any(|event| matches!(event, RtcEvent::Media(_)))
    });

    let media = harness.events.iter().find_map(|event| match event {
        RtcEvent::Media(packet) => Some(packet),
        _ => None,
    });
    let media = media.expect("authenticated media event");
    let descriptor = harness
        .media
        .iter()
        .find(|negotiated| negotiated.ingress() == Some(media.stream()))
        .and_then(NegotiatedMedia::descriptor)
        .expect("negotiated descriptor for authenticated media");
    assert_eq!(descriptor.kind(), pulsebeam_rtc::MediaKind::Video);
    assert_eq!(descriptor.codec().name(), "H264");
    assert_eq!(media.playout_time(), SystemTime::UNIX_EPOCH);
    assert!(!media.payload().is_empty());
    assert!(
        media
            .semantics(descriptor)
            .expect("H.264 semantics")
            .keyframe()
    );

    let position = harness
        .events
        .iter()
        .position(|event| matches!(event, RtcEvent::Media(_)))
        .expect("media position");
    let RtcEvent::Media(packet) = harness.events.swap_remove(position) else {
        unreachable!("selected media event")
    };
    let slot = harness
        .media
        .iter()
        .find_map(NegotiatedMedia::egress)
        .expect("egress slot");
    harness
        .peer
        .forward(
            harness.now,
            slot,
            &packet,
            MediaRewrite {
                sequence: ExtendedMediaSequence::new(65_535),
                timestamp: ExtendedRtpTimestamp::new(u64::from(u32::MAX)),
                marker: true,
                dependency: None,
            },
        )
        .expect("forward admission");
    harness.run_until(|harness| {
        harness
            .client_events
            .iter()
            .any(|event| matches!(event, Event::MediaData(_)))
    });

    harness.request_downstream_keyframe();
    harness.run_until(|harness| {
        harness.events.iter().any(
            |event| matches!(event, RtcEvent::KeyframeRequested(requested) if *requested == slot),
        )
    });

    harness.run_until(|harness| {
        harness
            .events
            .iter()
            .any(|event| matches!(event, RtcEvent::DataChannelOpened { .. }))
    });
    let channel = harness.events.iter().find_map(|event| match event {
        RtcEvent::DataChannelOpened { channel, .. } => Some(*channel),
        _ => None,
    });
    let channel = channel.expect("opened data channel");
    harness
        .peer
        .send_data(
            harness.now,
            channel,
            DataPayload::Text("contract".to_owned()),
        )
        .expect("data send");
    harness.run_until(|harness| {
        harness
            .client_events
            .iter()
            .any(|event| matches!(event, Event::ChannelData(_)))
    });

    harness.peer.close(harness.now);
    assert!(harness.peer.next_deadline().is_none());
    assert!(matches!(
        harness.peer.poll_event(),
        Some(RtcEvent::ConnectionStateChanged(
            pulsebeam_rtc::RtcConnectionState::Draining
        ))
    ));
}

#[test]
fn negotiation_handles_remain_stable_and_facade_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<RtcPeer>();

    let first = ContractHarness::new();
    let second = ContractHarness::new();
    let facts = |harness: &ContractHarness| {
        harness.peer.state();
        let mut ingress = Vec::new();
        let mut egress = Vec::new();
        for event in &harness.events {
            assert!(!matches!(event, RtcEvent::Media(_)));
        }
        for media in &harness.media {
            ingress.extend(media.ingress());
            egress.extend(media.egress());
        }
        (ingress, egress)
    };
    assert_eq!(facts(&first), facts(&second));
}

#[test]
fn malformed_input_is_rejected_without_destroying_the_peer() {
    let mut harness = ContractHarness::new();
    let malformed = IngressDatagram::new(
        DatagramProtocol::Udp,
        CLIENT_ADDRESS,
        SERVER_ADDRESS,
        vec![0xff],
        SystemTime::UNIX_EPOCH,
    );
    assert!(
        harness
            .peer
            .handle_datagram(harness.epoch, malformed)
            .is_err()
    );
    assert_eq!(
        harness.peer.state(),
        pulsebeam_rtc::RtcConnectionState::Negotiated
    );
    harness.connect();
}

#[test]
fn desired_idle_path_proves_capacity_through_the_internal_probe_stream() {
    let mut harness = ContractHarness::new();
    harness.connect();
    harness
        .peer
        .set_desired_bitrate(harness.now, 2_000_000)
        .expect("desired bitrate");
    harness
        .peer
        .set_current_bitrate(harness.now, 0)
        .expect("idle allocator bitrate");
    harness.run_until(|harness| {
        harness.events.iter().any(|event| {
            matches!(event, RtcEvent::BweCapacity(capacity) if capacity.bitrate_bps() >= 1_000_000)
        })
    });
}

#[test]
fn desired_idle_path_set_before_connect_proves_capacity_after_connect() {
    let mut harness = ContractHarness::viewer_only();
    harness
        .peer
        .set_desired_bitrate(harness.now, 2_000_000)
        .expect("desired bitrate");
    harness
        .peer
        .set_current_bitrate(harness.now, 0)
        .expect("idle allocator bitrate");
    harness.connect();
    harness.run_until(|harness| {
        harness.events.iter().any(|event| {
            matches!(event, RtcEvent::BweCapacity(capacity) if capacity.bitrate_bps() >= 1_000_000)
        })
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclusive_peer_ownership_survives_work_stealing_tasks() {
    let harness = ContractHarness::new();
    let now = harness.now;
    let mut peer = harness.peer;
    for _ in 0..16 {
        peer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            assert_eq!(peer.state(), pulsebeam_rtc::RtcConnectionState::Negotiated);
            peer
        })
        .await
        .expect("owning task completes");
    }
    peer.close(now);
    assert_eq!(peer.state(), pulsebeam_rtc::RtcConnectionState::Closed);
}
