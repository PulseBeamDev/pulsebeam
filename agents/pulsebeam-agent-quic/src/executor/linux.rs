use std::{
    collections::{BTreeMap, VecDeque},
    future::{Future, pending},
    net::IpAddr,
    num::{NonZeroU16, NonZeroU32},
    pin::Pin,
    time::Duration,
};

use noq_proto::ClientConfig;
use pulsebeam_agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, Effect, Failure, FailureClass, Generation,
    HostEvent, HttpEffect, HttpEvent, HttpMethod, HttpRequest, HttpResponse, MediaKind,
    Notification, OperationId, QuicEffect, QuicEvent, Snapshot, TimerEffect, TimerEvent, TimerId,
};
use pulsebeam_core::{
    fabric::MAX_WAN_RTP_BYTES,
    h264::{Depacketizer, MAX_ACCESS_UNIT_SIZE, Packetization, PacketizedChunk, Packetizer},
};
use pulsebeam_runtime::time::{self, Instant};
use rustls::RootCertStore;

use crate::{
    records::{Datagram, DatagramKind, PictureLossIndication},
    transport,
};

use super::udp::{OpenError, UdpAssociation};

const PLACEMENT_BODY_LIMIT: usize = 1_024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const EFFECTS_PER_TURN: usize = 32;
const RTP_HEADER: usize = 12;

type Work<T> = Pin<Box<dyn Future<Output = T>>>;
type DialResult = Result<Box<UdpAssociation>, QuicEvent>;

#[cfg(feature = "sim")]
struct SimServices {
    placement: Box<dyn Fn(HttpRequest) -> Work<Option<HttpResponse>>>,
    authority: String,
    address: IpAddr,
}

