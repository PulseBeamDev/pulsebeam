use std::{
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
    time::SystemTime,
};

use pulsebeam_core::fabric::media_source::{Anchors, Limits};
use pulsebeam_rtc::{AuthenticatedRtp, LocalCandidate, RtpReceiveCodec};
use str0m::{
    media::MediaTime,
    net::{Protocol, Receive},
};

use super::*;
use crate::{
    participant::{
        batcher::{AppendStatus, OwnedPacketQueue},
        event::test_utils::MockParticipantSink,
    },
    rtp::fabric_ingress::ExtensionForm,
};

struct PeerEgress<'a> {
    peer: &'a mut str0m::Rtc,
    local: SocketAddr,
    now: std::time::Instant,
}

impl NetworkEgress for PeerEgress<'_> {
    fn append_udp(&mut self, packets: &mut OwnedPacketQueue) -> AppendStatus {
        while let Some((remote, bytes)) = packets.pop_packet() {
            let received = Receive::new(Protocol::Udp, self.local, remote, &bytes).unwrap();
            self.peer
                .handle_input(str0m::Input::Receive(self.now, received))
                .unwrap();
        }
        AppendStatus::Drained
    }

    fn flush(&mut self) -> bool {
        true
    }
}

struct RtpLoss {
    primary: u32,
    repair: u32,
    drop_sequence: Option<u16>,
    repair_udp_bytes: Option<usize>,
}

impl RtpLoss {
    fn drop(&mut self, bytes: &[u8]) -> bool {
        let [first, pt, a, b, _, _, _, _, w, x, y, z, ..] = bytes else {
            return false;
        };
        if first & 0xe0 != 0x80 || (64..=95).contains(&(pt & 0x7f)) {
            return false;
        }
        let ssrc = u32::from_be_bytes([*w, *x, *y, *z]);
        if ssrc == self.repair {
            self.repair_udp_bytes = Some(bytes.len());
        }
        if ssrc == self.primary && self.drop_sequence == Some(u16::from_be_bytes([*a, *b])) {
            self.drop_sequence = None;
            return true;
        }
        false
    }
}

struct Pair {
    peer: str0m::Rtc,
    clock: net::ReceiveTime,
    local: SocketAddr,
    remote: SocketAddr,
    received: Vec<str0m::rtp::RtpPacket>,
    data_received: Vec<str0m::channel::ChannelData>,
    native_control: Option<str0m::channel::ChannelId>,
    refresh: Vec<str0m::media::KeyframeRequest>,
    output_form: Option<pulsebeam_rtc::RtpExtensionForm>,
    rtp_loss: Option<RtpLoss>,
    protocol_clock: net::ReceiveTime,
    #[cfg(feature = "transport-conformance")]
    udp: Option<ScopedTestUdp>,
}

#[cfg(feature = "transport-conformance")]
struct ScopedTestUdp {
    local: pulsebeam_udp::UdpSocket,
    remote: std::net::UdpSocket,
    primary: u32,
    repair: Option<u32>,
    received_primary: usize,
    peak: usize,
}

#[cfg(feature = "transport-conformance")]
impl ScopedTestUdp {
    fn deliver(
        &mut self,
        offered: &[pulsebeam_udp::SendDatagram<'_>],
        mut receive: impl FnMut(&[u8]),
    ) -> usize {
        let report = self.local.try_send_batch(offered);
        assert_eq!(
            report.accepted,
            offered.len(),
            "bounded socket rejected a scoped handoff: {report:?}"
        );
        let cap = usize::from(
            self.local
                .nonfragmenting_max_datagram_bytes()
                .unwrap()
                .get(),
        );
        let mut udp = vec![0; cap + 1];
        for datagram in offered {
            assert!(datagram.gso_segment_size.is_none());
            assert_eq!(datagram.destination, self.remote.local_addr().unwrap());
            let (len, source) = self
                .remote
                .recv_from(&mut udp)
                .expect("loopback handoff reached the selected socket");
            let received = udp.get(..len).expect("bounded datagram");
            assert_eq!(source, self.local.local_addr());
            assert_eq!(received, datagram.payload);
            assert!((1..=cap).contains(&len));
            self.peak = self.peak.max(len);
            if let Some((ssrc, _)) = fixture_rtp(received) {
                assert!(
                    ssrc == self.primary || self.repair == Some(ssrc),
                    "handoff used an SSRC outside the installed answer: {ssrc}"
                );
                if ssrc == self.primary {
                    self.received_primary += 1;
                }
            }
            receive(received);
        }
        report.accepted
    }
}

impl Pair {
    fn stage_receiver_handoff(&mut self, shard: &mut crate::shard::core::FabricRtpFixture) {
        for _ in 0..200 {
            self.step(shard.receiver(), None);
            if shard.receiver_handoff_staged() {
                shard.queue_receiver_datagrams();
                return;
            }
        }
        panic!("receiver did not stage a protected handoff");
    }

    fn step_receiver(&mut self, shard: &mut crate::shard::core::FabricRtpFixture) {
        let now = self.clock.monotonic;
        let (receiver, mut sink) = shard.receiver_with_sink();
        self.step_with_sink(receiver, None, &mut sink);
        shard.submit_receiver(|offered| {
            #[cfg(feature = "transport-conformance")]
            if let Some(udp) = self.udp.as_mut() {
                return udp.deliver(offered, |bytes| {
                    let received =
                        Receive::new(Protocol::Udp, self.local, self.remote, bytes).unwrap();
                    self.peer
                        .handle_input(str0m::Input::Receive(now.into(), received))
                        .unwrap();
                });
            }
            for datagram in offered {
                assert_eq!(datagram.destination, self.remote);
                assert!(datagram.gso_segment_size.is_none());
                let received = Receive::new(
                    Protocol::Udp,
                    self.local,
                    datagram.destination,
                    datagram.payload,
                )
                .unwrap();
                self.peer
                    .handle_input(str0m::Input::Receive(now.into(), received))
                    .unwrap();
            }
            offered.len()
        });
    }

    fn connect(&mut self, connection: &mut Connection) {
        // Connection construction samples its own clock after SDP/DTLS setup.
        self.clock.monotonic = Instant::now().max(self.clock.monotonic);
        for _ in 0..2000 {
            self.step(connection, None);
            if self.peer.is_connected() {
                return;
            }
        }
        panic!("test peer did not connect");
    }

    fn step(
        &mut self,
        connection: &mut Connection,
        observer: Option<&mut super::super::transport::AuthenticatedIngressObserver<'_>>,
    ) {
        self.step_with_sink(connection, observer, &mut MockParticipantSink::new());
    }

    fn step_with_sink(
        &mut self,
        connection: &mut Connection,
        mut observer: Option<&mut super::super::transport::AuthenticatedIngressObserver<'_>>,
        sink: &mut impl ParticipantSink,
    ) {
        let now = self.clock.monotonic;
        connection.advance_rtc_clock(now, now);
        self.peer
            .handle_input(str0m::Input::Timeout(now.into()))
            .unwrap();
        for _ in 0..64 {
            match self.peer.poll_output().unwrap() {
                str0m::Output::Transmit(packet) => {
                    assert_eq!(packet.destination, self.local);
                    assert_eq!(packet.source, self.remote);
                    if self
                        .rtp_loss
                        .as_mut()
                        .is_some_and(|loss| loss.drop(&packet.contents))
                    {
                        continue;
                    }
                    let len = packet.contents.len();
                    connection.transport.enqueue_ingress(
                        net::RecvPacketBatch {
                            src: self.remote,
                            dst: self.local,
                            len,
                            stride: len,
                            buf: packet.contents.to_vec(),
                            transport: net::Transport::Udp(net::UdpMode::Scalar),
                            received_at: self.clock,
                            offset: 0,
                        },
                        ShardId::new(0),
                    );
                }
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(str0m::Event::RtpPacket(packet)) => self.received.push(packet),
                str0m::Output::Event(str0m::Event::ChannelData(message)) => {
                    self.data_received.push(message);
                }
                str0m::Output::Event(str0m::Event::KeyframeRequest(request)) => {
                    self.refresh.push(request);
                }
                _ => {}
            }
        }
        for _ in 0..64 {
            let ingress = connection.transport.receive_pending(now);
            for _ in 0..64 {
                let output = if let Some(observer) = observer.as_deref_mut() {
                    connection.transport.poll_output(Some(observer))
                } else {
                    connection.poll_transport(now, sink)
                };
                match output {
                    TransportPollOutput::Event(event) => {
                        connection.handle_event(now, *event, sink);
                    }
                    TransportPollOutput::Transmit => {
                        connection.drain_network(&mut PeerEgress {
                            peer: &mut self.peer,
                            local: self.local,
                            now: now.into(),
                        });
                    }
                    TransportPollOutput::Timeout(_) => break,
                    TransportPollOutput::Pending => {}
                    TransportPollOutput::Closed(reason) => {
                        panic!("test endpoint closed: {reason:?}")
                    }
                }
            }
            if !connection.transport.has_pending_ingress()
                && matches!(ingress, IngressResult::Empty)
            {
                break;
            }
        }
        self.clock.monotonic = now.checked_add(Duration::from_millis(1)).unwrap();
    }
}

fn media_pair(
    kind: str0m::media::MediaKind,
    direction: str0m::media::Direction,
    rtp_mode: bool,
) -> (Pair, ConnectionConfig, Mid, str0m::media::Mid, u8) {
    let (pair, config, mid, peer_mid, pt, _) = endpoint_pair(kind, direction, rtp_mode, None);
    (pair, config, mid, peer_mid, pt)
}

type EndpointPair = (
    Pair,
    ConnectionConfig,
    Mid,
    str0m::media::Mid,
    u8,
    Option<str0m::channel::ChannelId>,
);

fn endpoint_pair(
    kind: str0m::media::MediaKind,
    direction: str0m::media::Direction,
    rtp_mode: bool,
    data_stream: Option<NonZeroU16>,
) -> EndpointPair {
    endpoint_pair_with_control(kind, direction, rtp_mode, data_stream, false)
}

fn endpoint_pair_with_control(
    kind: str0m::media::MediaKind,
    direction: str0m::media::Direction,
    rtp_mode: bool,
    data_stream: Option<NonZeroU16>,
    native: bool,
) -> EndpointPair {
    endpoint_pair_at(
        kind,
        direction,
        rtp_mode,
        data_stream,
        native,
        [
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:5000".parse().unwrap(),
        ],
    )
    .0
}

fn endpoint_pair_at(
    kind: str0m::media::MediaKind,
    direction: str0m::media::Direction,
    rtp_mode: bool,
    data_stream: Option<NonZeroU16>,
    native: bool,
    [local, remote]: [SocketAddr; 2],
) -> (EndpointPair, String) {
    let clock = net::ReceiveTime {
        monotonic: Instant::now(),
        wall: SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_millis(12345))
            .unwrap(),
    };
    let mut config = str0m::RtcConfig::new()
        .clear_codecs()
        .enable_opus(kind == str0m::media::MediaKind::Audio)
        .enable_h264(kind == str0m::media::MediaKind::Video)
        .set_rtp_mode(rtp_mode);
    if kind == str0m::media::MediaKind::Video && direction == str0m::media::Direction::RecvOnly {
        use pulsebeam_core::dd;
        config = config.set_extension(
            13,
            str0m::rtp::Extension::with_serializer(dd::URI, dd::Serializer),
        );
    }
    let mut peer = config.build(clock.monotonic.into());
    peer.add_local_candidate(
        str0m::Candidate::builder()
            .udp()
            .host(remote)
            .build()
            .unwrap(),
    );
    let mut changes = peer.sdp_api();
    let peer_mid = changes.add_media(kind, direction, None, None, None);
    let channel = data_stream.map(|stream| {
        changes.add_channel_with_config(str0m::channel::ChannelConfig {
            label: "fabric-test".into(),
            protocol: "pulsebeam.data.v1".into(),
            negotiated: Some(stream.get()),
            ordered: false,
            reliability: str0m::channel::Reliability::MaxRetransmits { retransmits: 0 },
        })
    });
    let signaling = native.then(crate::participant::signaling::native_signaling_config);
    let native_control = signaling.as_ref().map(|signaling| {
        changes.add_channel_with_config(str0m::channel::ChannelConfig {
            label: signaling.label.to_string(),
            protocol: signaling.protocol.to_string(),
            negotiated: Some(0),
            ordered: true,
            reliability: str0m::channel::Reliability::Reliable,
        })
    });
    let (offer, pending) = changes.apply().unwrap();
    let (mut accepted, answer, mut resources) =
        crate::control::Negotiator::new(vec![LocalCandidate::Udp(local)])
            .create_answer(
                offer.to_sdp_string(),
                crate::control::ufrag::test_credentials(),
                clock,
                None,
            )
            .unwrap();
    let (mid, codec) = match direction {
        str0m::media::Direction::SendOnly => {
            let inbound = accepted.session.inbound_media.first().unwrap();
            (inbound.mid.as_ref(), *inbound.codecs.first().unwrap())
        }
        _ => {
            let sender = accepted.session.senders.first().unwrap();
            (sender.mid.as_ref(), sender.codec)
        }
    };
    let mid = Mid::new(mid).unwrap();
    if let Some(signaling) = signaling {
        resources.signaling_channel = Some(
            crate::participant::signaling::SignalingChannel::compile(&signaling),
        );
        accepted
            .connection
            .open_data_channel(clock.monotonic.into(), signaling)
            .unwrap();
    }
    let output_form = accepted
        .connection
        .rtp_egress_extension_form(mid.as_str(), None);
    peer.sdp_api()
        .accept_answer(
            pending,
            str0m::change::SdpAnswer::from_sdp_string(&answer).unwrap(),
        )
        .unwrap();
    let participant_id =
        entity::ParticipantId::new(if direction == str0m::media::Direction::SendOnly {
            "publisher"
        } else {
            "receiver"
        })
        .unwrap();
    let connection_id = entity::ConnectionId::new();
    let config = ConnectionConfig {
        scope: entity::ProjectRoomKey::fixture(entity::RoomId::new("fabric-shadow").unwrap())
            .into(),
        participant_id,
        connection_id,
        profile: if native {
            SignalingProtocol::State
        } else if direction == str0m::media::Direction::SendOnly {
            SignalingProtocol::Whip
        } else {
            SignalingProtocol::Whep
        },
        authorization_expiry: None,
        rtc: accepted,
        resources,
    };
    let pair = Pair {
        peer,
        clock,
        local,
        remote,
        received: Vec::new(),
        data_received: Vec::new(),
        native_control,
        refresh: Vec::new(),
        output_form,
        rtp_loss: None,
        protocol_clock: clock,
        #[cfg(feature = "transport-conformance")]
        udp: None,
    };
    (
        (pair, config, mid, peer_mid, codec.payload_type, channel),
        answer,
    )
}

#[test]
fn native_data_v1_acknowledges_protected_publisher_and_subscriber_routes_before_delivery() {
    exercise_native_data_v1(pulsebeam_proto::state::DataV1Delivery::Unreliable);
}

#[test]
fn reliable_native_data_uses_protected_admission_and_subscriber_expectation_control() {
    exercise_native_data_v1(pulsebeam_proto::state::DataV1Delivery::Reliable);
}

fn exercise_native_data_v1(delivery: pulsebeam_proto::state::DataV1Delivery) {
    use crate::shard::core::FabricRtpFixture;
    use pulsebeam_core::fabric::Profile;
    use pulsebeam_proto::{data_endpoint::DATA_V1, state as wire};
    use str0m::media::{Direction, MediaKind};

    let sid = NonZeroU16::new(2).unwrap();
    let reliable = delivery == wire::DataV1Delivery::Reliable;
    let (mut publisher, mut config, _, _, _, peer_channel) = endpoint_pair_with_control(
        MediaKind::Audio,
        Direction::SendOnly,
        false,
        Some(sid),
        reliable,
    );
    let (mut subscriber, mut receiver, _, _, _, subscriber_channel) = endpoint_pair_with_control(
        MediaKind::Audio,
        Direction::RecvOnly,
        false,
        Some(sid),
        reliable,
    );
    let now = super::native_data::unix_now();
    let mut control = fabric_native_control(&mut config, now, publisher.clock.monotonic);
    let mut subscriber_control =
        fabric_native_control(&mut receiver, now, subscriber.clock.monotonic);
    if reliable {
        control.enable_reliable_data_fixture();
        subscriber_control.enable_reliable_data_fixture();
    }
    let mut shard = FabricRtpFixture::new(config, receiver, Profile::Data, None);
    publisher.connect(shard.publisher());
    subscriber.connect(shard.receiver());

    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                track_id: "cursor".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: if reliable {
                        wire::DataDelivery::Ordered
                    } else {
                        wire::DataDelivery::Lossy
                    } as i32,
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::SctpDataV1(
                            wire::SctpDataV1Parameters {
                                stream_id: Some(u32::from(sid.get())),
                                version: u32::from(DATA_V1),
                                channel_incarnation: 1,
                                delivery: delivery as i32,
                            },
                        )),
                    }),
                })),
            },
        )),
    };
    control
        .apply_input(&super::native_data::tests::frame(
            1,
            wire::desired_state::Value::Present(Box::new(spec)),
        ))
        .unwrap();
    let work = control.take_preparation().unwrap().unwrap();
    let command = control
        .prepare_execution(&work, now, None, |_| None)
        .unwrap()
        .unwrap();
    control.command(command);
    let (owner, mut sink) = shard.publisher_with_sink();
    owner.begin_native_control_turn(publisher.clock.monotonic, &mut sink);
    let peer_channel = peer_channel.unwrap();
    for _ in 0..32 {
        let (owner, mut sink) = shard.publisher_with_sink();
        publisher.step_with_sink(owner, None, &mut sink);
    }
    assert!(publisher.peer.channel(peer_channel).is_some());
    assert!(shard.publisher().fabric_data.is_empty());
    let publication = |value| match value {
        wire::confirmed_state::Value::Owned(live) => Some(*live),
        _ => None,
    };
    assert!(fabric_confirmation(&mut control, shard.publisher(), None, publication).is_none());
    shard.flush_receiver_feedback();
    let live = fabric_confirmation(&mut control, shard.publisher(), None, publication)
        .expect("shard acknowledged the publisher route");
    assert_eq!(live.execution(), wire::ExecutionStatus::Installed);
    assert!(live.data_v1_source.is_some_and(|source| source != 0));
    assert_eq!(shard.publisher().fabric_data.len(), 1);
    let id = PublicationKey::Data(live.track.as_ref().unwrap().publication_id.parse().unwrap());
    let track = shard.take_published_track(id).unwrap();
    assert_eq!(
        track.fabric_data_source().map(NonZeroU64::get),
        live.data_v1_source
    );
    let anchor = crate::route::fabric::DataPublisher {
        publication: id,
        connection: track.meta().connection_id,
        incarnation: track.fabric_data_source().unwrap(),
    };
    assert_eq!(shard.sample_data_publisher_anchor(anchor), Some(0));
    assert_eq!(
        shard.sample_data_publisher_anchor(crate::route::fabric::DataPublisher {
            incarnation: NonZeroU64::MAX,
            ..anchor
        }),
        None
    );
    assert_eq!(
        shard.sample_data_publisher_anchor(crate::route::fabric::DataPublisher {
            connection: crate::entity::ConnectionId::new(),
            ..anchor
        }),
        None
    );
    let publisher_probe =
        reliable.then(|| shard.install_peer_publisher_probe(anchor, Instant::now()));
    let subscriber_endpoint = install_fabric_test_subscription(
        &mut shard,
        &mut subscriber_control,
        &mut subscriber,
        &track,
        sid,
        now,
        delivery,
    );

    let data = crate::participant::data_fabric::R4;
    assert!(
        publisher
            .peer
            .channel(peer_channel)
            .unwrap()
            .write(true, &data)
            .unwrap()
    );
    let mut admission = None;
    let mut expectation = None;
    let subscriber_channel = subscriber_channel.unwrap();
    for _ in 0..64 {
        let (owner, mut sink) = shard.publisher_with_sink();
        publisher.step_with_sink(owner, None, &mut sink);
        shard.flush_publisher_media(publisher.clock.monotonic);
        shard.fire_fabric_control_timers(publisher.clock.monotonic);
        subscriber.step(shard.receiver(), None);
        admission = admission.or_else(|| take_protected_data_control(&mut publisher));
        expectation = expectation.or_else(|| take_protected_data_control(&mut subscriber));
        if subscriber
            .data_received
            .iter()
            .any(|message| message.id == subscriber_channel)
            && (!reliable || (admission.is_some() && expectation.is_some()))
        {
            break;
        }
    }
    let mut payloads = subscriber
        .data_received
        .iter()
        .filter(|message| message.id == subscriber_channel);
    let received = payloads.next().expect("exact Fabric record");
    assert!(payloads.next().is_none());
    assert!(received.binary);
    assert_eq!(received.data, data);
    assert_eq!(shard.sample_data_publisher_anchor(anchor), Some(1));
    if reliable {
        let admission = admission.expect("protected source admission ACK");
        let expectation = expectation.expect("protected child expectation");
        let unit = acknowledge_protected_expectation(
            &mut shard,
            &mut subscriber,
            subscriber_endpoint,
            admission,
            expectation,
        );
        use pulsebeam_proto::data_control::{RepairDemand, RepairStatus, subscriber_control};
        let missing = RepairDemand {
            unit: unit.unit,
            fragment_index: Some(0),
            request_id: 1,
        };
        let (_probe_link, route) = publisher_probe.unwrap();
        let peer_request = crate::route::fabric::local::RepairRequest {
            fragment: crate::route::fabric::local::RepairFragment {
                unit_seq: 3,
                index: 0,
            },
            id: NonZeroU64::MIN,
        };
        let mut peer_lookup = shard.probe_peer_publisher_lookup(
            route,
            crate::route::fabric::local::DataExpectation {
                unit_seq: 3,
                fragment_count: 2,
            },
            peer_request,
        );
        assert!(matches!(
            peer_lookup.try_recv(),
            Err(pulsebeam_runtime::sync::oneshot::error::TryRecvError::Empty)
        ));
        write_protected_subscriber_feedback(
            &mut subscriber,
            subscriber_endpoint,
            subscriber_control::Record::RequestRepair(missing),
        );
        let original = (0..32)
            .find_map(|_| {
                let (owner, mut sink) = shard.receiver_with_sink();
                subscriber.step_with_sink(owner, None, &mut sink);
                shard.flush_receiver_feedback();
                shard.fire_fabric_control_timers(
                    subscriber.clock.monotonic.max(publisher.clock.monotonic),
                );
                let (owner, mut sink) = shard.publisher_with_sink();
                publisher.step_with_sink(owner, None, &mut sink);
                take_protected_original_request(&mut publisher, subscriber_endpoint.source)
            })
            .expect("source requests unseen fragment from exact publisher");
        assert_eq!(original.unit, unit.unit.and_then(|unit| unit.source));
        assert_eq!(original.fragment_index, Some(0));
        let first_fragment = crate::participant::data_fabric::r3_record();
        assert!(
            publisher
                .peer
                .channel(peer_channel)
                .unwrap()
                .write(true, &first_fragment)
                .unwrap()
        );
        let mut repair_outcome = None;
        for _ in 0..32 {
            let (owner, mut sink) = shard.publisher_with_sink();
            publisher.step_with_sink(owner, None, &mut sink);
            shard.flush_publisher_media(publisher.clock.monotonic);
            subscriber.step(shard.receiver(), None);
            repair_outcome = repair_outcome
                .or_else(|| take_protected_repair_outcome(&mut subscriber, subscriber_endpoint));
            if repair_outcome.is_some()
                && subscriber.data_received.iter().any(|message| {
                    message.id == subscriber_channel && message.data == first_fragment
                })
            {
                break;
            }
        }
        assert_eq!(
            subscriber
                .data_received
                .iter()
                .filter(|message| message.id == subscriber_channel)
                .map(|message| message.data.as_slice())
                .collect::<Vec<_>>(),
            [data.as_slice(), first_fragment.as_slice()]
        );
        assert_eq!(
            repair_outcome.expect("live arrival consumed pending demand"),
            pulsebeam_proto::data_control::RepairOutcome {
                demand: Some(missing),
                status: RepairStatus::HandedOff as i32
            }
        );
        assert!(take_protected_delivery(&mut publisher, subscriber_endpoint.source).is_none());
        assert_eq!(shard.sample_data_publisher_anchor(anchor), Some(2));
        let cached = RepairDemand {
            fragment_index: Some(1),
            request_id: 2,
            ..missing
        };
        write_protected_subscriber_feedback(
            &mut subscriber,
            subscriber_endpoint,
            subscriber_control::Record::RequestRepair(cached),
        );
        write_protected_subscriber_feedback(
            &mut subscriber,
            subscriber_endpoint,
            subscriber_control::Record::RequestRepair(cached),
        );
        let mut outcome = None;
        for _ in 0..32 {
            let (owner, mut sink) = shard.receiver_with_sink();
            subscriber.step_with_sink(owner, None, &mut sink);
            shard.flush_receiver_feedback();
            shard.fire_fabric_control_timers(
                subscriber.clock.monotonic.max(publisher.clock.monotonic),
            );
            if let Some(next) = take_protected_repair_outcome(&mut subscriber, subscriber_endpoint)
            {
                assert!(outcome.replace(next).is_none());
            }
        }
        assert_eq!(
            outcome,
            Some(pulsebeam_proto::data_control::RepairOutcome {
                demand: Some(cached),
                status: RepairStatus::HandedOff as i32
            })
        );
        assert_eq!(
            subscriber
                .data_received
                .iter()
                .filter(|message| message.id == subscriber_channel)
                .map(|message| message.data.as_slice())
                .collect::<Vec<_>>(),
            [data.as_slice(), first_fragment.as_slice(), data.as_slice()]
        );
        assert_protected_receipt_ack(&mut shard, &mut subscriber, subscriber_endpoint, unit);
        let mut reported = None;
        for _ in 0..32 {
            let now = publisher.clock.monotonic.max(subscriber.clock.monotonic);
            shard.fire_fabric_control_timers(now);
            let (owner, mut sink) = shard.publisher_with_sink();
            publisher.step_with_sink(owner, None, &mut sink);
            if let Some(delivery) =
                take_protected_delivery(&mut publisher, subscriber_endpoint.source)
            {
                assert!(
                    reported.replace(delivery).is_none(),
                    "replayed receipt did not create another delivery outcome"
                );
            }
        }
        let report = reported.expect("delivery reported to the exact publisher lease");
        assert_eq!(report.expectation, Some(unit));
        assert_eq!(
            report.outcome,
            pulsebeam_proto::data_control::ControlOutcome::Confirmed as i32
        );
        let after_retry = (Instant::now() + std::time::Duration::from_millis(101))
            .max(publisher.clock.monotonic)
            .max(subscriber.clock.monotonic);
        shard.fire_fabric_control_timers(after_retry);
        let located = peer_lookup.try_recv().unwrap().unwrap();
        assert_eq!(located.sequence, 1);
        assert!(
            matches!(located.lookup.as_ref(), crate::route::fabric::relay::PeerRepairLookup::Publisher(lookup) if lookup.request == peer_request)
        );
    }
}

fn take_protected_data_control(
    pair: &mut Pair,
) -> Option<pulsebeam_proto::state::NativeDataControl> {
    let channel = pair.native_control?;
    let (index, control) = pair
        .data_received
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            if message.id != channel || !message.binary {
                return None;
            }
            pulsebeam_proto::state::codec::decode_data_control_frame(&message.data)?
                .ok()
                .map(|control| (index, control))
        })?;
    pair.data_received.remove(index);
    Some(control)
}

fn take_protected_original_request(
    pair: &mut Pair,
    source: NonZeroU64,
) -> Option<pulsebeam_proto::data_control::OriginalDemand> {
    use pulsebeam_proto::{
        data_control::{
            self, PublisherBinding, PublisherObservation, publisher_observation::Record,
        },
        state::native_data_control::Endpoint,
    };
    while let Some(frame) = take_protected_data_control(pair) {
        let Some(Endpoint::Publisher(frame)) = frame.endpoint else {
            panic!("publisher control role")
        };
        assert_eq!(frame.owned, 1);
        let observation = data_control::decode::<PublisherObservation>(
            PublisherBinding { source },
            &frame.record,
        )
        .unwrap();
        if let Some(Record::RequestOriginal(demand)) = observation.record {
            return Some(demand);
        }
    }
    None
}

fn take_protected_repair_outcome(
    pair: &mut Pair,
    endpoint: pulsebeam_proto::data_control::SubscriberBinding,
) -> Option<pulsebeam_proto::data_control::RepairOutcome> {
    use pulsebeam_proto::{
        data_control::{self, SubscriberObservation, subscriber_observation::Record},
        state::native_data_control::Endpoint,
    };
    while let Some(frame) = take_protected_data_control(pair) {
        let Some(Endpoint::Subscriber(frame)) = frame.endpoint else {
            panic!("subscriber control role")
        };
        assert_eq!(frame.owned, 1);
        let observation =
            data_control::decode::<SubscriberObservation>(endpoint, &frame.record).unwrap();
        if let Some(Record::Repair(result)) = observation.record {
            return Some(result);
        }
    }
    None
}

fn take_protected_delivery(
    pair: &mut Pair,
    source: NonZeroU64,
) -> Option<pulsebeam_proto::data_control::DeliveryOutcome> {
    use pulsebeam_proto::{
        data_control::{
            self, PublisherBinding, PublisherObservation, publisher_observation::Record,
        },
        state::native_data_control::Endpoint,
    };
    while let Some(frame) = take_protected_data_control(pair) {
        let Some(Endpoint::Publisher(frame)) = frame.endpoint else {
            panic!("publisher control role")
        };
        assert_eq!(frame.owned, 1);
        let observation = data_control::decode::<PublisherObservation>(
            PublisherBinding { source },
            &frame.record,
        )
        .unwrap();
        if let Some(Record::Delivery(report)) = observation.record {
            return Some(report);
        }
    }
    None
}

fn acknowledge_protected_expectation(
    shard: &mut crate::shard::core::FabricRtpFixture,
    subscriber: &mut Pair,
    endpoint: pulsebeam_proto::data_control::SubscriberBinding,
    admission: pulsebeam_proto::state::NativeDataControl,
    expectation: pulsebeam_proto::state::NativeDataControl,
) -> pulsebeam_proto::data_control::Expectation {
    use pulsebeam_proto::{
        data_control::{
            self, BranchUnit, ControlOutcome, Expectation, ExpectationOutcome, PublisherBinding,
            PublisherObservation, SourceUnit, SubscriberObservation, publisher_observation,
            subscriber_control, subscriber_observation,
        },
        state::native_data_control::Endpoint,
    };

    let source = SourceUnit {
        source_incarnation: endpoint.source.get(),
        unit_seq: Some(3),
    };
    let Some(Endpoint::Publisher(admission)) = admission.endpoint else {
        panic!("publisher admission")
    };
    assert_eq!(admission.owned, 1);
    assert_eq!(
        data_control::decode::<PublisherObservation>(
            PublisherBinding {
                source: endpoint.source
            },
            &admission.record
        )
        .unwrap()
        .record,
        Some(publisher_observation::Record::Admission(source))
    );
    let Some(Endpoint::Subscriber(expectation)) = expectation.endpoint else {
        panic!("subscriber expectation")
    };
    assert_eq!(expectation.owned, 1);
    let unit = Expectation {
        unit: Some(BranchUnit {
            source: Some(source),
            branch_incarnation: endpoint.branch.get(),
        }),
        fragment_count: 2,
    };
    assert_eq!(
        data_control::decode::<SubscriberObservation>(endpoint, &expectation.record)
            .unwrap()
            .record,
        Some(subscriber_observation::Record::Expectation(unit))
    );
    assert!(shard.has_fabric_control_work());
    write_protected_subscriber_feedback(
        subscriber,
        endpoint,
        subscriber_control::Record::ExpectationOutcome(ExpectationOutcome {
            expectation: Some(unit),
            outcome: ControlOutcome::Confirmed as i32,
        }),
    );
    for _ in 0..32 {
        let (owner, mut sink) = shard.receiver_with_sink();
        subscriber.step_with_sink(owner, None, &mut sink);
        shard.flush_receiver_feedback();
        shard.fire_fabric_control_timers(subscriber.clock.monotonic);
        if !shard.has_fabric_control_work() {
            break;
        }
    }
    assert!(
        !shard.has_fabric_control_work(),
        "protected exact ACK settled the branch retry"
    );
    unit
}