#[derive(Debug)]
pub enum Error {
    Core(AgentError),
    Http(reqwest::Error),
    Tls(transport::ConfigError),
    UnexpectedEffect,
    Capacity,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpusSendError {
    NotReady,
    Empty,
    Datagram(transport::DatagramAdmission),
}

#[derive(Debug, PartialEq, Eq)]
pub enum H264SendError {
    NotReady,
    Busy,
    InvalidAccessUnit,
    Size,
}

struct PendingVideo {
    track_id: String,
    generation: Generation,
    binding: NonZeroU16,
    timestamp: u32,
    packets: Packetization,
    next: PacketizedChunk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoRefresh {
    generation: Generation,
    binding: NonZeroU16,
    media_ssrc: NonZeroU32,
}

/// One encoded Opus packet. Sequence and timestamp are the negotiated RTP
/// stream's coordinates; the timestamp uses the 48 kHz RTP clock.
#[derive(Debug, PartialEq, Eq)]
pub struct ReceivedOpus {
    pub publication_id: String,
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// An encoded Annex-B access unit from the receiver's 90 kHz RTP clock.
#[derive(Debug, PartialEq, Eq)]
pub struct ReceivedH264 {
    pub publication_id: String,
    pub timestamp: u32,
    pub access_unit: Vec<u8>,
}

#[derive(Default)]
struct VideoAssembly {
    depacketizer: Depacketizer,
    last: Option<(u16, u32)>,
    discard: bool,
}

impl VideoAssembly {
    fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let mut start = true;
        if let Some((previous, time)) = self.last {
            let distance = sequence.wrapping_sub(previous);
            if distance == 0 || distance >= 0x8000 {
                return None;
            }
            start = timestamp != time;
            if distance != 1 {
                self.depacketizer.reset();
                self.discard = true;
            } else if start {
                self.discard = false;
            }
        }
        self.last = Some((sequence, timestamp));
        if self.discard {
            return None;
        }
        match self.depacketizer.push(payload, start, marker) {
            Ok(Some(frame)) => {
                self.discard = true;
                Some(frame)
            }
            Ok(None) => None,
            Err(_) => {
                self.discard = true;
                None
            }
        }
    }
}

const MAX_QUEUED_OPUS: usize = 32;
const MAX_QUEUED_H264: usize = 2;
const MAX_H264_ASSEMBLIES: usize = 8;

/// Caller-driven Linux Agent. Run its turns on the reactor that owns the UDP
/// sockets. It does not dispatch a background task or retain an RTC host.
pub struct LinuxAgent {
    core: Agent,
    http: reqwest::Client,
    tls: ClientConfig,
    local: IpAddr,
    clock_origin: Instant,
    request: Option<(OperationId, Work<HttpEvent>)>,
    dialing: Option<(Generation, Work<DialResult>)>,
    sockets: [Option<Box<UdpAssociation>>; 2],
    rtp_sequence:
        BTreeMap<(pulsebeam_agent_core::MediaKind, String), (Generation, NonZeroU16, u16)>,
    pending_video: Option<PendingVideo>,
    video_refresh: VecDeque<VideoRefresh>,
    video_assemblies: BTreeMap<(Generation, NonZeroU16), VideoAssembly>,
    received_h264: VecDeque<(Generation, NonZeroU16, ReceivedH264)>,
    received_opus: VecDeque<(Generation, NonZeroU16, ReceivedOpus)>,
    timers: BTreeMap<TimerId, Instant>,
    #[cfg(feature = "sim")]
    sim: Option<SimServices>,
}

impl LinuxAgent {
    pub fn new(
        config: AgentConfig,
        local: IpAddr,
        quic_roots: RootCertStore,
    ) -> Result<Self, Error> {
        let core = Agent::new_quic(config).map_err(Error::Core)?;
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(Error::Http)?;
        let tls = transport::system_client_config(quic_roots).map_err(Error::Tls)?;
        Ok(Self {
            core,
            http,
            tls,
            local,
            clock_origin: Instant::now(),
            request: None,
            dialing: None,
            sockets: [None, None],
            rtp_sequence: BTreeMap::new(),
            pending_video: None,
            video_refresh: VecDeque::new(),
            video_assemblies: BTreeMap::new(),
            received_h264: VecDeque::new(),
            received_opus: VecDeque::new(),
            timers: BTreeMap::new(),
            #[cfg(feature = "sim")]
            sim: None,
        })
    }

    pub fn command(&mut self, command: AgentCommand) -> Result<(), AgentError> {
        self.core.command_at(command, self.clock_origin.elapsed())
    }

    #[cfg(feature = "sim")]
    pub fn with_sim_services<F, Fut>(
        mut self,
        authority: String,
        address: IpAddr,
        placement: F,
    ) -> Self
    where
        F: Fn(HttpRequest) -> Fut + 'static,
        Fut: Future<Output = Option<HttpResponse>> + 'static,
    {
        self.sim = Some(SimServices {
            placement: Box::new(move |request| Box::pin(placement(request))),
            authority,
            address,
        });
        self
    }

    pub fn resource(
        &mut self,
        command: pulsebeam_agent_core::ResourceCommand,
    ) -> Result<pulsebeam_agent_core::ResourceResult, AgentError> {
        self.core.resource(command)
    }

    pub fn send_data(
        &mut self,
        spec: pulsebeam_agent_core::TrackSpec,
        payload: Vec<u8>,
    ) -> Result<OperationId, AgentError> {
        self.core
            .send_data_at(spec, payload, self.clock_origin.elapsed())
    }

    /// The canonical ID of the installed and enabled local Opus publication.
    pub fn opus_publication_id(&self, track_id: &str) -> Option<&str> {
        let publication = self
            .core
            .quic_rtp_publication(pulsebeam_agent_core::MediaKind::Audio, track_id)?;
        publication.ready.then_some(publication.publication_id)
    }

    /// Admit a single encoded Opus packet for an installed audio publication.
    /// The timestamp is in the caller's 48 kHz RTP clock. Success transfers
    /// the complete datagram to QUIC, not to the remote application's queue.
    pub fn send_opus(
        &mut self,
        track_id: &str,
        timestamp: u32,
        marker: bool,
        opus: &[u8],
    ) -> Result<(), OpusSendError> {
        self.offer_media_rtp(
            pulsebeam_agent_core::MediaKind::Audio,
            track_id,
            timestamp,
            marker,
            opus,
        )
    }

    fn offer_media_rtp(
        &mut self,
        kind: pulsebeam_agent_core::MediaKind,
        track_id: &str,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Result<(), OpusSendError> {
        if payload.is_empty() {
            return Err(OpusSendError::Empty);
        }
        let wire_len = payload.len().saturating_add(RTP_HEADER);
        if wire_len.saturating_add(crate::records::DATAGRAM_HEADER)
            > usize::from(transport::UDP_PAYLOAD_BYTES)
        {
            return Err(OpusSendError::Datagram(transport::DatagramAdmission::Size));
        }
        let pulsebeam_agent_core::QuicRtpPublication {
            generation,
            binding,
            ssrc,
            payload_type,
            ..
        } = self
            .core
            .quic_rtp_publication(kind, track_id)
            .filter(|publication| publication.ready)
            .ok_or(OpusSendError::NotReady)?;
        let key = (kind, track_id.to_owned());
        if !self.rtp_sequence.contains_key(&key) && self.rtp_sequence.len() >= 64 {
            self.rtp_sequence
                .retain(|(kind, track), (generation, binding, _)| {
                    self.core
                        .quic_rtp_publication(*kind, track)
                        .is_some_and(|live| {
                            *generation == live.generation && *binding == live.binding
                        })
                });
            if self.rtp_sequence.len() >= 64 {
                return Err(OpusSendError::NotReady);
            }
        }
        let socket = self
            .sockets
            .iter_mut()
            .flatten()
            .find(|socket| socket.generation() == generation)
            .ok_or(OpusSendError::NotReady)?;
        let sequence = self
            .rtp_sequence
            .get(&key)
            .filter(|(live_generation, live_binding, _)| {
                *live_generation == generation && *live_binding == binding
            })
            .map_or(0, |(_, _, next)| *next);
        let mut packet = Vec::with_capacity(wire_len);
        packet.extend_from_slice(&[0x80, payload_type | (u8::from(marker) << 7)]);
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        socket
            .offer_media(Datagram {
                kind: DatagramKind::Rtp,
                binding,
                payload: &packet,
            })
            .map_err(OpusSendError::Datagram)?;
        let next = (generation, binding, sequence.wrapping_add(1));
        if let Some(slot) = self.rtp_sequence.get_mut(&key) {
            *slot = next;
        } else {
            self.rtp_sequence.insert(key, next);
        }
        Ok(())
    }

    pub fn h264_publication_id(&self, track_id: &str) -> Option<&str> {
        let live = self
            .core
            .quic_rtp_publication(pulsebeam_agent_core::MediaKind::Video, track_id)?;
        live.ready.then_some(live.publication_id)
    }

    /// Consume a pending PLI for this currently installed video publication.
    pub fn take_h264_keyframe_request(&mut self, track_id: &str) -> bool {
        let Some(live) = self
            .core
            .quic_rtp_publication(MediaKind::Video, track_id)
            .filter(|live| live.ready)
        else {
            return false;
        };
        let Some(media_ssrc) = NonZeroU32::new(live.ssrc) else {
            return false;
        };
        let request = VideoRefresh {
            generation: live.generation,
            binding: live.binding,
            media_ssrc,
        };
        let Some(index) = self
            .video_refresh
            .iter()
            .position(|pending| *pending == request)
        else {
            return false;
        };
        self.video_refresh.remove(index).is_some()
    }

    /// Queue one Annex-B access unit for the current binding. Drive Agent turns
    /// to transmit the fragments. The caller owns the 90 kHz RTP clock.
    pub fn send_h264(
        &mut self,
        track_id: &str,
        timestamp: u32,
        access_unit: Vec<u8>,
    ) -> Result<(), H264SendError> {
        if self.pending_video.is_some() {
            return Err(H264SendError::Busy);
        }
        if access_unit.len() > MAX_ACCESS_UNIT_SIZE {
            return Err(H264SendError::Size);
        }
        let live = self
            .core
            .quic_rtp_publication(pulsebeam_agent_core::MediaKind::Video, track_id)
            .filter(|live| live.ready)
            .ok_or(H264SendError::NotReady)?;
        let limit = self
            .sockets
            .iter_mut()
            .flatten()
            .find(|socket| socket.generation() == live.generation)
            .and_then(|socket| socket.datagram_limit())
            .ok_or(H264SendError::NotReady)?;
        let budget = limit
            .saturating_sub(crate::records::DATAGRAM_HEADER)
            .min(MAX_WAN_RTP_BYTES)
            .saturating_sub(RTP_HEADER);
        if budget <= 2 {
            return Err(H264SendError::Size);
        }
        let mut packets = Packetizer::new(budget).into_packets(access_unit);
        let next = packets.next().ok_or(H264SendError::InvalidAccessUnit)?;
        self.pending_video = Some(PendingVideo {
            track_id: track_id.into(),
            generation: live.generation,
            binding: live.binding,
            timestamp,
            packets,
            next,
        });
        Ok(())
    }

    fn offer_pending_video(&mut self) -> Option<Generation> {
        use transport::DatagramAdmission::{ControlTurn, Full};
        let mut video = self.pending_video.take()?;
        let kind = pulsebeam_agent_core::MediaKind::Video;
        if !self
            .core
            .quic_rtp_publication(kind, &video.track_id)
            .is_some_and(|live| {
                live.ready && live.generation == video.generation && live.binding == video.binding
            })
        {
            return None;
        }
        match self.offer_media_rtp(
            kind,
            &video.track_id,
            video.timestamp,
            video.next.end_of_frame,
            &video.next.payload,
        ) {
            Ok(()) => {
                if let Some(next) = video.packets.next() {
                    video.next = next;
                    self.pending_video = Some(video);
                }
                None
            }
            Err(OpusSendError::Datagram(ControlTurn)) => {
                let generation = video.generation;
                self.pending_video = Some(video);
                Some(generation)
            }
            Err(OpusSendError::Datagram(Full)) => {
                self.pending_video = Some(video);
                None
            }
            Err(_) => None,
        }
    }

    pub fn h264_receiver_ready(&self, publication_id: &str) -> bool {
        self.core
            .quic_rtp_receivers(MediaKind::Video)
            .any(|live| live.publication_id == publication_id)
    }

    /// Request a keyframe for a current pinned video binding using RTCP PLI.
    /// Success admits a best-effort datagram, not an acknowledgement from the source.
    pub fn request_h264_keyframe(
        &mut self,
        publication_id: &str,
    ) -> Result<(), transport::DatagramAdmission> {
        use transport::DatagramAdmission::Unsupported;
        let live = self
            .core
            .quic_rtp_receivers(MediaKind::Video)
            .find(|live| live.publication_id == publication_id)
            .ok_or(Unsupported)?;
        let media_ssrc = NonZeroU32::new(live.ssrc).ok_or(Unsupported)?;
        let socket = self
            .sockets
            .iter_mut()
            .flatten()
            .find(|socket| socket.generation() == live.generation)
            .ok_or(Unsupported)?;
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc,
        }
        .encode();
        socket.offer_media(Datagram {
            kind: DatagramKind::Rtcp,
            binding: live.binding,
            payload: &pli,
        })
    }