fn write_protected_subscriber_feedback(
    subscriber: &mut Pair,
    endpoint: pulsebeam_proto::data_control::SubscriberBinding,
    feedback: pulsebeam_proto::data_control::subscriber_control::Record,
) {
    use pulsebeam_proto::{
        data_control::{self, SubscriberControl},
        state::{NativeDataControl, OwnedDataControl, codec, native_data_control::Endpoint},
    };
    let record = data_control::encode(
        endpoint,
        &SubscriberControl {
            record: Some(feedback),
        },
    )
    .unwrap();
    let bytes = codec::encode_data_control(&NativeDataControl {
        endpoint: Some(Endpoint::Subscriber(OwnedDataControl { owned: 1, record })),
    })
    .unwrap();
    assert!(
        subscriber
            .peer
            .channel(subscriber.native_control.unwrap())
            .unwrap()
            .write(true, &bytes)
            .unwrap()
    );
}

fn assert_protected_receipt_ack(
    shard: &mut crate::shard::core::FabricRtpFixture,
    subscriber: &mut Pair,
    endpoint: pulsebeam_proto::data_control::SubscriberBinding,
    unit: pulsebeam_proto::data_control::Expectation,
) {
    use pulsebeam_proto::{
        data_control::{self, SubscriberObservation, subscriber_control, subscriber_observation},
        state::native_data_control::Endpoint,
    };
    for _ in 0..2 {
        write_protected_subscriber_feedback(
            subscriber,
            endpoint,
            subscriber_control::Record::AssemblyReceipt(unit),
        );
        let mut ack = None;
        for _ in 0..32 {
            let (owner, mut sink) = shard.receiver_with_sink();
            subscriber.step_with_sink(owner, None, &mut sink);
            shard.flush_receiver_feedback();
            if let Some(control) = take_protected_data_control(subscriber) {
                ack = Some(control);
                break;
            }
        }
        let Some(Endpoint::Subscriber(frame)) =
            ack.expect("acknowledged assembly receipt").endpoint
        else {
            panic!("exact receipt role")
        };
        assert_eq!(frame.owned, 1);
        assert_eq!(
            data_control::decode::<SubscriberObservation>(endpoint, &frame.record)
                .unwrap()
                .record,
            Some(subscriber_observation::Record::ReceiptAck(unit))
        );
    }
}

fn fabric_native_control(
    config: &mut ConnectionConfig,
    now: u64,
    rtc_now: Instant,
) -> crate::control::native::fixture::NativeControl {
    use crate::control::native::fixture::NativeControl;
    let expiry = now.checked_add(60).unwrap();
    let (mut control, handoff) = NativeControl::new(
        config.participant_id,
        config.connection_id,
        config.scope.room.unwrap(),
        None,
        expiry,
    );
    config.authorization_expiry =
        Some(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(expiry));
    config.resources.native_handoff = Some(handoff);
    control.enable_data_v1();
    control.set_cid(crate::media::ChannelId::from_value(0), rtc_now);
    control
}

fn fabric_confirmation<T>(
    control: &mut crate::control::native::fixture::NativeControl,
    owner: &mut Connection,
    source: Option<&crate::track::Track>,
    pick: impl Fn(pulsebeam_proto::state::confirmed_state::Value) -> Option<T>,
) -> Option<T> {
    use pulsebeam_proto::state::{codec, resource_key};
    for _ in 0..32 {
        control
            .service_with_lookup(|id| source.filter(|track| track.id() == id), |_| source)
            .unwrap();
        let handoff = owner.native_handoff.as_mut().unwrap();
        let output = handoff.output()?;
        let batch = codec::decode_server(&output.bytes).unwrap();
        assert!(
            handoff.progress(crate::participant::signaling::SignalingProgress {
                sequence: output.sequence,
                cid: output.cid,
                at: Instant::now(),
            })
        );
        for record in batch.records {
            if record.key.and_then(|key| key.key) == Some(resource_key::Key::Owned(1))
                && let Some(value) = record
                    .confirmed
                    .and_then(|confirmed| confirmed.value)
                    .and_then(&pick)
            {
                return Some(value);
            }
        }
    }
    panic!("control output did not quiesce");
}

fn install_fabric_test_subscription(
    shard: &mut crate::shard::core::FabricRtpFixture,
    control: &mut crate::control::native::fixture::NativeControl,
    subscriber: &mut Pair,
    track: &crate::track::Track,
    stream: NonZeroU16,
    now: u64,
    delivery: pulsebeam_proto::state::DataV1Delivery,
) -> pulsebeam_proto::data_control::SubscriberBinding {
    use pulsebeam_proto::{data_endpoint::DATA_V1, state as wire};
    let meta = track.meta();
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                data_version: Some(u32::from(DATA_V1)),
                source: Some(wire::LogicalSourceAddress {
                    room: meta.room.room.to_string(),
                    participant_id: meta.origin.to_string(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: meta.track_id.unwrap().to_string(),
                }),
            },
        )),
    };
    control
        .apply_input(&super::native_data::tests::frame(
            1,
            wire::desired_state::Value::Present(Box::new(spec)),
        ))
        .unwrap();
    let logical = |value| match value {
        wire::confirmed_state::Value::LogicalSubscription(logical) if logical.data_v1.is_some() => {
            Some(*logical)
        }
        _ => None,
    };
    let offer = fabric_confirmation(control, shard.receiver(), Some(track), logical)
        .expect("one explicit subscriber offer");
    assert_eq!(
        offer.data_v1.as_ref().map(|offer| offer.source),
        track.fabric_data_source().map(NonZeroU64::get)
    );
    let binding = wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::SctpDataV1(
            wire::SctpDataV1Parameters {
                stream_id: Some(u32::from(stream.get())),
                version: u32::from(DATA_V1),
                channel_incarnation: 2,
                delivery: delivery as i32,
            },
        )),
    };
    let report = wire::ClientStateBatch {
        records: vec![wire::ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(wire::ExecutionReport {
                revision: 2,
                outcome: Some(wire::execution_report::Outcome::LogicalReceiver(
                    wire::LogicalReceiverReport {
                        source_revision: offer.source_revision,
                        outcome: Some(wire::logical_receiver_report::Outcome::Prepared(
                            binding.clone(),
                        )),
                    },
                )),
            }),
        }],
        ..Default::default()
    };
    assert!(
        control
            .receive_input(&wire::codec::encode_client(&report).unwrap(), now)
            .unwrap()
            .is_empty()
    );
    let work = control.take_preparation().unwrap().unwrap();
    let command = control
        .prepare_execution(&work, now, None, |_| {
            Some(crate::control::native::ResolvedSource {
                track,
                publisher: &meta.origin,
            })
        })
        .unwrap()
        .unwrap();
    control.command(command);
    let (owner, mut sink) = shard.receiver_with_sink();
    owner.begin_native_control_turn(subscriber.clock.monotonic, &mut sink);
    for _ in 0..32 {
        let (owner, mut sink) = shard.receiver_with_sink();
        subscriber.step_with_sink(owner, None, &mut sink);
    }
    shard.flush_receiver_feedback();
    assert!(
        fabric_confirmation(control, shard.receiver(), Some(track), logical)
            .is_none_or(|state| state.execution.is_none())
    );
    assert!(shard.receiver().fabric_data.is_empty());

    shard.apply_data_subscription_plan(track, work.reference);
    let (owner, mut sink) = shard.receiver_with_sink();
    for _ in 0..8 {
        if !owner.advance_native_source(&mut sink) {
            break;
        }
    }
    shard.flush_receiver_feedback();
    let installed = fabric_confirmation(control, shard.receiver(), Some(track), logical)
        .expect("shard acknowledged the subscriber route");
    assert_eq!(installed.source_revision, offer.source_revision);
    assert_eq!(
        installed.execution,
        Some(wire::logical_subscription_state::Execution::Installed(
            binding
        ))
    );
    assert_eq!(shard.receiver().fabric_data.len(), 1);
    installed.data_v1.unwrap().endpoint().unwrap()
}

#[test]
fn protected_sctp_data_uses_the_bound_pt99_route_and_preserves_each_message() {
    use crate::shard::core::FabricRtpFixture;
    use pulsebeam_core::fabric::Profile;
    use pulsebeam_proto::{
        data_endpoint::DATA_V1,
        state::{DataV1Delivery, SctpDataV1Parameters},
    };
    use str0m::media::{Direction, MediaKind};

    use crate::participant::data_fabric::{R4, r3_record};
    let r3 = r3_record();
    let sid = NonZeroU16::new(2).unwrap();
    let (mut publisher, pub_config, _, _, _, pub_channel) =
        endpoint_pair(MediaKind::Audio, Direction::SendOnly, false, Some(sid));
    let (mut subscriber, sub_config, _, _, _, sub_channel) =
        endpoint_pair(MediaKind::Audio, Direction::RecvOnly, false, Some(sid));
    let pub_channel = pub_channel.unwrap();
    let sub_channel = sub_channel.unwrap();
    let mut shard = FabricRtpFixture::new(pub_config, sub_config, Profile::Data, None);
    let parameters = SctpDataV1Parameters {
        stream_id: Some(u32::from(sid.get())),
        version: u32::from(DATA_V1),
        channel_incarnation: 1,
        delivery: DataV1Delivery::Unreliable as i32,
    };
    assert!(matches!(
        shard
            .publisher()
            .transport
            .install_fabric_data(SctpDataV1Parameters {
                version: 2,
                ..parameters
            }),
        Err(pulsebeam_rtc::WriteError::InvalidState)
    ));
    let ingress = shard
        .publisher()
        .transport
        .install_fabric_data(parameters)
        .unwrap();
    let egress = shard
        .receiver()
        .transport
        .install_fabric_data(parameters)
        .unwrap();
    shard.attach_data(parameters, ingress, egress);
    publisher.connect(shard.publisher());
    subscriber.connect(shard.receiver());
    for _ in 0..2_000 {
        if publisher.peer.channel(pub_channel).is_some()
            && subscriber.peer.channel(sub_channel).is_some()
            && shard.publisher().transport.supports_fabric_data(ingress)
            && shard.receiver().transport.supports_fabric_data(egress)
        {
            break;
        }
        publisher.step(shard.publisher(), None);
        subscriber.step(shard.receiver(), None);
    }
    assert!(shard.publisher().transport.supports_fabric_data(ingress));
    assert!(shard.receiver().transport.supports_fabric_data(egress));
    let step_publisher = |pair: &mut Pair, shard: &mut FabricRtpFixture| {
        let (connection, mut events) = shard.publisher_with_sink();
        pair.step_with_sink(connection, None, &mut events);
        shard.flush_publisher_media(pair.clock.monotonic);
    };
    // A valid DATA v1 descriptor cannot be forwarded over an unqualified path.
    const WITH_DD: &[u8] = &[
        0x50, 0x44, 1, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, 0xc0, 0, 0,
    ];
    assert!(pulsebeam_proto::data_record::Record::decode(WITH_DD, None).is_ok());
    for (binary, bytes) in [
        (false, R4.as_slice()),
        (true, b"".as_slice()),
        (true, b"bad".as_slice()),
        (true, WITH_DD),
    ] {
        assert!(
            publisher
                .peer
                .channel(pub_channel)
                .unwrap()
                .write(binary, bytes)
                .unwrap()
        );
    }
    for _ in 0..32 {
        step_publisher(&mut publisher, &mut shard);
        subscriber.step(shard.receiver(), None);
    }
    assert!(subscriber.data_received.is_empty());

    for bytes in [R4.as_slice(), r3.as_slice()] {
        assert!(
            publisher
                .peer
                .channel(pub_channel)
                .unwrap()
                .write(true, bytes)
                .unwrap()
        );
        for _ in 0..256 {
            step_publisher(&mut publisher, &mut shard);
            subscriber.step(shard.receiver(), None);
            if !subscriber.data_received.is_empty() {
                break;
            }
        }
        let mut delivered = subscriber.data_received.drain(..);
        let message = delivered
            .next()
            .expect("authenticated DATA reached the exact branch");
        assert_eq!(message.id, sub_channel);
        assert!(message.binary);
        assert_eq!(message.data, bytes);
        assert!(delivered.next().is_none());
    }
}

#[test]
fn authenticated_opus_shadow_uses_observed_clock_without_a_second_delivery() {
    let (mut pair, config, mid, peer_mid, payload_type) = media_pair(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::SendOnly,
        false,
    );
    let mut sink = MockParticipantSink::new();
    let mut connection = Connection::new(config, ShardId::new(0), 8);
    pair.connect(&mut connection);
    assert!(
        connection
            .transport
            .whip_opus_receive_binding(mid)
            .is_none()
    );
    let wall = pair.clock.wall;
    let write = |pair: &mut Pair, timestamp, bytes: &'static [u8]| {
        pair.peer
            .writer(peer_mid)
            .unwrap()
            .write(
                payload_type.into(),
                pair.clock.monotonic.into(),
                MediaTime::from_millis(timestamp),
                bytes,
            )
            .unwrap();
    };
    write(&mut pair, 20, &[0xf8, 0xff, 0xfe]);
    let mut ingress = None;
    let mut observe = |packet: AuthenticatedRtp<'_>, _| {
        ingress = Some(
            RtpIngressShadow::bind_whole(
                pulsebeam_core::fabric::Profile::Opus,
                packet.binding(),
                ExtensionForm::OneByte,
                &[],
                Anchors::default(),
                Limits {
                    positions: NonZeroU16::new(4).unwrap(),
                    max_age: Duration::from_millis(120),
                    max_idle: Duration::from_secs(2),
                },
            )
            .unwrap(),
        );
    };
    for _ in 0..200 {
        pair.step_with_sink(&mut connection, Some(&mut observe), &mut sink);
        if !sink.publish_track_calls.is_empty() {
            break;
        }
    }
    let (_, publication_id) = connection.upstream.slot_for_mid(mid).unwrap();
    assert_eq!(sink.publish_track_calls, [publication_id]);
    assert!(sink.publish_track_packet_calls.is_empty());
    let fanout = slotmap::SlotMap::<TrackHandle, ()>::with_key().insert(());
    connection.apply(
        ParticipantEffect::TrackPublished { publication_id },
        Some(fanout),
    );
    write(&mut pair, 40, &[0xf8, 0xff, 0xfe]);
    for _ in 0..200 {
        pair.step_with_sink(&mut connection, None, &mut sink);
        if !sink.publish_track_packet_calls.is_empty() {
            break;
        }
    }
    assert_eq!(sink.publish_track_packet_calls, [fanout]);
    assert!(
        connection
            .prepare_whip_opus_ingress(publication_id, fanout)
            .is_none()
    );
    connection.authorization_expiry = Some(
        pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(native_data::unix_now() + 60),
    );
    let (authorized, qualified) = connection
        .prepare_whip_opus_ingress(publication_id, fanout)
        .unwrap();
    assert_eq!(authorized.id, publication_id);
    let ingress = ingress.unwrap();
    assert_eq!(qualified.primary_ssrc(), ingress.primary_ssrc());
    assert!(connection.attach_fabric_media_shadow(publication_id, ingress));
    write(&mut pair, 60, &[0xf8, 0xff, 0xfe]);
    for _ in 0..200 {
        pair.step_with_sink(&mut connection, None, &mut sink);
        if sink.publish_track_packet_calls.len() >= 2 {
            break;
        }
    }
    assert_eq!(sink.publish_track_packet_calls, [fanout, fanout]);
    let shadow = connection.media_ingress.values().next().unwrap();
    let (stamp, observation) = shadow.observation.unwrap();
    assert_eq!(
        stamp,
        crate::clock::NtpTime::from_system_time(wall).middle32()
    );
    assert!(matches!(
        observation,
        Observation::Media {
            first: true,
            wan_plaintext_bytes: 15,
            ..
        }
    ));

    let fenced =
        slotmap::SlotMap::<crate::shard::router::FabricSourceHandle, ()>::with_key().insert(());
    connection
        .media_ingress
        .values_mut()
        .next()
        .unwrap()
        .publisher_source = Some(fenced);
    connection.fenced_fabric_sources.insert(fenced);
    assert!(!connection.fabric_source_authorized(fenced));
    write(&mut pair, 80, &[0xf8, 0xff, 0xfe]);
    for _ in 0..200 {
        pair.step_with_sink(&mut connection, None, &mut sink);
    }
    assert_eq!(sink.publish_track_packet_calls, [fanout, fanout]);
}