    /// Read a complete access unit for a current pinned video binding.
    pub fn next_h264(&mut self) -> Option<ReceivedH264> {
        while let Some((generation, binding, frame)) = self.received_h264.pop_front() {
            if self.core.quic_rtp_receivers(MediaKind::Video).any(|live| {
                live.generation == generation
                    && live.binding == binding
                    && live.publication_id == frame.publication_id
            }) {
                return Some(frame);
            }
        }
        None
    }

    fn receive_h264(&mut self, generation: Generation, binding: NonZeroU16, packet: &[u8]) {
        let Some(live) = self
            .core
            .quic_rtp_receivers(MediaKind::Video)
            .find(|live| live.generation == generation && live.binding == binding)
        else {
            return;
        };
        let Some(([0x80, pt, a, b, c, d, e, f, g, h, i, j], payload)) =
            packet.split_first_chunk::<RTP_HEADER>()
        else {
            return;
        };
        if *pt & 0x7f != live.payload_type || u32::from_be_bytes([*g, *h, *i, *j]) != live.ssrc {
            return;
        }
        let key = (generation, binding);
        if self.video_assemblies.len() >= MAX_H264_ASSEMBLIES
            && !self.video_assemblies.contains_key(&key)
        {
            self.video_assemblies.retain(|&(generation, binding), _| {
                self.core
                    .quic_rtp_receivers(MediaKind::Video)
                    .any(|live| live.generation == generation && live.binding == binding)
            });
            if self.video_assemblies.len() >= MAX_H264_ASSEMBLIES {
                let Some(idle) = self
                    .video_assemblies
                    .iter()
                    .find_map(|(key, assembly)| assembly.discard.then_some(*key))
                else {
                    return;
                };
                self.video_assemblies.remove(&idle);
            }
        }
        let timestamp = u32::from_be_bytes([*c, *d, *e, *f]);
        if let Some(access_unit) = self.video_assemblies.entry(key).or_default().push(
            u16::from_be_bytes([*a, *b]),
            timestamp,
            *pt & 0x80 != 0,
            payload,
        ) {
            if self.received_h264.len() >= MAX_QUEUED_H264 {
                let _ = self.received_h264.pop_front();
            }
            self.received_h264.push_back((
                generation,
                binding,
                ReceivedH264 {
                    publication_id: live.publication_id.into(),
                    timestamp,
                    access_unit,
                },
            ));
        }
    }

    /// Whether the pinned audio source has an installed Opus receive binding.
    pub fn opus_receiver_ready(&self, publication_id: &str) -> bool {
        self.core
            .quic_rtp_receivers(MediaKind::Audio)
            .any(|live| live.publication_id == publication_id)
    }

    /// Read received Opus for the currently installed pinned audio sources.
    /// The bounded queue drops its oldest packet when the caller falls behind.
    pub fn next_opus(&mut self) -> Option<ReceivedOpus> {
        while let Some((generation, binding, frame)) = self.received_opus.pop_front() {
            if self.core.quic_rtp_receivers(MediaKind::Audio).any(|live| {
                live.generation == generation
                    && live.binding == binding
                    && live.publication_id == frame.publication_id
            }) {
                return Some(frame);
            }
        }
        None
    }

    fn receive_opus(&mut self, generation: Generation, binding: NonZeroU16, packet: &[u8]) {
        let Some(live) = self
            .core
            .quic_rtp_receivers(MediaKind::Audio)
            .find(|live| live.generation == generation && live.binding == binding)
        else {
            return;
        };
        let Some(([0x80, pt, a, b, c, d, e, f, g, h, i, j], payload)) =
            packet.split_first_chunk::<12>()
        else {
            return;
        };
        if payload.is_empty()
            || *pt & 0x7f != live.payload_type
            || u32::from_be_bytes([*g, *h, *i, *j]) != live.ssrc
        {
            return;
        }
        let frame = ReceivedOpus {
            publication_id: live.publication_id.to_owned(),
            sequence: u16::from_be_bytes([*a, *b]),
            timestamp: u32::from_be_bytes([*c, *d, *e, *f]),
            marker: *pt & 0x80 != 0,
            payload: payload.to_vec(),
        };
        if self.received_opus.len() >= MAX_QUEUED_OPUS {
            self.received_opus.pop_front();
        }
        self.received_opus.push_back((generation, binding, frame));
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.core.snapshot()
    }