#[test]
fn authenticated_large_primary_and_rtx_fan_out_and_receiver_refresh_reaches_the_publisher() {
    use crate::rtp::fabric_egress::{
        CaptureClock, EndpointHints, OutputLeg, SenderLeg, SourceMetadata, Vad, timeline,
    };
    use crate::shard::{core::FabricRtpFixture, worker::ShardFrame};
    use pulsebeam_core::fabric::{Profile, WireError, wan::Window};
    use str0m::media::{Direction, KeyframeRequestKind, MediaKind};

    let (mut tx, tx_config, tx_leg, tx_mid, tx_pt) =
        media_pair(MediaKind::Video, Direction::SendOnly, true);
    let (mut rx, rx_config, mid, rx_mid, _) =
        media_pair(MediaKind::Video, Direction::RecvOnly, true);
    let (ssrc, repair) = {
        let mut direct = tx.peer.direct_api();
        let stream = direct.stream_tx_by_mid(tx_mid, None).unwrap();
        (stream.ssrc(), stream.rtx().expect("negotiated RTX"))
    };
    let mut shard =
        FabricRtpFixture::new(tx_config, rx_config, Profile::H264, NonZeroU32::new(*ssrc));
    tx.connect(shard.publisher());
    rx.connect(shard.receiver());
    let now = tx.clock.monotonic.max(rx.clock.monotonic);
    tx.clock.monotonic = now;
    rx.clock.monotonic = now;
    let send = |tx: &mut Pair, sequence: u64, bytes: &[u8]| {
        tx.peer
            .direct_api()
            .stream_tx_by_mid(tx_mid, None)
            .unwrap()
            .write_rtp(
                str0m::rtp::RtpWrite::new(
                    tx_pt.into(),
                    sequence.into(),
                    9000,
                    tx.clock.monotonic.into(),
                    bytes,
                )
                .marker(true)
                .nackable(true),
            );
    };
    // Use an opaque NAL that fits the new envelope but not the old image.
    let mut bytes = [0x65; 1150];
    bytes[0] = 0x1f;
    bytes[1149] = 0xa5;
    send(&mut tx, 99, &bytes);
    let mut ingress = None;
    for _ in 0..200 {
        tx.step(
            shard.publisher(),
            Some(&mut |packet: AuthenticatedRtp<'_>, _| {
                assert_eq!(packet.primary_ssrc, *ssrc);
                let mut shadow = RtpIngressShadow::bind_whole(
                    Profile::H264,
                    packet.binding(),
                    ExtensionForm::OneByte,
                    &[],
                    Anchors::default(),
                    Limits {
                        positions: NonZeroU16::new(64).unwrap(),
                        max_age: Duration::from_millis(120),
                        max_idle: Duration::from_secs(2),
                    },
                )
                .unwrap();
                assert_eq!(
                    shadow.observe(packet.arrival.into(), 0, packet),
                    Ok(Observation::Media {
                        sequence: 99,
                        first: true,
                        wan_plaintext_bytes: 1162,
                    })
                );
                ingress = Some(shadow);
            }),
        );
        if ingress.is_some() {
            break;
        }
    }
    let (_, publication) = shard.publisher().upstream.slot_for_mid(tx_leg).unwrap();
    shard.use_negotiated_publication(publication, NonZeroU32::new(*ssrc));
    assert!(shard.attach_publisher_ingress(ingress.unwrap()));
    let form = match rx.output_form.unwrap() {
        pulsebeam_rtc::RtpExtensionForm::OneByte => ExtensionForm::OneByte,
        pulsebeam_rtc::RtpExtensionForm::TwoByte => ExtensionForm::TwoByte,
        pulsebeam_rtc::RtpExtensionForm::Mixed => ExtensionForm::Mixed,
    };
    let hints = EndpointHints::new(Profile::H264, form, &[], None).unwrap();
    let leg = SenderLeg::new(
        OutputLeg {
            mid,
            primary: None,
            max_wire_bytes: 1200,
            replace_queued: false,
        },
        hints,
        timeline::Timeline::new(500, 1500, 90_000, 64).unwrap(),
    );
    assert!(shard.bind_output(
        leg,
        SourceMetadata {
            vad: Vad::On,
            capture: CaptureClock::Unmapped,
            same_topology: true,
        }
    ));
    let (_mailbox_owner, mut mailbox) = shard.bind_publisher_mailbox(tx.clock.monotonic);
    send(&mut tx, 100, &bytes);
    for _ in 0..200 {
        let (owner, mut sink) = shard.publisher_with_sink();
        tx.step_with_sink(owner, None, &mut sink);
        if shard.flush_publisher_media(tx.clock.monotonic) > 0 {
            break;
        }
    }
    rx.clock.monotonic = tx.clock.monotonic;
    for _ in 0..200 {
        rx.step_receiver(&mut shard);
        if !rx.received.is_empty() {
            break;
        }
    }
    assert_eq!(rx.received.len(), 1);
    assert_eq!(rx.received[0].payload.as_ref(), bytes);
    assert!(rx.received[0].header.marker);

    let mut lost = bytes;
    lost[1] = 0x55;
    tx.rtp_loss = Some(RtpLoss {
        primary: *ssrc,
        repair: *repair,
        drop_sequence: Some(101),
        repair_udp_bytes: None,
    });
    send(&mut tx, 101, &lost);
    send(&mut tx, 102, &[0x1f, 0x10]);
    for _ in 0..200 {
        let (owner, mut sink) = shard.publisher_with_sink();
        tx.step_with_sink(owner, None, &mut sink);
        shard.flush_publisher_media(tx.clock.monotonic);
        rx.step_receiver(&mut shard);
        if rx
            .received
            .iter()
            .any(|packet| packet.payload.as_ref() == lost)
        {
            break;
        }
    }
    let witness = tx.rtp_loss.take().unwrap();
    assert_eq!(witness.drop_sequence, None);
    assert!(matches!(witness.repair_udp_bytes, Some(1180..=1216)));
    let repaired = rx
        .received
        .iter()
        .find(|packet| packet.payload.as_ref() == lost)
        .expect("only the authenticated RTX copy can deliver the lost payload");
    assert!(repaired.header.marker);

    let mut frames = Vec::new();
    assert_eq!(mailbox.try_recv_batch(&mut frames, 8), 3);
    let window =
        Window::from_tuple([3, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 6, 7, 8], 0).unwrap();
    let mut large_positions = Vec::new();
    for frame in frames {
        let ShardFrame::Fabric(offer) = frame.packet else {
            panic!("expected the typed shard mailbox, not a legacy image");
        };
        let expected = match offer.sequence {
            100 => &bytes,
            101 => &lost,
            102 => continue,
            other => panic!("unexpected source position {other}"),
        };
        large_positions.push(offer.sequence);
        assert_eq!(offer.packet.payload(), expected);
        assert_eq!(offer.packet.legacy_wire_len(), Err(WireError::Oversize));
        assert_eq!(offer.packet.remaining_hops(), 7);
        let mut image = [0; 1162];
        assert_eq!(
            window.write(offer.sequence, &offer.packet, &mut image),
            Ok(1162)
        );
        assert_eq!(image[0..2], [0x80, 0xe6]);
        let (position, decoded) = window.decode(&image).unwrap();
        assert_eq!(position, offer.sequence);
        assert!(decoded.same_content(&offer.packet.arrival_view(7).unwrap()));
        assert_eq!(decoded.remaining_hops(), 6);
    }
    assert_eq!(large_positions, [100, 101]);

    rx.peer
        .direct_api()
        .stream_rx_by_mid(rx_mid, None)
        .unwrap()
        .request_keyframe(KeyframeRequestKind::Pli);
    tx.refresh.clear();
    for _ in 0..250 {
        rx.step_receiver(&mut shard);
        shard.flush_receiver_feedback();
        tx.step(shard.publisher(), None);
        if !tx.refresh.is_empty() {
            break;
        }
    }
    assert_eq!(tx.refresh.len(), 1);
    assert_eq!(tx.refresh[0].mid, tx_mid);
    assert_eq!(tx.refresh[0].kind, KeyframeRequestKind::Pli);
}