    pub fn next_notification(&mut self) -> Option<Notification> {
        self.core.next_notification()
    }

    /// Execute bounded ready effects, then wait for one input. It is safe to
    /// cancel this wait, invoke a command, and drive the same Agent again.
    pub async fn turn(&mut self) -> Result<(), Error> {
        for _ in 0..EFFECTS_PER_TURN {
            let Some(effect) = self.core.next_effect() else {
                return self.wait().await;
            };
            self.execute(effect)?;
        }
        Ok(())
    }

    fn accept(&mut self, event: HostEvent) -> Result<(), Error> {
        if let HostEvent::Quic(QuicEvent::Rtp {
            generation,
            binding,
            payload,
        }) = event
        {
            self.receive_opus(generation, binding, &payload);
            self.receive_h264(generation, binding, &payload);
            return Ok(());
        }
        if let HostEvent::Quic(QuicEvent::PictureLoss {
            generation,
            binding,
            media_ssrc,
        }) = event
        {
            let request = VideoRefresh {
                generation,
                binding,
                media_ssrc,
            };
            if !self.video_refresh.contains(&request) {
                if self.video_refresh.len() >= 8 {
                    let _ = self.video_refresh.pop_front();
                }
                self.video_refresh.push_back(request);
            }
            return Ok(());
        }
        self.core
            .handle_at(event, self.clock_origin.elapsed())
            .map_err(Error::Core)
    }

    fn request_placement(&self, request: HttpRequest) -> Work<Option<HttpResponse>> {
        #[cfg(feature = "sim")]
        if let Some(sim) = &self.sim {
            return (sim.placement)(request);
        }
        let client = self.http.clone();
        Box::pin(async move { execute_placement(&client, request).await })
    }

    fn execute(&mut self, effect: Effect) -> Result<(), Error> {
        match effect {
            Effect::Http(HttpEffect::Request {
                operation, request, ..
            }) => {
                if self.request.is_some() {
                    return Err(Error::Capacity);
                }
                let response = self.request_placement(request);
                self.request = Some((
                    operation,
                    Box::pin(async move {
                        match response.await {
                            Some(response) => HttpEvent::Response {
                                operation,
                                response,
                            },
                            None => HttpEvent::Failed {
                                operation,
                                message: "HTTPS placement failed".into(),
                            },
                        }
                    }),
                ));
            }
            Effect::Http(HttpEffect::Cancel { operation }) => {
                if self
                    .request
                    .as_ref()
                    .is_some_and(|(id, _)| *id == operation)
                {
                    self.request = None;
                }
            }
            Effect::Timer(TimerEffect::Schedule { timer, after }) => {
                let now = Instant::now();
                self.timers
                    .insert(timer, now.checked_add(after).unwrap_or(now));
            }
            Effect::Timer(TimerEffect::Cancel { timer }) => {
                self.timers.remove(&timer);
            }
            Effect::Quic(QuicEffect::Admit(admit)) => {
                if self.dialing.is_some() || self.sockets.iter().all(Option::is_some) {
                    return Err(Error::Capacity);
                }
                let generation = admit.generation;
                let tls = self.tls.clone();
                let local = self.local;
                let origin = self.clock_origin;
                #[cfg(feature = "sim")]
                let sim_address = self.sim.as_ref().map(|sim| {
                    (sim.authority == admit.placement.host
                        && sim.address.is_ipv4() == local.is_ipv4())
                    .then_some(sim.address)
                });
                #[cfg(not(feature = "sim"))]
                let sim_address: Option<Option<IpAddr>> = None;
                self.dialing = Some((
                    generation,
                    Box::pin(async move {
                        let unavailable = || QuicEvent::AdmissionFailed {
                            generation,
                            failure: transient("QUIC authority unavailable"),
                        };
                        let deadline =
                            origin.checked_add(admit.deadline).ok_or_else(unavailable)?;
                        let connect = async {
                            let remote = if let Some(sim_address) = sim_address {
                                sim_address.ok_or_else(unavailable)?
                            } else {
                                tokio::net::lookup_host((
                                    admit.placement.host.as_str(),
                                    admit.placement.port.get(),
                                ))
                                .await
                                .map_err(|_| unavailable())?
                                .find(|address| address.is_ipv4() == local.is_ipv4())
                                .ok_or_else(unavailable)?
                                .ip()
                            };
                            UdpAssociation::connect(origin.into_std(), local, remote, tls, admit)
                                .await
                                .map(Box::new)
                                .map_err(|error| match error {
                                    OpenError::Admission(event) => event,
                                    OpenError::Socket(_) => unavailable(),
                                })
                        };
                        time::timeout_at(deadline, connect)
                            .await
                            .map_err(|_| unavailable())?
                    }),
                ));
            }
            Effect::Quic(effect @ QuicEffect::Close { generation }) => {
                if self
                    .dialing
                    .as_ref()
                    .is_some_and(|(id, _)| *id == generation)
                {
                    self.dialing = None;
                }
                if let Some(socket) = self
                    .sockets
                    .iter_mut()
                    .flatten()
                    .find(|socket| socket.generation() == generation)
                {
                    socket.submit(effect).map_err(|_| Error::UnexpectedEffect)?;
                } else {
                    self.accept(HostEvent::Quic(QuicEvent::Closed { generation }))?;
                }
            }
            Effect::Quic(effect @ QuicEffect::RetireData { generation, .. }) => {
                if let Some(socket) = self
                    .sockets
                    .iter_mut()
                    .flatten()
                    .find(|socket| socket.generation() == generation)
                {
                    socket.submit(effect).map_err(|_| Error::UnexpectedEffect)?;
                }
            }
            Effect::Quic(QuicEffect::SendData(send)) => {
                let (generation, channel, operation) =
                    (send.generation, send.channel, send.operation);
                let result = self
                    .sockets
                    .iter_mut()
                    .flatten()
                    .find(|socket| socket.generation() == generation)
                    .ok_or(super::SubmitError::NotAdmitted)
                    .and_then(|socket| socket.submit(QuicEffect::SendData(send)));
                if let Err(error) = result {
                    self.accept(HostEvent::Quic(QuicEvent::DataSent {
                        generation,
                        channel,
                        operation,
                        result: Err(format!("QUIC DATA submission: {error:?}")),
                    }))?;
                }
            }
            Effect::Quic(effect @ (QuicEffect::SendControl { .. } | QuicEffect::Renew(_))) => {
                let generation = match &effect {
                    QuicEffect::SendControl { generation, .. } => *generation,
                    QuicEffect::Renew(renew) => renew.generation,
                    _ => return Err(Error::UnexpectedEffect),
                };
                if let Some(slot) = self.sockets.iter_mut().find(|socket| {
                    socket
                        .as_ref()
                        .is_some_and(|socket| socket.generation() == generation)
                }) && let Some(socket) = slot.as_mut()
                    && socket.submit(effect).is_err()
                {
                    let event = socket.io_failure();
                    *slot = None;
                    self.accept(HostEvent::Quic(event))?;
                }
            }
            _ => return Err(Error::UnexpectedEffect),
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<(), Error> {
        let retry_payload_turn = self.offer_pending_video();
        let next_timer = self
            .timers
            .iter()
            .min_by_key(|(_, deadline)| *deadline)
            .map(|(timer, at)| (*timer, *at));
        let timer = async {
            if let Some((timer, at)) = next_timer {
                time::sleep_until(at).await;
                timer
            } else {
                pending().await
            }
        };
        let [first, second] = &mut self.sockets;
        let input = pulsebeam_runtime::select! {
            http = work(&mut self.request) => Input::Http(http),
            dial = work(&mut self.dialing) => Input::Dial(dial),
            timer = timer => Input::Timer(timer),
            event = socket_turn(first, retry_payload_turn) => Input::Socket(event),
            event = socket_turn(second, retry_payload_turn) => Input::Socket(event),
        };
        match input {
            Input::Http(event) => {
                self.request = None;
                self.accept(HostEvent::Http(event))?;
            }
            Input::Dial(result) => {
                self.dialing = None;
                match result {
                    Ok(socket) => {
                        let slot = self
                            .sockets
                            .iter_mut()
                            .find(|socket| socket.is_none())
                            .ok_or(Error::Capacity)?;
                        *slot = Some(socket);
                    }
                    Err(event) => self.accept(HostEvent::Quic(event))?,
                }
            }
            Input::Timer(timer) => {
                self.timers.remove(&timer);
                self.accept(HostEvent::Timer(TimerEvent::Fired { timer }))?;
            }
            Input::Socket(Some(event)) => self.accept(HostEvent::Quic(event))?,
            Input::Socket(None) => {}
        }
        Ok(())
    }
}

enum Input {
    Http(HttpEvent),
    Dial(DialResult),
    Timer(TimerId),
    Socket(Option<QuicEvent>),
}

async fn work<Id, T>(work: &mut Option<(Id, Work<T>)>) -> T {
    match work {
        Some((_, future)) => future.as_mut().await,
        None => pending().await,
    }
}

async fn socket_turn(
    slot: &mut Option<Box<UdpAssociation>>,
    retry_payload_turn: Option<Generation>,
) -> Option<QuicEvent> {
    let Some(socket) = slot.as_mut() else {
        return pending().await;
    };
    let retry = retry_payload_turn == Some(socket.generation());
    let Ok(event) = socket.progress(retry).await else {
        let event = socket.io_failure();
        *slot = None;
        return Some(event);
    };
    if matches!(event, Some(QuicEvent::Closed { .. })) {
        *slot = None;
    }
    event
}

async fn execute_placement(http: &reqwest::Client, request: HttpRequest) -> Option<HttpResponse> {
    if request.method != HttpMethod::Post {
        return None;
    }
    let mut outgoing = http.post(request.uri).body(request.body);
    for header in request.headers {
        outgoing = outgoing.header(header.name, header.value);
    }
    let mut response = outgoing.send().await.ok()?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    if status == 200 {
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len().saturating_add(chunk.len()) > PLACEMENT_BODY_LIMIT {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
    }
    Some(HttpResponse {
        status,
        body,
        headers: Vec::new(),
    })
}

fn transient(message: &'static str) -> Failure {
    Failure {
        class: FailureClass::Transient,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::VideoAssembly;

    #[test]
    fn video_assembly_discards_incomplete_units_and_recovers_on_the_next_timestamp() {
        let mut rx = VideoAssembly::default();
        let nal = [0x65, 0xa5];
        assert_eq!(rx.push(u16::MAX, 10, false, &nal), None);
        assert_eq!(
            rx.push(0, 10, true, &nal),
            Some(vec![0, 0, 0, 1, 0x65, 0xa5, 0, 0, 0, 1, 0x65, 0xa5])
        );
        assert_eq!(rx.push(0, 10, true, &nal), None);
        assert_eq!(rx.push(1, 20, false, &nal), None);
        assert_eq!(rx.push(3, 20, true, &nal), None);
        assert_eq!(rx.push(2, 20, false, &nal), None);
        assert_eq!(
            rx.push(4, 30, true, &nal),
            Some(vec![0, 0, 0, 1, 0x65, 0xa5])
        );
    }
}