#[test]
fn filtered_fabric_dd_retries_overbudget_entry_and_rebootstraps_after_feedback() {
    use str0m::media::{Direction, MediaKind};

    prove_filtered_dd_switch(media_pair(MediaKind::Video, Direction::RecvOnly, true));
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn filtered_fabric_dd_switch_on_installed_fid_and_nonfragmenting_socket() {
    use str0m::media::{Direction, MediaKind};

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut options = pulsebeam_udp::BindOptions::new(addr);
    options.nonfragmenting_max_datagram_bytes = NonZeroU16::new(1_216);
    let local = pulsebeam_udp::bind(options)
        .await
        .unwrap()
        .activate()
        .unwrap();
    assert_eq!(
        local.nonfragmenting_max_datagram_bytes(),
        NonZeroU16::new(1_216)
    );
    let remote = std::net::UdpSocket::bind(addr).unwrap();
    remote.set_nonblocking(true).unwrap();
    let ((mut peer, config, mid, peer_mid, pt, _), answer) = endpoint_pair_at(
        MediaKind::Video,
        Direction::RecvOnly,
        true,
        None,
        false,
        [local.local_addr(), remote.local_addr().unwrap()],
    );
    let [primary, repair] = crate::stock_rtc::answer_fid(&answer, mid.as_str());
    peer.udp = Some(ScopedTestUdp {
        local,
        remote,
        primary,
        repair: Some(repair),
        received_primary: 0,
        peak: 0,
    });
    prove_filtered_dd_switch((peer, config, mid, peer_mid, pt));
    decode_stock_h264_on_participant_udp(true).await;
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn dd_free_h264_repairs_at_exact_protected_ceiling_on_participant_udp() {
    decode_stock_h264_on_participant_udp(false).await;
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn native_maximum_encoded_opus_reaches_authenticated_rtp() {
    use crate::stock_rtc::{LoopbackMedia, NativeMediaEndpoint, assert_decoded_stock_opus};

    let opus = maximum_stock_opus_packet();
    assert_decoded_stock_opus(opus.clone(), 960);

    // The observation socket must not inherit the narrower Fabric limit.
    let (mut publisher, config, _, primary) =
        NativePublisherUdp::connect_with_cap(LoopbackMedia::OpusPublisher, 1_500).await;
    let mut endpoint =
        NativeParticipantUdp::connect_with_publisher(LoopbackMedia::Opus, Some(config)).await;
    publisher.native.push_opus(opus.clone(), 960);
    let mut protected = None;
    let mut authenticated = false;
    for _ in 0..200 {
        publisher
            .step(
                endpoint.now,
                endpoint.shard.publisher(),
                &mut MockParticipantSink::new(),
                Some(&mut |packet: AuthenticatedRtp<'_>, _| {
                    assert_eq!(packet.primary_ssrc, primary.get());
                    assert_eq!(packet.wire_payload(), opus);
                    assert_eq!(packet.protected_bytes(), Some(1_236));
                    authenticated = true;
                }),
                |bytes| {
                    if let Some((ssrc, _)) = fixture_rtp(bytes) {
                        assert_eq!(ssrc, primary.get());
                        let base = 12 + usize::from(bytes[0] & 0x0f) * 4;
                        let extension = if bytes[0] & 0x10 != 0 {
                            4 + 4 * usize::from(u16::from_be_bytes([
                                bytes[base + 2],
                                bytes[base + 3],
                            ]))
                        } else {
                            0
                        };
                        assert_eq!(bytes[0] & 0x20, 0, "no RTP padding");
                        assert_eq!(bytes.len(), base + extension + opus.len() + 16);
                        assert!(protected.replace(bytes.len()).is_none());
                    }
                    true
                },
            )
            .await;
        endpoint.step(NativeMediaEndpoint::deliver);
        if authenticated {
            break;
        }
    }
    assert!(authenticated);
    eprintln!(
        "Native maximum input Opus protected UDP bytes: {}",
        protected.unwrap()
    );
}

#[cfg(feature = "transport-conformance")]
fn maximum_stock_opus_packet() -> Vec<u8> {
    const MAX_OPUS_INPUT: usize = 1_200;
    let source = crate::stock_rtc::stock_opus_packet();
    assert_eq!(source[0] & 3, 0, "single-frame stock packet");
    // Use Opus code 3 padding without changing the stock encoded frame.
    let mut opus = vec![source[0] | 3, 0b0100_0001];
    let mut remaining = MAX_OPUS_INPUT - source.len() - 1;
    while remaining > 255 {
        opus.push(255);
        remaining -= 255;
    }
    opus.push(u8::try_from(remaining - 1).unwrap());
    opus.extend_from_slice(&source[1..]);
    opus.resize(MAX_OPUS_INPUT, 0);
    opus
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn stock_opus_decodes_across_capped_whip_and_whep_udp() {
    use crate::rtp::fabric_egress::{CaptureClock, EndpointHints, OutputLeg, Vad, timeline};
    use crate::stock_rtc::{LoopbackMedia, NativeMediaEndpoint, assert_decoded_stock_opus};
    use pulsebeam_core::fabric::Profile;

    const PUBLISHER_WIRE_BYTES: u16 = 1_236;
    let opus = maximum_stock_opus_packet();
    let (mut publisher, config, mid, primary) =
        NativePublisherUdp::connect_with_cap(LoopbackMedia::OpusPublisher, PUBLISHER_WIRE_BYTES)
            .await;
    let mut endpoint = NativeParticipantUdp::connect_with_cap(
        LoopbackMedia::Opus,
        Some(config),
        PUBLISHER_WIRE_BYTES,
    )
    .await;
    let mut binding = None;
    publisher.native.push_opus(opus.clone(), 960);
    for _ in 0..200 {
        publisher
            .step(
                endpoint.now,
                endpoint.shard.publisher(),
                &mut MockParticipantSink::new(),
                Some(&mut |packet: AuthenticatedRtp<'_>, _| {
                    assert_eq!(packet.primary_ssrc, primary.get());
                    let negotiated = packet.binding();
                    let form = if negotiated.allow_mixed_extensions {
                        ExtensionForm::Mixed
                    } else {
                        ExtensionForm::OneByte
                    };
                    binding = Some(
                        RtpIngressShadow::bind_whole(
                            Profile::Opus,
                            negotiated,
                            form,
                            &[],
                            Anchors::default(),
                            Limits {
                                positions: NonZeroU16::new(64).unwrap(),
                                max_age: Duration::from_millis(120),
                                max_idle: Duration::from_secs(2),
                            },
                        )
                        .expect("core-only Opus ingress must match the fixture emission"),
                    );
                }),
                |_| true,
            )
            .await;
        endpoint.step(NativeMediaEndpoint::deliver);
        if binding.is_some() {
            break;
        }
    }
    let (_, publication) = endpoint
        .shard
        .publisher()
        .upstream
        .slot_for_mid(mid)
        .unwrap();
    endpoint.shard.use_negotiated_publication(publication, None);
    assert!(
        endpoint
            .shard
            .attach_publisher_ingress(binding.expect("authenticated Opus ingress binding"))
    );
    assert!(
        endpoint.shard.bind_output(
            SenderLeg::new(
                OutputLeg {
                    mid: endpoint.mid,
                    primary: None,
                    max_wire_bytes: 1_216,
                    replace_queued: false,
                },
                EndpointHints::new(Profile::Opus, endpoint.form, &[], None).unwrap(),
                timeline::Timeline::new(500, 1500, 48_000, 64).unwrap(),
            )
            .with_source_wire_budget(),
            SourceMetadata {
                vad: Vad::On,
                capture: CaptureClock::Unmapped,
                same_topology: true,
            },
        )
    );
    let endpoint_original_ssrc = endpoint.udp.primary;
    let mut input = None;
    let mut output = None;
    let mut audio = None;
    let mut offered = 0;
    for _ in 0..20 {
        publisher
            .step(
                endpoint.now,
                endpoint.shard.publisher(),
                &mut MockParticipantSink::new(),
                None,
                |_| true,
            )
            .await;
        endpoint.step(NativeMediaEndpoint::deliver);
    }
    publisher.native.push_opus(opus.clone(), 1920);
    for _ in 0..400 {
        let (owner, mut sink) = endpoint.shard.publisher_with_sink();
        publisher
            .step(endpoint.now, owner, &mut sink, None, |bytes| {
                if let Some((ssrc, _)) = fixture_rtp(bytes) {
                    assert_eq!(ssrc, primary.get());
                    assert!(input.replace(bytes.len()).is_none());
                }
                true
            })
            .await;
        offered += endpoint.shard.flush_publisher_media(endpoint.now);
        endpoint.step(|native, bytes| {
            if let Some((ssrc, sequence)) = fixture_rtp(bytes) {
                assert_eq!(ssrc, endpoint_original_ssrc);
                assert_eq!(sequence, 500);
                assert!(output.replace(bytes.len()).is_none());
            }
            native.deliver(bytes);
        });
        if let Some(frame) = endpoint.native.try_next_audio() {
            audio = Some(frame);
            break;
        }
    }
    let audio = audio.unwrap_or_else(|| {
        let recv = endpoint.shard.receiver().transport.connection_stats();
        panic!("stock Opus did not reach fresh decode: publisher={input:?} Fabric offers={offered} receiver={output:?} receiver RTC={recv:?}");
    });
    assert_eq!(input, Some(usize::from(PUBLISHER_WIRE_BYTES)));
    assert!(
        (1_217..=input.unwrap()).contains(&output.unwrap()),
        "Opus delivery must exceed the old cap without primary growth"
    );
    assert_eq!(endpoint.udp.received_primary, 1);
    assert_eq!(audio.ssrc, endpoint.udp.primary);
    assert_eq!(audio.rtp_timestamp, 1500);
    assert_eq!(audio.data, opus);
    assert_decoded_stock_opus(audio.data, audio.rtp_timestamp);
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn stock_h264_repairs_and_refreshes_on_capped_udp_without_primary_growth() {
    prove_stock_video_on_capped_udp(pulsebeam_core::fabric::Profile::H264, false).await;
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn stock_h264_crosses_shard_mailbox_and_repairs_subscriber_loss() {
    prove_stock_video_on_capped_udp(pulsebeam_core::fabric::Profile::H264, true).await;
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn stock_av1_repairs_and_refreshes_on_capped_udp_without_primary_growth() {
    prove_stock_video_on_capped_udp(pulsebeam_core::fabric::Profile::Av1, false).await;
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn stock_av1_crosses_shard_mailbox_and_decodes_without_primary_growth() {
    prove_stock_video_on_capped_udp(pulsebeam_core::fabric::Profile::Av1, true).await;
}

#[cfg(feature = "transport-conformance")]
async fn prove_stock_video_on_capped_udp(
    profile: pulsebeam_core::fabric::Profile,
    cross_shard: bool,
) {
    use crate::rtp::fabric_egress::{CaptureClock, EndpointHints, OutputLeg, Vad, timeline};
    use crate::stock_rtc::{
        LoopbackMedia, NativeMediaEndpoint, assert_decoded_stock_av1_keyframe,
        assert_decoded_stock_keyframe,
    };
    use pulsebeam_core::fabric::Profile;

    type Push = fn(&NativeMediaEndpoint);
    type Decode = fn(&pulsebeam_rtc_simulation::EncodedReceivedVideoFrame);
    let (publisher_media, receiver_media, push, decode): (
        LoopbackMedia,
        LoopbackMedia,
        Push,
        Decode,
    ) = match profile {
        Profile::H264 => (
            LoopbackMedia::H264Publisher,
            LoopbackMedia::H264,
            NativeMediaEndpoint::push_stock_h264,
            assert_decoded_stock_keyframe,
        ),
        Profile::Av1 => (
            LoopbackMedia::Av1Publisher,
            LoopbackMedia::Av1,
            NativeMediaEndpoint::push_stock_av1,
            assert_decoded_stock_av1_keyframe,
        ),
        _ => panic!("stock video test requires a video profile"),
    };
    let (mut publisher, config, mid, primary) = NativePublisherUdp::connect(publisher_media).await;
    let mut endpoint = NativeParticipantUdp::connect_with_topology(
        receiver_media,
        Some(config),
        1_216,
        cross_shard,
    )
    .await;
    assert!(publisher.native.dd_id.is_none() && endpoint.native.dd_id.is_none());
    publisher.ingress_shard = endpoint.shard.publisher_shard();
    let mut binding = None;
    let mut publisher_repair = None;
    let mut initialized = false;
    let mut probe_wire_rtp = 0;
    let mut probe_auth_rtp = 0;
    let mut probe_payloads = Vec::new();
    push(&publisher.native);
    for _ in 0..200 {
        publisher
            .step(
                endpoint.now,
                endpoint.shard.publisher(),
                &mut MockParticipantSink::new(),
                Some(&mut |packet: AuthenticatedRtp<'_>, _| {
                    probe_auth_rtp += 1;
                    assert_eq!(packet.primary_ssrc, primary.get());
                    let repair = packet
                        .declared_repair_ssrc
                        .expect("stock publisher RTX FID");
                    assert!(packet.rid.is_none());
                    assert_eq!(*publisher_repair.get_or_insert(repair), repair);
                    if binding.is_none() {
                        let form = if packet.allow_mixed_extensions {
                            ExtensionForm::Mixed
                        } else if matches!(packet.extension_profile(), Some(0x1000..=0x100f)) {
                            ExtensionForm::TwoByte
                        } else {
                            ExtensionForm::OneByte
                        };
                        binding = Some(
                            RtpIngressShadow::bind_whole(
                                profile,
                                packet.binding(),
                                form,
                                &[],
                                Anchors::default(),
                                Limits {
                                    positions: NonZeroU16::new(64).unwrap(),
                                    max_age: Duration::from_millis(120),
                                    max_idle: Duration::from_secs(2),
                                },
                            )
                            .unwrap(),
                        );
                    }
                    if cross_shard && !packet.is_repair && !packet.wire_payload().is_empty() {
                        probe_payloads.push(packet.wire_payload().to_vec());
                    }
                    initialized |= packet.wire_header().marker;
                }),
                |bytes| {
                    probe_wire_rtp += usize::from(fixture_rtp(bytes).is_some());
                    true
                },
            )
            .await;
        endpoint.step(crate::stock_rtc::NativeMediaEndpoint::deliver);
        if initialized {
            break;
        }
    }
    assert!(
        initialized,
        "stock source completes the binding probe frame: {probe_wire_rtp} UDP RTP, {probe_auth_rtp} authenticated"
    );
    let (_, publication) = endpoint
        .shard
        .publisher()
        .upstream
        .slot_for_mid(mid)
        .unwrap();
    endpoint
        .shard
        .use_negotiated_publication(publication, Some(primary));
    assert!(endpoint.shard.attach_publisher_ingress(binding.unwrap()));
    let fid = [endpoint.udp.primary, endpoint.udp.repair.unwrap()];
    assert!(
        endpoint.shard.bind_output(
            SenderLeg::new(
                OutputLeg {
                    mid: endpoint.mid,
                    primary: None,
                    max_wire_bytes: 1_216,
                    replace_queued: false,
                },
                EndpointHints::new(profile, endpoint.form, &[], None).unwrap(),
                timeline::Timeline::new(500, 1500, 90_000, 64).unwrap(),
            )
            .with_answer_fid(fid),
            SourceMetadata {
                vad: Vad::On,
                capture: CaptureClock::Unmapped,
                same_topology: true
            },
        )
    );
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Recovery {
        Direct,
        PublisherLoss,
        SubscriberLoss,
        Refresh,
    }
    let mut first_output = 500_u16;
    let mut previous_timestamp = None;
    let cases: &[_] = if cross_shard && profile == Profile::H264 {
        &[Recovery::SubscriberLoss]
    } else if cross_shard {
        &[Recovery::Direct]
    } else {
        &[
            Recovery::PublisherLoss,
            Recovery::SubscriberLoss,
            Recovery::Refresh,
        ]
    };
    for &recovery in cases {
        publisher.feedback_peak = 0;
        endpoint.feedback_peak = 0;
        if recovery == Recovery::Refresh {
            prove_native_video_refresh(&mut publisher, &mut endpoint, primary.get()).await;
        }
        let mut input = RepairWitness::new(
            [primary.get(), publisher_repair.unwrap()],
            recovery == Recovery::PublisherLoss,
        );
        let mut output = RepairWitness::new(fid, recovery == Recovery::SubscriberLoss);
        let mut crossed_payloads = Vec::new();
        push(&publisher.native);
        let mut decoded = None;
        for _ in 0..1_000 {
            let (owner, mut sink) = endpoint.shard.publisher_with_sink();
            publisher
                .step(endpoint.now, owner, &mut sink, None, |bytes| {
                    input.receive(bytes)
                })
                .await;
            endpoint.shard.flush_publisher_media(endpoint.now);
            if cross_shard {
                endpoint.shard.deliver_shard_fabric(endpoint.now, |packet| {
                    let header = packet.header();
                    assert_eq!(header.profile, profile);
                    crossed_payloads.push((
                        header.sequence,
                        packet.payload().to_vec(),
                        packet.source_protected_bytes(),
                    ));
                });
            }
            endpoint.step(|native, bytes| {
                if output.receive(bytes) {
                    native.deliver(bytes);
                }
            });
            if !cross_shard {
                endpoint
                    .shard
                    .flush_receiver_feedback_with_clock(|| endpoint.now);
            }
            if let Some(frame) = endpoint.native.try_next_video() {
                decoded = Some(frame);
                break;
            }
        }
        let frame = decoded.unwrap_or_else(|| {
            let input_rtc = endpoint.shard.publisher().transport.connection_stats();
            let output_rtc = endpoint.shard.receiver().transport.connection_stats();
            panic!(
                "no reassembled {profile:?} after publisher/participant UDP: {recovery:?}, input={input:?}, output={output:?}, input_rtc={input_rtc:?}, output_rtc={output_rtc:?}, shard packets={}, peak feedback={}/{}",
                crossed_payloads.len(), publisher.feedback_peak, endpoint.feedback_peak,
            );
        });
        assert_ne!(previous_timestamp, Some(frame.rtp_timestamp));
        previous_timestamp = Some(frame.rtp_timestamp);
        let first_input = input.first.unwrap();
        assert_eq!(output.first, Some(first_output));
        assert!(
            input.primary_sizes.len() >= 3
                && input.primary_sizes.values().any(|bytes| *bytes >= 1_100)
        );
        assert_eq!(input.primary_sizes.len(), output.primary_sizes.len());
        for (sequence, input_len) in &input.primary_sizes {
            let projected = first_output.wrapping_add(sequence.wrapping_sub(first_input));
            assert!(output.primary_sizes[&projected] <= *input_len);
        }
        first_output = first_output.wrapping_add(u16::try_from(input.primary_sizes.len()).unwrap());
        if cross_shard {
            assert_eq!(crossed_payloads.len(), input.primary_sizes.len());
            for ((sequence, payload, protected), expected_payload) in
                crossed_payloads.iter().zip(&probe_payloads)
            {
                let sequence = u16::try_from(sequence & u32::from(u16::MAX)).unwrap();
                assert_eq!(payload, expected_payload);
                assert_eq!(
                    usize::from(protected.unwrap().get()),
                    input.primary_sizes[&sequence]
                );
            }
            assert_eq!(crossed_payloads.len(), probe_payloads.len());
        }
        let repair = match recovery {
            Recovery::PublisherLoss => Some((&input, publisher.feedback_peak)),
            Recovery::SubscriberLoss => Some((&output, endpoint.feedback_peak)),
            Recovery::Direct | Recovery::Refresh => None,
        };
        if let Some((witness, feedback)) = repair {
            let (_, lost_size) = witness
                .lost
                .expect("large interior primary deliberately dropped");
            assert!((1_100..=1_216).contains(&witness.repair_peak));
            assert!((1..=1_216).contains(&feedback));
            if recovery == Recovery::SubscriberLoss {
                assert_eq!(
                    witness.repair_peak,
                    lost_size + 2,
                    "standard RTX adds its two-byte OSN"
                );
            }
        }
        decode(&frame);
    }
}

#[cfg(feature = "transport-conformance")]
async fn prove_native_video_refresh(
    publisher: &mut NativePublisherUdp,
    endpoint: &mut NativeParticipantUdp,
    primary: u32,
) {
    async fn pump(publisher: &mut NativePublisherUdp, endpoint: &mut NativeParticipantUdp) {
        endpoint.step(crate::stock_rtc::NativeMediaEndpoint::deliver);
        endpoint
            .shard
            .flush_receiver_feedback_with_clock(|| endpoint.now);
        let (owner, mut sink) = endpoint.shard.publisher_with_sink();
        publisher
            .step(endpoint.now, owner, &mut sink, None, |_| true)
            .await;
    }

    // Drain any feedback from the loss exercises and allow the refresh debounce to expire.
    let quiet_until = endpoint
        .now
        .checked_add(crate::track::KEYFRAME_DEBOUNCE)
        .unwrap();
    while endpoint.now <= quiet_until {
        pump(publisher, endpoint).await;
    }
    let before = publisher.native.publisher_pli_count(primary);
    let subscriber_primary = endpoint.udp.primary;
    let reports_before = endpoint
        .native
        .subscriber_refresh_counts(subscriber_primary);
    publisher.feedback_peak = 0;
    endpoint.feedback_peak = 0;
    endpoint.native.request_video_refresh();
    for tick in 1..=1_000 {
        pump(publisher, endpoint).await;
        if tick % 50 == 0
            && publisher.native.publisher_pli_count(primary) > before
            && endpoint
                .native
                .subscriber_refresh_counts(subscriber_primary)
                != reports_before
        {
            assert!((1..=1_216).contains(&publisher.feedback_peak));
            assert!((1..=1_216).contains(&endpoint.feedback_peak));
            return;
        }
    }
    panic!(
        "stock refresh not confirmed: subscriber PLI/FIR {reports_before:?} -> {:?}, publisher PLI {before} -> {}, protected feedback peaks subscriber={} publisher={}",
        endpoint
            .native
            .subscriber_refresh_counts(subscriber_primary),
        publisher.native.publisher_pli_count(primary),
        endpoint.feedback_peak,
        publisher.feedback_peak,
    );
}

#[cfg(feature = "transport-conformance")]
#[derive(Debug)]
struct RepairWitness {
    fid: [u32; 2],
    drop_primary: bool,
    first: Option<u16>,
    primary_sizes: std::collections::BTreeMap<u16, usize>,
    lost: Option<(u16, usize)>,
    repair_peak: usize,
}

#[cfg(feature = "transport-conformance")]
impl RepairWitness {
    fn new(fid: [u32; 2], drop_primary: bool) -> Self {
        Self {
            fid,
            drop_primary,
            first: None,
            primary_sizes: Default::default(),
            lost: None,
            repair_peak: 0,
        }
    }

    fn receive(&mut self, bytes: &[u8]) -> bool {
        let Some((ssrc, sequence)) = fixture_rtp(bytes) else {
            return true;
        };
        if ssrc == self.fid[1] {
            // Receiver feedback may also request the publisher's temporarily
            // missing packet; only the selected loss must have a repair.
            self.repair_peak = self.repair_peak.max(bytes.len());
        } else {
            assert_eq!(ssrc, self.fid[0]);
            assert!(self.primary_sizes.insert(sequence, bytes.len()).is_none());
            let first = *self.first.get_or_insert(sequence);
            if self.drop_primary
                && self.lost.is_none()
                && sequence != first
                && bytes.len() >= 1_100
                && bytes[1] & 0x80 == 0
            {
                self.lost = Some((sequence, bytes.len()));
                return false;
            }
        }
        true
    }
}

#[cfg(feature = "transport-conformance")]
fn fixture_srtcp(bytes: &[u8]) -> bool {
    matches!(bytes, [first, 192..=223, ..] if first >> 6 == 2)
}

#[cfg(feature = "transport-conformance")]
fn fixture_rtp(bytes: &[u8]) -> Option<(u32, u16)> {
    let [first, pt, seq_hi, seq_lo, _, _, _, _, w, x, y, z, ..] = bytes else {
        return None;
    };
    (first >> 6 == 2 && !(64..=95).contains(&(pt & 127))).then_some((
        u32::from_be_bytes([*w, *x, *y, *z]),
        u16::from_be_bytes([*seq_hi, *seq_lo]),
    ))
}

#[cfg(feature = "transport-conformance")]
fn native_loopback_admission(
    local: SocketAddr,
    media: crate::stock_rtc::LoopbackMedia,
) -> (
    crate::stock_rtc::PeerFixture,
    pulsebeam_rtc::SdpAnswer,
    pulsebeam_rtc::SessionInfo,
    NegotiatedResources,
) {
    use crate::stock_rtc::LoopbackMedia;

    let av1 = matches!(media, LoopbackMedia::Av1 | LoopbackMedia::Av1Publisher);
    let mut negotiated = None;
    let stock = crate::stock_rtc::PeerFixture::bounded_media_with_loopback(
        local,
        media,
        |config, offer, now| {
            let capabilities = av1.then(|| {
                use crate::control::rtp::{bootstrap_capabilities, transport_capabilities};
                use pulsebeam_rtc::PacketFeedbackKind;
                let without_ccfb = offer.as_str().replace("ack ccfb", "invalid-feedback");
                assert_eq!(
                    transport_capabilities(&bootstrap_capabilities(&without_ccfb))
                        .unwrap()
                        .feedback,
                    PacketFeedbackKind::TransportWide,
                );
                let caps = transport_capabilities(&bootstrap_capabilities(offer.as_str())).unwrap();
                assert_eq!(caps.feedback, PacketFeedbackKind::Rfc8888);
                caps
            });
            let (accepted, _, resources) = crate::control::Negotiator::new(config.local_candidates)
                .create_answer(
                    offer.as_str().to_owned(),
                    crate::control::ufrag::test_credentials(),
                    net::ReceiveTime {
                        monotonic: now.into(),
                        wall: SystemTime::UNIX_EPOCH,
                    },
                    capabilities,
                )
                .unwrap();
            negotiated = Some((accepted.answer.clone(), accepted.session.clone(), resources));
            accepted
        },
    );
    let (answer, session, resources) = negotiated.unwrap();
    (stock, answer, session, resources)
}

#[cfg(feature = "transport-conformance")]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn maximum_native_data_record_crosses_capped_dtls13_udp() {
    use crate::stock_rtc::{FixtureDataEvent, PeerFixture, maximum_data_record};
    use pulsebeam_rtc::{
        DataChannelEvent, DataMessage, Event, NetworkInput, Output, TransmitTarget,
    };
    use pulsebeam_rtc_simulation::{
        DataChannelEvent as NativeDataEvent, DataChannelMessage, DataChannelMessageKind,
        DataChannelSendResult,
    };

    let mut edge = bind_capped_fixture_udp(SocketAddr::from(([127, 0, 0, 1], 0))).await;
    let edge_addr = edge.local_addr();
    let mut stock = PeerFixture::bounded_data_with_loopback(edge_addr);
    let (mut channel, mut native_opened) = (None, false);
    while channel.is_none() || !native_opened {
        match stock.next_data_event() {
            FixtureDataEvent::ConnectionOpened(id) => channel = Some(id),
            FixtureDataEvent::PeerOpened => native_opened = true,
            _ => {}
        }
    }
    let channel = channel.unwrap();
    assert_eq!(
        stock.native_sctp_dtls_parameters(),
        (Some(0xfefc), Some(0x1302))
    );
    let mut now = stock.at();
    let (mut rtc, native, data) = stock.into_native_data_endpoint();
    let mut client = bind_capped_fixture_udp(native.remote).await;
    let client_addr = client.local_addr();

    // Qualify the complete envelope in both directions on the exact local pair.
    // These probes do not enter either DTLS association.
    let probe = [0x5a; 1_216];
    send_capped_fixture_udp(&mut edge, client_addr, &probe);
    send_capped_fixture_udp(&mut client, edge_addr, &probe);
    receive_capped_fixture_udp(&mut edge, client_addr, 1, |bytes| assert_eq!(bytes, probe)).await;
    receive_capped_fixture_udp(&mut client, edge_addr, 1, |bytes| assert_eq!(bytes, probe)).await;

    let record = maximum_data_record();
    assert_eq!(
        data.send(DataChannelMessage {
            kind: DataChannelMessageKind::Binary,
            bytes: record.to_vec(),
        }),
        DataChannelSendResult::Sent
    );
    rtc.send_data(
        now,
        channel,
        DataMessage::Binary(bytes::Bytes::copy_from_slice(&record)),
    )
    .unwrap();

    let (mut native_peak, mut edge_peak) = (0, 0);
    let (mut edge_received, mut native_received) = (false, false);
    for _ in 0..500 {
        now += Duration::from_millis(1);
        let mut count = 0;
        native.step(now, |bytes| {
            native_peak = native_peak.max(bytes.len());
            send_capped_fixture_udp(&mut client, edge_addr, bytes);
            count += 1;
        });
        receive_capped_fixture_udp(&mut edge, client_addr, count, |bytes| {
            rtc.receive(
                now,
                now,
                NetworkInput::Udp {
                    local: edge_addr,
                    remote: client_addr,
                    ecn: None,
                    payload: bytes::Bytes::copy_from_slice(bytes),
                },
            )
            .unwrap();
        })
        .await;
        let mut count = 0;
        loop {
            match rtc.poll(now) {
                Output::Transmit(transmit) => {
                    assert!(
                        matches!(transmit.target, TransmitTarget::Udp { local, remote, .. }
                        if local == edge_addr && remote == client_addr)
                    );
                    edge_peak = edge_peak.max(transmit.payload.len());
                    send_capped_fixture_udp(&mut edge, client_addr, &transmit.payload);
                    count += 1;
                }
                Output::Event(Event::DataChannel(DataChannelEvent::Message {
                    channel: received,
                    message: DataMessage::Binary(payload),
                })) => {
                    assert_eq!(received, channel);
                    assert_eq!(payload.as_ref(), record);
                    assert!(!std::mem::replace(&mut edge_received, true));
                }
                Output::Event(_) => {}
                Output::Idle { .. } => break,
                output => panic!("unexpected DATA transport output: {output:?}"),
            }
        }
        receive_capped_fixture_udp(&mut client, edge_addr, count, |bytes| native.deliver(bytes))
            .await;
        while let Some(event) = data.try_next_event() {
            if let NativeDataEvent::Message(message) = event {
                assert_eq!(message.kind, DataChannelMessageKind::Binary);
                assert_eq!(message.bytes, record);
                assert!(!std::mem::replace(&mut native_received, true));
            }
        }
        if edge_received && native_received {
            assert!(
                [native_peak, edge_peak]
                    .into_iter()
                    .all(|bytes| matches!(bytes, 1_206 | 1_210)),
                "maximum protected DATA writes on the qualified local pair: {native_peak}, {edge_peak}"
            );
            return;
        }
    }
    panic!("maximum DATA record did not arrive on both sides of the capped pair");
}

#[cfg(feature = "transport-conformance")]
struct NativePublisherUdp {
    local: pulsebeam_udp::UdpSocket,
    remote: pulsebeam_udp::UdpSocket,
    native: crate::stock_rtc::NativeMediaEndpoint,
    ingress_shard: ShardId,
    feedback_sent: usize,
    feedback_peak: usize,
}

#[cfg(feature = "transport-conformance")]
impl NativePublisherUdp {
    async fn connect(
        media: crate::stock_rtc::LoopbackMedia,
    ) -> (Self, ConnectionConfig, Mid, NonZeroU32) {
        Self::connect_with_cap(media, 1_216).await
    }

    async fn connect_with_cap(
        media: crate::stock_rtc::LoopbackMedia,
        cap: u16,
    ) -> (Self, ConnectionConfig, Mid, NonZeroU32) {
        let local = bind_fixture_udp(SocketAddr::from(([127, 0, 0, 1], 0)), cap).await;
        let (stock, answer, session, resources) =
            native_loopback_admission(local.local_addr(), media);
        let [media] = session.inbound_media.as_ref() else {
            panic!("one stock media source");
        };
        let mid = Mid::new(&media.mid).unwrap();
        let [primary] = media.primary_ssrcs.as_ref() else {
            panic!("one signaled source SSRC");
        };
        let ssrc = NonZeroU32::new(*primary).unwrap();
        assert!(matches!(
            stock.native_media_srtp_profile(mid.as_str()),
            Some(0x0007 | 0x0008)
        ));
        let (connection, native) = stock.into_native_media_endpoint();
        let remote = bind_fixture_udp(native.remote, cap).await;
        let config = ConnectionConfig {
            scope: entity::ProjectRoomKey::fixture(entity::RoomId::new("fabric-shadow").unwrap())
                .into(),
            participant_id: entity::ParticipantId::new("native-publisher").unwrap(),
            connection_id: entity::ConnectionId::new(),
            profile: SignalingProtocol::Whip,
            authorization_expiry: None,
            rtc: pulsebeam_rtc::AcceptedConnection {
                connection,
                answer,
                session,
            },
            resources,
        };
        (
            Self {
                local,
                remote,
                native,
                ingress_shard: ShardId::new(0),
                feedback_sent: 0,
                feedback_peak: 0,
            },
            config,
            mid,
            ssrc,
        )
    }

    async fn step(
        &mut self,
        now: Instant,
        publisher: &mut Connection,
        sink: &mut impl ParticipantSink,
        mut observer: Option<&mut super::super::transport::AuthenticatedIngressObserver<'_>>,
        mut receive: impl FnMut(&[u8]) -> bool,
    ) {
        publisher.advance_rtc_clock(now, now);
        let local_addr = self.local.local_addr();
        let remote_addr = self.remote.local_addr();
        let mut sent = 0;
        self.native.step(now.into(), |bytes| {
            send_capped_fixture_udp(&mut self.remote, local_addr, bytes);
            sent += 1;
        });
        receive_capped_fixture_udp(&mut self.local, remote_addr, sent, |bytes| {
            if !receive(bytes) {
                return;
            }
            publisher.transport.enqueue_ingress(
                net::RecvPacketBatch {
                    src: remote_addr,
                    dst: local_addr,
                    len: bytes.len(),
                    stride: bytes.len(),
                    buf: bytes.to_vec(),
                    transport: net::Transport::Udp(net::UdpMode::Scalar),
                    received_at: net::ReceiveTime {
                        monotonic: now,
                        wall: SystemTime::UNIX_EPOCH,
                    },
                    offset: 0,
                },
                self.ingress_shard,
            );
        })
        .await;
        for _ in 0..64 {
            let ingress = publisher.transport.receive_pending(now);
            for _ in 0..64 {
                let output = if let Some(observer) = observer.as_deref_mut() {
                    publisher.transport.poll_output(Some(observer))
                } else {
                    publisher.poll_transport(now, sink)
                };
                match output {
                    TransportPollOutput::Event(event) => publisher.handle_event(now, *event, sink),
                    TransportPollOutput::Transmit => publisher.drain_network(self),
                    TransportPollOutput::Timeout(_) => break,
                    TransportPollOutput::Pending => {}
                    TransportPollOutput::Closed(reason) => {
                        panic!("publisher RTC closed: {reason:?}")
                    }
                }
            }
            if !publisher.transport.has_pending_ingress() && matches!(ingress, IngressResult::Empty)
            {
                break;
            }
        }
        let sent = std::mem::take(&mut self.feedback_sent);
        receive_capped_fixture_udp(&mut self.remote, local_addr, sent, |bytes| {
            if fixture_srtcp(bytes) {
                self.feedback_peak = self.feedback_peak.max(bytes.len());
            }
            self.native.deliver(bytes);
        })
        .await;
    }
}

#[cfg(feature = "transport-conformance")]
impl NetworkEgress for NativePublisherUdp {
    fn append_udp(&mut self, packets: &mut OwnedPacketQueue) -> AppendStatus {
        while let Some((destination, bytes)) = packets.pop_packet() {
            assert_eq!(destination, self.remote.local_addr());
            send_capped_fixture_udp(&mut self.local, destination, &bytes);
            self.feedback_sent += 1;
        }
        AppendStatus::Drained
    }

    fn flush(&mut self) -> bool {
        true
    }
}

#[cfg(feature = "transport-conformance")]
fn send_capped_fixture_udp(
    socket: &mut pulsebeam_udp::UdpSocket,
    destination: SocketAddr,
    payload: &[u8],
) {
    let report = socket.try_send_batch(&[pulsebeam_udp::SendDatagram {
        destination,
        payload,
        gso_segment_size: None,
    }]);
    assert_eq!(report.accepted, 1, "capped loopback UDP send: {report:?}");
}

#[cfg(feature = "transport-conformance")]
async fn receive_capped_fixture_udp(
    socket: &mut pulsebeam_udp::UdpSocket,
    source: SocketAddr,
    expected: usize,
    mut receive: impl FnMut(&[u8]),
) {
    let destination = socket.local_addr();
    let cap = usize::from(socket.nonfragmenting_max_datagram_bytes().unwrap().get());
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), async {
        let mut count = 0;
        while count < expected {
            socket.readable().await.unwrap();
            match socket.try_recv_batch() {
                Ok(batch) => {
                    for datagram in batch.iter() {
                        assert_eq!(
                            (datagram.source, datagram.destination),
                            (source, destination)
                        );
                        assert!((1..=cap).contains(&datagram.payload.len()));
                        receive(datagram.payload);
                        count += 1;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("capped loopback UDP receive: {error}"),
            }
        }
        assert_eq!(count, expected);
    })
    .await
    .expect("loopback socket receives every accepted datagram");
}

#[cfg(feature = "transport-conformance")]
async fn bind_capped_fixture_udp(addr: SocketAddr) -> pulsebeam_udp::UdpSocket {
    bind_fixture_udp(addr, 1_216).await
}

#[cfg(feature = "transport-conformance")]
async fn bind_fixture_udp(addr: SocketAddr, cap: u16) -> pulsebeam_udp::UdpSocket {
    let mut options = pulsebeam_udp::BindOptions::new(addr);
    options.nonfragmenting_max_datagram_bytes = NonZeroU16::new(cap);
    pulsebeam_udp::bind(options)
        .await
        .unwrap()
        .activate()
        .unwrap()
}

#[cfg(feature = "transport-conformance")]
struct NativeParticipantUdp {
    shard: crate::shard::core::FabricRtpFixture,
    native: crate::stock_rtc::NativeMediaEndpoint,
    udp: ScopedTestUdp,
    now: Instant,
    mid: Mid,
    form: ExtensionForm,
    feedback_peak: usize,
}

#[cfg(feature = "transport-conformance")]
impl NativeParticipantUdp {
    async fn connect(media: crate::stock_rtc::LoopbackMedia) -> Self {
        Self::connect_with_publisher(media, None).await
    }

    async fn connect_with_publisher(
        media: crate::stock_rtc::LoopbackMedia,
        publisher: Option<ConnectionConfig>,
    ) -> Self {
        Self::connect_with_cap(media, publisher, 1_216).await
    }

    async fn connect_with_cap(
        media: crate::stock_rtc::LoopbackMedia,
        publisher: Option<ConnectionConfig>,
        cap: u16,
    ) -> Self {
        Self::connect_with_topology(media, publisher, cap, false).await
    }

    async fn connect_with_topology(
        media: crate::stock_rtc::LoopbackMedia,
        publisher: Option<ConnectionConfig>,
        cap: u16,
        cross_shard: bool,
    ) -> Self {
        use crate::shard::core::FabricRtpFixture;
        use crate::stock_rtc::LoopbackMedia;
        use pulsebeam_core::{dd::evidence::DecodeTarget, fabric::Profile};
        use str0m::media::{Direction, MediaKind};
        let (profile, kind, target) = match media {
            LoopbackMedia::Opus => (Profile::Opus, MediaKind::Audio, None),
            LoopbackMedia::H264 => (Profile::H264, MediaKind::Video, None),
            LoopbackMedia::FilteredH264 => (Profile::H264, MediaKind::Video, DecodeTarget::new(0)),
            LoopbackMedia::Av1 => (Profile::Av1, MediaKind::Video, None),
            LoopbackMedia::OpusPublisher
            | LoopbackMedia::H264Publisher
            | LoopbackMedia::Av1Publisher => {
                panic!("the participant UDP endpoint must receive")
            }
        };
        let local = bind_fixture_udp(SocketAddr::from(([127, 0, 0, 1], 0)), cap).await;
        let local_addr = local.local_addr();
        let (stock, answer, session, resources) = native_loopback_admission(local_addr, media);
        let mid = Mid::new(&stock.sender_mid).unwrap();
        let (primary, repair) = if matches!(media, LoopbackMedia::Opus) {
            (stock.connection.stats().senders[0].ssrc, None)
        } else {
            let [primary, repair] = stock.installed_answer_fid(mid.as_str());
            (primary, Some(repair))
        };
        assert!(matches!(
            stock.native_media_srtp_profile(mid.as_str()),
            Some(0x0007 | 0x0008)
        ));
        let form = match stock
            .connection
            .rtp_egress_extension_form(mid.as_str(), None)
            .unwrap()
        {
            pulsebeam_rtc::RtpExtensionForm::OneByte => ExtensionForm::OneByte,
            pulsebeam_rtc::RtpExtensionForm::TwoByte => ExtensionForm::TwoByte,
            pulsebeam_rtc::RtpExtensionForm::Mixed => ExtensionForm::Mixed,
        };
        let mut now = Instant::from(stock.at());
        let (connection, native) = stock.into_native_media_endpoint();
        // The controlled peer uses this exact tuple. Bridge only UDP bytes received
        // by the kernel, without emitting new RTP or changing the SRTP association.
        let remote = std::net::UdpSocket::bind(native.remote).unwrap();
        remote.set_nonblocking(true).unwrap();
        let udp = ScopedTestUdp {
            local,
            remote,
            primary,
            repair,
            received_primary: 0,
            peak: 0,
        };
        let (publisher_config, control_peer) = if let Some(config) = publisher {
            (config, None)
        } else {
            let (peer, config, _, _, _) = media_pair(kind, Direction::SendOnly, true);
            (config, Some(peer))
        };
        let receiver_config = ConnectionConfig {
            scope: entity::ProjectRoomKey::fixture(entity::RoomId::new("fabric-shadow").unwrap())
                .into(),
            participant_id: entity::ParticipantId::new("native-receiver").unwrap(),
            connection_id: entity::ConnectionId::new(),
            profile: SignalingProtocol::Whep,
            authorization_expiry: None,
            rtc: pulsebeam_rtc::AcceptedConnection {
                connection,
                answer,
                session,
            },
            resources,
        };
        let mut shard = if cross_shard {
            assert!(target.is_none() && control_peer.is_none());
            FabricRtpFixture::across_shards(publisher_config, receiver_config, profile, now)
        } else {
            FabricRtpFixture::with_selection(
                publisher_config,
                receiver_config,
                profile,
                None,
                target,
            )
        };
        if let Some(mut peer) = control_peer {
            peer.connect(shard.publisher());
            now = now.max(peer.clock.monotonic);
        }
        Self {
            shard,
            native,
            udp,
            now,
            mid,
            form,
            feedback_peak: 0,
        }
    }

    fn step(&mut self, mut deliver: impl FnMut(&crate::stock_rtc::NativeMediaEndpoint, &[u8])) {
        let clock = net::ReceiveTime {
            monotonic: self.now,
            wall: SystemTime::UNIX_EPOCH,
        };
        let (receiver, mut sink) = self.shard.receiver_with_sink();
        receiver.advance_rtc_clock(self.now, self.now);
        self.native.step(self.now.into(), |bytes| {
            if fixture_srtcp(bytes) {
                self.feedback_peak = self.feedback_peak.max(bytes.len());
            }
            receiver.transport.enqueue_ingress(
                net::RecvPacketBatch {
                    src: self.native.remote,
                    dst: self.udp.local.local_addr(),
                    len: bytes.len(),
                    stride: bytes.len(),
                    buf: bytes.to_vec(),
                    transport: net::Transport::Udp(net::UdpMode::Scalar),
                    received_at: clock,
                    offset: 0,
                },
                ShardId::new(0),
            );
        });
        let _ = receiver.transport.receive_pending(self.now);
        for _ in 0..64 {
            match receiver.poll_transport(self.now, &mut sink) {
                TransportPollOutput::Event(e) => receiver.handle_event(self.now, *e, &mut sink),
                TransportPollOutput::Closed(reason) => panic!("Native receiver closed: {reason:?}"),
                TransportPollOutput::Transmit
                | TransportPollOutput::Pending
                | TransportPollOutput::Timeout(_) => break,
            }
        }
        self.shard.submit_receiver(|offered| {
            self.udp
                .deliver(offered, |bytes| deliver(&self.native, bytes))
        });
        self.now = self.now.checked_add(Duration::from_millis(1)).unwrap();
    }
}

#[cfg(feature = "transport-conformance")]
async fn decode_stock_h264_on_participant_udp(filtered: bool) {
    use crate::rtp::fabric_egress::{
        CaptureClock, EndpointHints, Mapping, OutputLeg, Vad, timeline,
    };
    use crate::stock_rtc::LoopbackMedia;
    use pulsebeam_core::dd::{
        self, DependencyDescriptorReader, DependencyDescriptorWriter, RenderResolution,
    };
    use pulsebeam_core::fabric::{Extension, ExtensionKind, FabricPacket, Header, Profile};

    const WIRE_CAP: usize = 1_216;
    const RTP_HEADER: usize = 12;
    const GCM_TAG: usize = 16;
    const RTX_OSN: usize = 2;
    const PAYLOAD_CAP: usize = WIRE_CAP - RTP_HEADER - GCM_TAG - RTX_OSN;
    let packets: Vec<(Box<[u8]>, bool)> = if filtered {
        let mut source = crate::stock_rtc::PeerFixture::connected_video_with_rfc8888();
        assert!(matches!(
            source.native_media_srtp_profile(&source.sender_mid),
            Some(0x0007 | 0x0008)
        ));
        source
            .send_stock_h264_keyframe_with_sizes()
            .into_iter()
            .map(|(rtp, _)| (rtp.payload.as_ref().into(), rtp.header.marker))
            .collect()
    } else {
        // Repacketize the stock encoder's unchanged access unit rather than
        // adding bytes to make the Native packetizer fill the writer's budget.
        let world = pulsebeam_rtc_simulation::SimulationWorld::acquire();
        let unit = crate::stock_rtc::stock_h264_keyframe_720p(&world, false);
        pulsebeam_core::h264::Packetizer::new(PAYLOAD_CAP)
            .packetize(&unit.data)
            .into_iter()
            .map(|chunk| (chunk.payload.into_boxed_slice(), chunk.end_of_frame))
            .collect()
    };
    let media = if filtered {
        LoopbackMedia::FilteredH264
    } else {
        LoopbackMedia::H264
    };
    let mut endpoint = NativeParticipantUdp::connect(media).await;
    let fid = [endpoint.udp.primary, endpoint.udp.repair.unwrap()];
    let mid = endpoint.mid;
    let form = endpoint.form;
    const FIRST_OUTPUT_SEQUENCE: u16 = 500;
    assert_eq!(endpoint.native.dd_id.is_some(), filtered);
    let mapping = endpoint.native.dd_id.map(|id| Mapping {
        id,
        uri: dd::URI,
        attributes: None,
    });
    let leg = SenderLeg::new(
        OutputLeg {
            mid,
            primary: None,
            max_wire_bytes: WIRE_CAP,
            replace_queued: false,
        },
        EndpointHints::new(Profile::H264, form, mapping.as_slice(), None).unwrap(),
        timeline::Timeline::new(FIRST_OUTPUT_SEQUENCE, 1500, 90_000, 64).unwrap(),
    );
    let mut leg = if filtered {
        leg.with_dependency_descriptors(0)
    } else {
        leg.with_answer_fid(fid)
    };
    let source = SourceMetadata {
        vad: Vad::On,
        capture: CaptureClock::Unmapped,
        same_topology: true,
    };
    let mut sequence = 0_u32;
    let mut first_frame = None;
    let lost = (1..packets.len() - 1)
        .find(|&index| filtered || packets[index].0.len() == PAYLOAD_CAP)
        .expect("an interior H.264 fragment to repair");
    let mut dropped = false;
    let mut repaired = false;
    let mut step = |endpoint: &mut NativeParticipantUdp| {
        endpoint.step(|native, bytes| {
            if let [first, pt, a, b, _, _, _, _, w, x, y, z, ..] = bytes
                && first >> 6 == 2
                && !(64..=95).contains(&(pt & 127))
            {
                if !filtered {
                    assert_eq!(*first, 0x80, "no output RTP header extensions or padding");
                }
                let ssrc = u32::from_be_bytes([*w, *x, *y, *z]);
                let sequence = u16::from_be_bytes([*a, *b]);
                if ssrc == fid[0] {
                    let index = usize::from(sequence - FIRST_OUTPUT_SEQUENCE);
                    if !filtered {
                        assert_eq!(bytes.len(), RTP_HEADER + packets[index].0.len() + GCM_TAG);
                    }
                    if index == lost {
                        assert!(!dropped, "repair cannot be a second primary");
                        dropped = true;
                        return;
                    }
                } else {
                    assert_eq!(ssrc, fid[1]);
                    assert!(dropped);
                    if !filtered {
                        assert_eq!(bytes.len(), WIRE_CAP, "full-size protected RTX");
                    }
                    repaired = true;
                }
            }
            native.deliver(bytes);
        });
    };
    let frames = if filtered { 2 } else { 1 };
    for (input_frame, timestamp) in [(0, 90_000), (1, 99_000)].into_iter().take(frames) {
        let mut descriptor = DependencyDescriptorReader::new()
            .read(&[0xc0, 0, 0, 0x80, 0, 0x3b, 0x40, 0])
            .unwrap();
        descriptor.frame_number = input_frame;
        if input_frame == 1 {
            let resolutions = &mut descriptor.attached_structure.as_mut().unwrap().resolutions;
            resolutions.clear();
            resolutions.push(RenderResolution {
                width: 1_280,
                height: 720,
            });
        }
        let mut writer = DependencyDescriptorWriter::new();
        let mut encoded = [0; dd::MAX_DD_LEN];
        for (index, (payload, marker)) in packets.iter().enumerate() {
            let dd = if filtered {
                descriptor.start_of_frame = index == 0;
                descriptor.end_of_frame = *marker;
                let len = writer.write(&descriptor, &mut encoded).unwrap();
                descriptor.attached_structure = None;
                Some(Extension {
                    kind: ExtensionKind::DependencyDescriptor,
                    value: &encoded[..len],
                })
            } else {
                None
            };
            let packet = FabricPacket::new(
                Header {
                    profile: Profile::H264,
                    marker: *marker,
                    sequence,
                    timestamp,
                    ingress_ntp32: 0,
                },
                dd.as_slice(),
                payload.clone(),
            )
            .unwrap();
            endpoint
                .shard
                .offer(endpoint.now, endpoint.now, u64::from(sequence), packet);
            endpoint.shard.write(endpoint.now, &mut leg, source);
            sequence += 1;
            for _ in 0..200 {
                step(&mut endpoint);
                if endpoint.udp.received_primary >= usize::try_from(sequence).unwrap() {
                    break;
                }
            }
            assert_eq!(
                endpoint.udp.received_primary,
                usize::try_from(sequence).unwrap()
            );
        }
        let frame = (0..1_000)
            .find_map(|_| {
                step(&mut endpoint);
                endpoint.native.try_next_video()
            })
            .expect("Native reassembled the participant's original protected RTP");
        if filtered {
            let id = frame.frame_id.expect("Native parsed the projected DD");
            if let Some(first) = first_frame {
                assert_ne!(id, first);
                crate::stock_rtc::assert_decoded_stock_keyframe(&frame);
            } else {
                first_frame = Some(id);
            }
        } else {
            crate::stock_rtc::assert_decoded_stock_keyframe(&frame);
        }
    }
    assert!(
        dropped && repaired,
        "Native decoded after a lost primary was repaired on its installed FID"
    );
    assert!((1_000..=WIRE_CAP).contains(&endpoint.udp.peak));
}

fn prove_filtered_dd_switch(
    (mut rx, receiver_config, mid, rx_mid, _): (Pair, ConnectionConfig, Mid, str0m::media::Mid, u8),
) {
    use crate::rtp::fabric_egress::{
        AdmissionError, CaptureClock, EndpointHints, Mapping, OutputLeg, SenderLeg, SourceMetadata,
        Vad, timeline,
    };
    use crate::shard::core::FabricRtpFixture;
    use pulsebeam_core::dd::{
        self, DependencyDescriptorReader, DependencyDescriptorWriter, RawDependencyDescriptor,
        RenderResolution, evidence::DecodeTarget,
    };
    use pulsebeam_core::fabric::{Extension, ExtensionKind, FabricPacket, Header, Profile};
    use str0m::media::{Direction, KeyframeRequestKind, MediaKind};

    let (mut tx, publisher_config, _, _, _) =
        media_pair(MediaKind::Video, Direction::SendOnly, true);
    let mut shard = FabricRtpFixture::with_selection(
        publisher_config,
        receiver_config,
        Profile::H264,
        None,
        DecodeTarget::new(0),
    );
    tx.connect(shard.publisher());
    rx.connect(shard.receiver());
    let form = match rx.output_form.unwrap() {
        pulsebeam_rtc::RtpExtensionForm::OneByte => ExtensionForm::OneByte,
        pulsebeam_rtc::RtpExtensionForm::TwoByte => ExtensionForm::TwoByte,
        pulsebeam_rtc::RtpExtensionForm::Mixed => ExtensionForm::Mixed,
    };
    let endpoint = EndpointHints::new(
        Profile::H264,
        form,
        &[Mapping {
            id: 13,
            uri: dd::URI,
            attributes: None,
        }],
        None,
    )
    .unwrap();
    let mut leg = SenderLeg::new(
        OutputLeg {
            mid,
            primary: None,
            max_wire_bytes: 32,
            replace_queued: false,
        },
        endpoint,
        timeline::Timeline::new(500, 1500, 90_000, 64).unwrap(),
    )
    .with_dependency_descriptors(u16::MAX);
    let source = SourceMetadata {
        vad: Vad::On,
        capture: CaptureClock::Unmapped,
        same_topology: true,
    };
    let mut reader = DependencyDescriptorReader::new();
    // Appendix A HF1 through HF4 descriptor and marker trace. Payloads on
    // the two larger fragments need only be opaque to the forwarding path.
    let payloads: [&[u8]; 4] = [
        &[
            0x67, 0x42, 0xc0, 0x0a, 0xda, 0x10, 0x9b, 0x01, 0x10, 0, 0, 3, 0, 0x10, 0, 0, 3, 3,
            0xc8, 0xf1, 0x22, 0x6a,
        ],
        &[0x68, 0xce, 0x0f, 0xc8],
        &[0x7c; 1100],
        &[0x45; 1018],
    ];
    let descriptors: [&[u8]; 4] = [
        &[0x80, 0, 0, 0x80, 0, 0x3b, 0x40, 0],
        &[0, 0, 0],
        &[0, 0, 0],
        &[0x40, 0, 0],
    ];
    let packet = |sequence, timestamp, marker, raw: &[u8], payload: &[u8]| {
        FabricPacket::new(
            Header {
                profile: Profile::H264,
                marker,
                sequence,
                timestamp,
                ingress_ntp32: 0,
            },
            &[Extension {
                kind: ExtensionKind::DependencyDescriptor,
                value: raw,
            }],
            Box::from(payload),
        )
        .unwrap()
    };
    for (index, (raw, payload)) in descriptors.into_iter().zip(payloads).enumerate() {
        let marker = index == 3;
        let sequence = u32::try_from(index).unwrap();
        let packet = packet(sequence, 90_000, marker, raw, payload);
        let now = rx.clock.monotonic.max(tx.clock.monotonic);
        rx.clock.monotonic = now;
        shard.offer(now, now, u64::from(sequence), packet);
        if index == 0 {
            assert_eq!(
                shard.try_write(now, &mut leg, source),
                Err(AdmissionError::Rtc(pulsebeam_rtc::WriteError::RtpTooLarge))
            );
            assert_eq!(
                shard.receiver().transport.fabric_primary_progress(mid),
                None
            );
            leg.set_wire_budget(1200);
        }
        shard.write(now, &mut leg, source);
        for _ in 0..200 {
            rx.step_receiver(&mut shard);
            if rx.received.len() > index {
                break;
            }
        }
        let received = &rx.received[index];
        assert_eq!(
            received.header.sequence_number,
            500 + u16::try_from(index).unwrap()
        );
        assert_eq!(received.header.marker, marker);
        assert_eq!(received.payload.as_ref(), payload);
        let raw = received
            .header
            .ext_vals
            .user_values
            .get::<RawDependencyDescriptor>()
            .unwrap();
        let qualified = reader.read(&raw.0).unwrap();
        assert_eq!(qualified.frame_number, u16::MAX);
        assert_eq!(reader.active_decode_targets(), 1);
        assert_eq!(qualified.active_decode_targets, (index == 0).then_some(1));
        assert_eq!(qualified.attached_structure.is_some(), index == 0);
        assert_eq!(qualified.start_of_frame, index == 0);
        assert_eq!(qualified.end_of_frame, marker);
    }

    let now = rx.clock.monotonic;
    shard.offer(
        now,
        now,
        4,
        packet(4, 93_000, true, &[0xc0, 0, 1], b"queued before loss"),
    );
    rx.peer
        .direct_api()
        .stream_rx_by_mid(rx_mid, None)
        .unwrap()
        .request_keyframe(KeyframeRequestKind::Pli);
    for _ in 0..250 {
        rx.step_receiver(&mut shard);
        shard.flush_receiver_feedback();
    }
    let now = rx.clock.monotonic;
    assert_eq!(shard.try_write(now, &mut leg, source), Ok(None));
    let omitted = shard.offer_report(
        now,
        now,
        5,
        packet(5, 96_000, true, &[0xc0, 0, 2], b"old context"),
    );
    assert_eq!((omitted.enqueued, omitted.awaiting_entry), (0, 1));
    shard.offer(
        now,
        now,
        6,
        packet(
            6,
            99_000,
            true,
            &[0xc0, 0, 3, 0x80, 0, 0x3b, 0x40, 0],
            b"fresh structure",
        ),
    );
    shard.write(now, &mut leg, source);
    for _ in 0..200 {
        rx.step_receiver(&mut shard);
        if rx.received.len() == 5 {
            break;
        }
    }
    let received = &rx.received[4];
    assert_eq!(received.header.sequence_number, 506);
    assert_eq!(received.payload.as_ref(), b"fresh structure");
    assert!(received.header.marker);
    let raw = received
        .header
        .ext_vals
        .user_values
        .get::<RawDependencyDescriptor>()
        .unwrap();
    let mut reset_reader = DependencyDescriptorReader::new();
    let dd = reset_reader.read(&raw.0).unwrap();
    assert_eq!(dd.frame_number, 2);
    assert_eq!(dd.active_decode_targets, Some(1));
    assert!(dd.attached_structure.is_some());

    let mut replacement = DependencyDescriptorReader::new()
        .read(descriptors[0])
        .unwrap();
    replacement.frame_number = 4;
    replacement.end_of_frame = true;
    let structure = replacement.attached_structure.as_mut().unwrap();
    let resolution = RenderResolution {
        width: 640,
        height: 480,
    };
    structure.resolutions =
        std::iter::repeat_n(resolution, structure.spatial_layer_count()).collect();
    let mut encoded = [0; dd::MAX_DD_LEN];
    let len = DependencyDescriptorWriter::new()
        .write(&replacement, &mut encoded)
        .unwrap();
    let now = rx.clock.monotonic;
    shard.offer(
        now,
        now,
        7,
        packet(7, 102_000, true, &encoded[..len], b"new structure"),
    );
    shard.write(now, &mut leg, source);
    for _ in 0..200 {
        rx.step_receiver(&mut shard);
        if rx.received.len() == 6 {
            break;
        }
    }
    let changed = &rx.received[5];
    assert_eq!(changed.header.sequence_number, 507);
    assert!(changed.header.marker);
    assert_eq!(changed.payload.as_ref(), b"new structure");
    let raw = changed
        .header
        .ext_vals
        .user_values
        .get::<RawDependencyDescriptor>()
        .unwrap();
    let changed_dd = reset_reader.read(&raw.0).unwrap();
    assert_eq!(changed_dd.frame_number, 3);
    assert_ne!(changed_dd.template_id, dd.template_id);
    assert_eq!(changed_dd.resolution, Some(resolution));
    assert_eq!(changed_dd.active_decode_targets, Some(1));
    assert!(changed_dd.attached_structure.is_some());

    let now = rx.clock.monotonic;
    for (sequence, frame) in [(8, 5), (9, 6)] {
        shard.offer(
            now,
            now,
            u64::from(sequence),
            packet(
                sequence,
                101_000 + u32::from(frame) * 3000,
                true,
                &[0xc0, 0, frame],
                b"refused at handoff",
            ),
        );
        shard.write(now, &mut leg, source);
    }
    rx.stage_receiver_handoff(&mut shard);
    shard.reset_output();
    shard.submit_receiver(|_| panic!("pre-reset RTP crossed the final handoff"));
    for _ in 0..60 {
        rx.step_receiver(&mut shard);
    }
    assert_eq!(rx.received.len(), 6);
    assert_eq!(
        shard.receiver().transport.fabric_primary_progress(mid),
        Some(507)
    );

    let previous = leg.committed_branch();
    let replacement = shard.replace_source();
    leg.stage_branch(replacement);
    assert_eq!(leg.committed_branch(), previous);
    let now = rx.clock.monotonic;
    shard.offer(
        now,
        now,
        12,
        packet(
            12,
            8_000,
            true,
            &[0xc0, 0, 77, 0x80, 0, 0x3b, 0x40, 0],
            b"new source",
        ),
    );
    shard.write(now, &mut leg, source);
    for _ in 0..200 {
        rx.step_receiver(&mut shard);
        if rx.received.len() == 7 {
            break;
        }
    }
    assert_eq!(leg.committed_branch(), Some(replacement));
    let cut = &rx.received[6];
    assert_eq!(cut.header.sequence_number, 511);
    assert_eq!(cut.payload.as_ref(), b"new source");
    assert!(cut.header.marker);
    let raw = cut
        .header
        .ext_vals
        .user_values
        .get::<RawDependencyDescriptor>()
        .unwrap();
    let dd = reset_reader.read(&raw.0).unwrap();
    assert_eq!(dd.frame_number, 6);
    assert_eq!(dd.active_decode_targets, Some(1));
    assert!(dd.attached_structure.is_some());
    #[cfg(feature = "transport-conformance")]
    if let Some(udp) = &rx.udp {
        assert_eq!(udp.received_primary, rx.received.len());
        assert!(
            (1_000..=1_200).contains(&udp.peak),
            "participant's real protected RTP peak: {}",
            udp.peak
        );
    }
}

#[test]
fn installed_fabric_branch_projects_authenticated_rtp_without_waiting_for_holes() {
    use crate::rtp::fabric_egress::{
        CaptureClock, EndpointHints, OutputLeg, SenderLeg, SourceMetadata, Vad, timeline,
    };
    use crate::shard::core::FabricRtpFixture;
    use pulsebeam_core::fabric::{FabricPacket, Header, Profile};

    let (mut pair, receiver_config, mid, _, payload_type) = media_pair(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::RecvOnly,
        true,
    );
    let (mut publisher, publisher_config, publisher_leg, publisher_mid, publisher_pt) = media_pair(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::SendOnly,
        true,
    );
    let mut shard = FabricRtpFixture::new(publisher_config, receiver_config, Profile::Opus, None);
    publisher.connect(shard.publisher());
    pair.connect(shard.receiver());
    let form = match pair.output_form.unwrap() {
        pulsebeam_rtc::RtpExtensionForm::OneByte => ExtensionForm::OneByte,
        pulsebeam_rtc::RtpExtensionForm::TwoByte => ExtensionForm::TwoByte,
        pulsebeam_rtc::RtpExtensionForm::Mixed => ExtensionForm::Mixed,
    };
    let endpoint = EndpointHints::new(Profile::Opus, form, &[], None).unwrap();
    let leg = SenderLeg::new(
        OutputLeg {
            mid,
            primary: None,
            max_wire_bytes: 1200,
            replace_queued: false,
        },
        endpoint,
        timeline::Timeline::new(500, 1500, 48_000, 64).unwrap(),
    );
    let source = SourceMetadata {
        vad: Vad::On,
        capture: CaptureClock::Unmapped,
        same_topology: true,
    };
    let opaque = [0xf8, 0xff, 0xfe];
    let packet = |sequence, timestamp, marker| {
        FabricPacket::new(
            Header {
                profile: Profile::Opus,
                marker,
                sequence,
                timestamp,
                ingress_ntp32: 0x1234,
            },
            &[],
            Box::from(opaque.as_slice()),
        )
        .unwrap()
    };
    let send = |publisher: &mut Pair, sequence, timestamp, marker| {
        publisher
            .peer
            .direct_api()
            .stream_tx_by_mid(publisher_mid, None)
            .unwrap()
            .write_rtp(
                str0m::rtp::RtpWrite::new(
                    publisher_pt.into(),
                    u64::from(sequence).into(),
                    timestamp,
                    publisher.clock.monotonic.into(),
                    opaque.as_slice(),
                )
                .marker(marker),
            );
    };
    send(&mut publisher, 99_u32, 900, false);
    let mut ingress = None;
    for _ in 0..200 {
        publisher.step(
            shard.publisher(),
            Some(&mut |rtp: AuthenticatedRtp<'_>, _| {
                ingress = Some(
                    RtpIngressShadow::bind_whole(
                        Profile::Opus,
                        rtp.binding(),
                        ExtensionForm::OneByte,
                        &[],
                        Anchors::default(),
                        Limits {
                            positions: NonZeroU16::new(64).unwrap(),
                            max_age: Duration::from_millis(120),
                            max_idle: Duration::from_secs(2),
                        },
                    )
                    .unwrap(),
                );
            }),
        );
        if ingress.is_some() {
            break;
        }
    }
    let (_, publication) = shard
        .publisher()
        .upstream
        .slot_for_mid(publisher_leg)
        .unwrap();
    shard.use_negotiated_publication(publication, None);
    assert!(shard.attach_publisher_ingress(ingress.unwrap()));
    assert!(shard.bind_output(leg, source));
    let (mailbox_control, mut mailbox) = shard.bind_publisher_mailbox(publisher.clock.monotonic);
    let mut frames = Vec::new();
    let mut first_anchor = None;
    for (index, (sequence, timestamp, marker, output_sequence, output_timestamp)) in [
        (100_u32, 1000_u32, false, 500_u16, 1500_u32),
        (102, 1600, true, 502, 2100),
        (101, 1300, false, 501, 1800),
    ]
    .into_iter()
    .enumerate()
    {
        publisher.clock.monotonic = publisher.clock.monotonic.max(pair.clock.monotonic);
        send(&mut publisher, sequence, timestamp, marker);
        for _ in 0..200 {
            let (owner, mut sink) = shard.publisher_with_sink();
            publisher.step_with_sink(owner, None, &mut sink);
            shard.flush_publisher_media(publisher.clock.monotonic);
            if mailbox.try_recv_batch(&mut frames, 2) != 0 {
                break;
            }
        }
        assert_eq!(frames.len(), 1, "one Fabric publication and no legacy copy");
        let crate::shard::worker::ShardFrame::Fabric(offer) = frames.pop().unwrap().packet else {
            panic!("expected authenticated Fabric publication");
        };
        assert_eq!(offer.sequence, u64::from(sequence));
        assert_eq!(offer.packet.header().marker, marker);
        assert_eq!(offer.packet.header().timestamp, timestamp);
        assert_eq!(offer.packet.payload(), opaque);
        assert_eq!(offer.packet.remaining_hops(), 7);
        let now = publisher.clock.monotonic;
        pair.clock.monotonic = now;
        first_anchor.get_or_insert(now);
        for _ in 0..200 {
            pair.step_receiver(&mut shard);
            if pair.received.len() > index {
                break;
            }
        }
        let received = pair
            .received
            .get(index)
            .expect("authenticated output before the next offer");
        assert_eq!(received.header.sequence_number, output_sequence);
        assert_eq!(received.header.timestamp, output_timestamp);
        assert_eq!(received.header.marker, marker);
        assert_eq!(*received.header.payload_type, payload_type);
        assert_eq!(received.payload.as_ref(), opaque);
    }
    assert_eq!(pair.received.len(), 3);
    let mut leg = shard.receiver().media_egress.remove(&mid).unwrap().leg;
    drop(mailbox);
    drop(mailbox_control);

    for _ in 0..1100 {
        pair.step_receiver(&mut shard);
    }
    let now = pair.clock.monotonic;
    shard.offer(now, now, 103, packet(103, 1900, false));
    shard.write(now, &mut leg, source);
    pair.stage_receiver_handoff(&mut shard);
    shard.submit_receiver(|offered| {
        assert_eq!(offered.len(), 1);
        0
    });
    assert_eq!(
        shard.receiver().transport.fabric_primary_progress(mid),
        Some(502)
    );

    // The old epoch reserves a future timestamp. Revoke the branch after
    // protection and batching, but before the socket sees the datagram.
    let now = pair.clock.monotonic;
    shard.offer(now, now, 104, packet(104, 200_000, false));
    shard.write(now, &mut leg, source);
    pair.stage_receiver_handoff(&mut shard);
    let replacement = shard.replace_source();
    shard.submit_receiver(|_| panic!("revoked datagram reached the socket"));
    assert_eq!(
        shard.receiver().transport.fabric_primary_progress(mid),
        Some(502)
    );
    let cut_at = pair.clock.monotonic;
    leg.stage_branch(replacement);
    shard.offer(cut_at, cut_at, 4, packet(4, 800, true));
    shard.write(cut_at, &mut leg, source);
    for _ in 0..1100 {
        pair.step_receiver(&mut shard);
    }
    assert_eq!(pair.received.len(), 4);
    let cut_packet = &pair.received[3];
    assert_eq!(cut_packet.header.sequence_number, 506);
    assert!(cut_packet.header.marker);
    let cut_timestamp = u64::from(cut_packet.header.timestamp);
    assert_eq!(cut_timestamp, 248_500);
    let old_report = cut_packet.last_sender_info.expect("report before cut");
    assert_eq!(old_report.sender_packet_count, 3);

    let now = pair.clock.monotonic;
    shard.offer(now, now, 5, packet(5, 1280, false));
    shard.write(now, &mut leg, source);
    for _ in 0..200 {
        pair.step_receiver(&mut shard);
        if pair.received.len() == 5 {
            break;
        }
    }
    let probe = &pair.received[4];
    assert_eq!(u64::from(probe.header.timestamp), cut_timestamp + 480);
    let new_report = probe.last_sender_info.expect("report after cut");
    assert_eq!(new_report.sender_packet_count, 4);
    assert_eq!(old_report.ssrc, new_report.ssrc);
    let check_report = |rtp: u64, wall: SystemTime, anchor_rtp, anchor_at: Instant| {
        let clock = pair.protocol_clock;
        let wall_at_anchor = clock.wall + anchor_at.duration_since(clock.monotonic);
        let elapsed = wall.duration_since(wall_at_anchor).unwrap();
        let ticks = u64::try_from(elapsed.as_nanos() * 48_000 / 1_000_000_000).unwrap();
        assert!(rtp.abs_diff(anchor_rtp + ticks) <= 1);
    };
    check_report(
        old_report.rtp_time.numer(),
        old_report.ntp_time,
        1500,
        first_anchor.unwrap(),
    );
    check_report(
        new_report.rtp_time.numer(),
        new_report.ntp_time,
        cut_timestamp,
        cut_at,
    );
}
