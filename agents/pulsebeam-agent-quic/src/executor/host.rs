use std::{
    collections::{BTreeMap, VecDeque},
    future::{Future, pending},
    io,
    net::IpAddr,
    num::{NonZeroU16, NonZeroU32},
    pin::Pin,
    time::Duration,
};

use noq_proto::ClientConfig;
use pulsebeam_agent_core::{
    Agent as CoreAgent, AgentCommand, AgentConfig, AgentError, Effect, Failure, FailureClass,
    Generation, HostEvent, HttpEffect, HttpEvent, HttpMethod, HttpRequest, HttpResponse, MediaKind,
    Notification, OperationId, QuicEffect, QuicEvent, Snapshot, TimerEffect, TimerEvent, TimerId,
};
use pulsebeam_core::{
    fabric::{
        FabricPacket, Header, Profile,
        packet::{Agreement, BASE_BYTES, MAX_PACKET_BYTES},
    },
    h264::{Depacketizer, MAX_ACCESS_UNIT_SIZE, Packetization, PacketizedChunk, Packetizer},
};
use rustls::RootCertStore;
use tokio::time::{self, Instant};

use crate::{
    data_bindings::FabricOffer,
    records::{Datagram, DatagramKind, PictureLossIndication},
    transport::{
        self, DatagramAdmission,
        framing::{SendError, SendProgress, Sender, WHOLE_CAPACITY},
    },
};

use super::{
    Event, ReceiveBinding,
    rtp::{Binding, Publication, Receive, ReceivedPacket, SendPacket, SendPacketError},
    udp::{OpenError, UdpAssociation},
};

const PLACEMENT_BODY_LIMIT: usize = 1_024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const EFFECTS_PER_TURN: usize = 32;
const RTP_HEADER: usize = 12;
const MAX_VIDEO_SEND_AGE: Duration = Duration::from_millis(120);

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
    accepted_at: Instant,
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
    pub first_sequence: u16,
    pub last_sequence: u16,
    pub access_unit: Vec<u8>,
}

#[derive(Default)]
struct VideoAssembly {
    depacketizer: Depacketizer,
    last: Option<(u16, u32)>,
    first_sequence: u16,
    discard: bool,
}

impl VideoAssembly {
    fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<(u16, Vec<u8>)> {
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
        if start {
            self.first_sequence = sequence;
        }
        match self.depacketizer.push(payload, start, marker) {
            Ok(Some(frame)) => {
                self.discard = true;
                Some((self.first_sequence, frame))
            }
            Ok(None) => None,
            Err(_) => {
                self.discard = true;
                None
            }
        }
    }
}

const MAX_QUEUED_RTP: usize = 32;
const MAX_QUEUED_OPUS: usize = 32;
const MAX_QUEUED_H264: usize = 2;
const MAX_H264_ASSEMBLIES: usize = 8;

struct MediaSender {
    generation: Generation,
    binding: NonZeroU16,
    sequence: u32,
    automatic_sequence: Option<bool>,
    agreement: Agreement,
    framer: Sender,
}

/// Caller-driven native Agent. Run its turns on the reactor that owns the UDP
/// sockets. It does not dispatch a background task or retain an RTC host.
pub struct Agent {
    core: CoreAgent,
    http: reqwest::Client,
    tls: ClientConfig,
    local: IpAddr,
    clock_origin: Instant,
    request: Option<(OperationId, Work<HttpEvent>)>,
    dialing: Option<(Generation, Work<DialResult>)>,
    sockets: [Option<Box<UdpAssociation>>; 2],
    media_senders: BTreeMap<(MediaKind, String), MediaSender>,
    pending_video: Option<PendingVideo>,
    video_refresh: VecDeque<VideoRefresh>,
    video_assemblies: BTreeMap<(Generation, NonZeroU16), VideoAssembly>,
    received_h264: VecDeque<(Generation, NonZeroU16, ReceivedH264)>,
    received_opus: VecDeque<(Generation, NonZeroU16, ReceivedOpus)>,
    raw_rtp: Option<RawRtp>,
    timers: BTreeMap<TimerId, Instant>,
    #[cfg(feature = "sim")]
    sim: Option<SimServices>,
}

#[derive(Default)]
struct RawRtp {
    packets: VecDeque<ReceivedPacket>,
    dropped: u64,
}

impl Agent {
    pub fn new(
        config: AgentConfig,
        local: IpAddr,
        quic_roots: RootCertStore,
    ) -> Result<Self, Error> {
        let core = CoreAgent::new_quic(config).map_err(Error::Core)?;
        let http = Self::placement_client().build().map_err(Error::Http)?;
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
            media_senders: BTreeMap::new(),
            pending_video: None,
            video_refresh: VecDeque::new(),
            video_assemblies: BTreeMap::new(),
            received_h264: VecDeque::new(),
            received_opus: VecDeque::new(),
            raw_rtp: None,
            timers: BTreeMap::new(),
            #[cfg(feature = "sim")]
            sim: None,
        })
    }

    /// Receive complete RTP packets instead of assembling Opus/H.264 frames.
    /// Select this before connecting so no packets go through codec assembly.
    #[must_use]
    pub fn with_raw_rtp(mut self) -> Self {
        self.raw_rtp.get_or_insert_with(RawRtp::default);
        self.video_assemblies.clear();
        self.received_h264.clear();
        self.received_opus.clear();
        self
    }

    /// Poll the raw receive queue in completion order, without a jitter wait.
    /// Packets from retired or disabled bindings are not returned.
    pub fn next_rtp(&mut self) -> Option<Receive> {
        let raw = self.raw_rtp.as_mut()?;
        if raw.dropped != 0 {
            return Some(Receive::Overflow {
                dropped: std::mem::take(&mut raw.dropped),
            });
        }
        while let Some(packet) = raw.packets.pop_front() {
            if self.core.quic_rtp_receivers(packet.kind).any(|live| {
                live.generation == packet.binding.generation
                    && live.binding == packet.binding.id
                    && live.publication_id == packet.publication_id
            }) {
                return Some(Receive::Packet(packet));
            }
        }
        None
    }

    fn placement_client() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
    }

    /// Add a trust anchor for the configured HTTPS placement origin. Normal
    /// certificate and hostname verification still applies.
    pub fn with_placement_root(mut self, root_der: &[u8]) -> Result<Self, Error> {
        let root = reqwest::Certificate::from_der(root_der).map_err(Error::Http)?;
        self.http = Self::placement_client()
            .add_root_certificate(root)
            .build()
            .map_err(Error::Http)?;
        Ok(self)
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
    /// The timestamp is in the caller's 48 kHz RTP clock. On success the
    /// carrier owns the packet. Drive Agent turns to finish any sliced send.
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
        self.send_rtp_inner(
            kind,
            track_id,
            SendPacket {
                sequence: 0,
                timestamp,
                marker,
                extensions: &[],
                payload,
            },
            None,
        )
        .map_err(|error| match error {
            SendPacketError::NotReady | SendPacketError::SequenceOwner => OpusSendError::NotReady,
            SendPacketError::Invalid(_) => OpusSendError::Datagram(DatagramAdmission::Size),
            SendPacketError::Datagram(reason) => OpusSendError::Datagram(reason),
        })
    }

    pub fn rtp_publication(&self, kind: MediaKind, track_id: &str) -> Option<Publication> {
        let live = self
            .core
            .quic_rtp_publication(kind, track_id)
            .filter(|live| live.ready)?;
        Some(Publication {
            binding: Binding {
                generation: live.generation,
                id: live.binding,
            },
            kind,
            track_id: track_id.to_owned(),
            publication_id: live.publication_id.to_owned(),
            ssrc: live.ssrc,
            payload_type: live.payload_type,
        })
    }

    /// Submit one already packetized RTP payload to this installed publication.
    /// If the binding was replaced, resolve a new publication and reset source state.
    /// On backpressure no part was accepted; retry the same packet later.
    /// Success transfers the packet to bounded staging, not delivery to the peer.
    pub fn send_rtp(
        &mut self,
        publication: &Publication,
        packet: SendPacket<'_>,
    ) -> Result<(), SendPacketError> {
        self.send_rtp_inner(
            publication.kind,
            &publication.track_id,
            packet,
            Some(publication),
        )
    }

    fn send_rtp_inner(
        &mut self,
        kind: MediaKind,
        track_id: &str,
        input: SendPacket<'_>,
        expected: Option<&Publication>,
    ) -> Result<(), SendPacketError> {
        use pulsebeam_core::fabric::WireError;

        if input.payload.len().saturating_add(BASE_BYTES) > MAX_PACKET_BYTES {
            return Err(SendPacketError::Invalid(WireError::Oversize));
        }
        let live = self
            .core
            .quic_rtp_publication(kind, track_id)
            .filter(|publication| publication.ready)
            .ok_or(SendPacketError::NotReady)?;
        let (generation, binding) = (live.generation, live.binding);
        if expected.is_some_and(|source| {
            source.binding.generation != generation
                || source.binding.id != binding
                || source.publication_id != live.publication_id
                || source.ssrc != live.ssrc
                || source.payload_type != live.payload_type
        }) {
            return Err(SendPacketError::NotReady);
        }
        let generate_sequence = expected.is_none();
        let key = (kind, track_id.to_owned());
        if !self.media_senders.contains_key(&key) && self.media_senders.len() >= 64 {
            self.retain_media_senders();
            if self.media_senders.len() >= 64 {
                return Err(SendPacketError::NotReady);
            }
        }
        let socket = self
            .sockets
            .iter_mut()
            .flatten()
            .find(|socket| socket.generation() == generation)
            .ok_or(SendPacketError::NotReady)?;
        let profile = match kind {
            MediaKind::Audio => Profile::Opus,
            MediaKind::Video => Profile::H264,
        };
        if !self
            .media_senders
            .get(&key)
            .is_some_and(|sender| sender.generation == generation && sender.binding == binding)
        {
            let offer = socket.peer_fabric().ok_or(SendPacketError::NotReady)?;
            let selected = FabricOffer::CURRENT
                .bind(offer, profile, 0)
                .map_err(SendPacketError::Invalid)?;
            self.media_senders.insert(
                key.clone(),
                MediaSender {
                    generation,
                    binding,
                    sequence: 0,
                    automatic_sequence: None,
                    agreement: selected.agreement,
                    framer: Sender::new(selected.framing, binding),
                },
            );
        }
        let sender = self
            .media_senders
            .get_mut(&key)
            .ok_or(SendPacketError::NotReady)?;
        if sender
            .automatic_sequence
            .is_some_and(|automatic| automatic != generate_sequence)
        {
            return Err(SendPacketError::SequenceOwner);
        }
        if sender.framer.is_pending() {
            return Err(SendPacketError::Datagram(DatagramAdmission::Full));
        }
        let header = Header {
            profile,
            marker: input.marker,
            sequence: if generate_sequence {
                sender.sequence
            } else {
                input.sequence
            },
            timestamp: input.timestamp,
        };
        let packet = FabricPacket::new(header, input.extensions, input.payload)
            .map_err(SendPacketError::Invalid)?;
        let mut wire = [0; MAX_PACKET_BYTES];
        let size = sender
            .agreement
            .write(&packet, &mut wire)
            .map_err(SendPacketError::Invalid)?;
        let now = Instant::now().into_std();
        let bytes = wire
            .get(..size)
            .ok_or(SendPacketError::Invalid(WireError::Oversize))?;
        sender
            .framer
            .start(now, Box::from(bytes))
            .map_err(media_send_error)?;
        match socket
            .offer_media_packet(&mut sender.framer, now)
            .map_err(media_send_error)?
        {
            SendProgress::Admitted | SendProgress::Partial => {
                sender.automatic_sequence = Some(generate_sequence);
                if generate_sequence {
                    sender.sequence = sender.sequence.wrapping_add(1);
                }
                Ok(())
            }
            SendProgress::Deferred(reason) => {
                sender.framer.discard_pending();
                Err(SendPacketError::Datagram(reason))
            }
            SendProgress::Idle => Err(SendPacketError::NotReady),
        }
    }

    fn retain_media_senders(&mut self) {
        self.media_senders.retain(|(kind, track), sender| {
            self.core
                .quic_rtp_publication(*kind, track)
                .is_some_and(|live| {
                    if !live.ready {
                        sender.framer.discard_pending();
                    }
                    sender.generation == live.generation && sender.binding == live.binding
                })
        });
    }

    fn offer_pending_media(&mut self) -> Option<Generation> {
        self.retain_media_senders();
        let now = Instant::now().into_std();
        for sender in self
            .media_senders
            .values_mut()
            .filter(|sender| sender.framer.is_pending())
        {
            let Some(socket) = self
                .sockets
                .iter_mut()
                .flatten()
                .find(|socket| socket.generation() == sender.generation)
            else {
                sender.framer.discard_pending();
                continue;
            };
            if let Ok(
                SendProgress::Partial | SendProgress::Deferred(DatagramAdmission::ControlTurn),
            ) = socket.offer_media_packet(&mut sender.framer, now)
            {
                return Some(sender.generation);
            }
        }
        None
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
        if !self
            .sockets
            .iter()
            .flatten()
            .any(|socket| socket.generation() == live.generation && socket.peer_fabric().is_some())
        {
            return Err(H264SendError::NotReady);
        }
        // The packetizer's ordinary output fits a Whole frame on every path.
        let mut packets = Packetizer::new(WHOLE_CAPACITY - BASE_BYTES).into_packets(access_unit);
        let next = packets.next().ok_or(H264SendError::InvalidAccessUnit)?;
        self.pending_video = Some(PendingVideo {
            track_id: track_id.into(),
            generation: live.generation,
            binding: live.binding,
            timestamp,
            accepted_at: Instant::now(),
            packets,
            next,
        });
        Ok(())
    }

    fn offer_pending_video(&mut self) -> Option<Generation> {
        use transport::DatagramAdmission::{ControlTurn, Full};
        let mut video = self.pending_video.take()?;
        if video.accepted_at.elapsed() >= MAX_VIDEO_SEND_AGE {
            return None;
        }
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
        let last_sequence = u16::from_be_bytes([*a, *b]);
        if let Some((first_sequence, access_unit)) = self
            .video_assemblies
            .entry(key)
            .or_default()
            .push(last_sequence, timestamp, *pt & 0x80 != 0, payload)
        {
            if self.received_h264.len() >= MAX_QUEUED_H264 {
                let _ = self.received_h264.pop_front();
            }
            self.received_h264.push_back((
                generation,
                binding,
                ReceivedH264 {
                    publication_id: live.publication_id.into(),
                    timestamp,
                    first_sequence,
                    last_sequence,
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

    /// Move the active QUIC association to a new same-family local socket.
    /// This starts path validation, not a new admission. Keep driving Agent
    /// turns for validation or normal authenticated recovery on transport loss.
    pub async fn rebind(&mut self, local: IpAddr) -> io::Result<()> {
        let generation = self.core.snapshot().generation.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "no current QUIC association")
        })?;
        let socket = self
            .sockets
            .iter_mut()
            .flatten()
            .find(|socket| socket.generation() == generation)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no current QUIC socket"))?;
        socket.rebind(local).await?;
        self.local = local;
        Ok(())
    }

    /// Per-association DATA executor staging high-water marks and wait.
    #[cfg(feature = "sim")]
    pub fn data_staging(&self) -> Option<super::DataStaging> {
        let generation = self.core.snapshot().generation?;
        self.sockets
            .iter()
            .flatten()
            .find(|socket| socket.generation() == generation)
            .map(|socket| socket.data_staging())
    }

    pub fn catalog_watch(
        &mut self,
        room: String,
        config: pulsebeam_agent_core::ffi::WatchConfig,
    ) -> Result<pulsebeam_agent_core::TrackOwner, pulsebeam_agent_core::ffi::ErrorInfo> {
        self.core.catalog_watch(room, config)
    }

    pub fn catalog_watch_state(
        &self,
        owner: pulsebeam_agent_core::TrackOwner,
    ) -> Option<pulsebeam_agent_core::ffi::WatchState> {
        self.core.catalog_watch_state(owner)
    }

    pub fn close_catalog_watch(
        &mut self,
        owner: pulsebeam_agent_core::TrackOwner,
    ) -> Option<pulsebeam_agent_core::ffi::WatchState> {
        self.core.close_catalog_watch(owner)
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

    fn accept_socket(&mut self, event: Event) -> Result<(), Error> {
        let (generation, binding, arrived_at, ssrc, payload_type, packet) = match event {
            Event::Core(event) => return self.accept(HostEvent::Quic(event)),
            Event::Rtp {
                generation,
                binding,
                arrived_at,
                ssrc,
                payload_type,
                packet,
            } => (generation, binding, arrived_at, ssrc, payload_type, packet),
        };
        let header = packet.header();
        let kind = match header.profile {
            Profile::Opus => MediaKind::Audio,
            Profile::H264 => MediaKind::Video,
            _ => return Ok(()),
        };
        if let Some(raw) = &mut self.raw_rtp {
            let Some(live) = self
                .core
                .quic_rtp_receivers(kind)
                .find(|live| live.generation == generation && live.binding == binding)
            else {
                return Ok(());
            };
            if raw.packets.len() >= MAX_QUEUED_RTP {
                raw.dropped = raw.dropped.saturating_add(1);
            } else {
                raw.packets.push_back(ReceivedPacket {
                    binding: Binding {
                        generation,
                        id: binding,
                    },
                    publication_id: live.publication_id.to_owned(),
                    kind,
                    arrived_at,
                    ssrc,
                    payload_type,
                    packet,
                });
            }
        } else {
            // The compatibility frame adapters consume minimal RTP. Only the
            // raw boundary exposes negotiated extensions to the application.
            let mut bytes = Vec::with_capacity(RTP_HEADER.saturating_add(packet.payload().len()));
            bytes.extend_from_slice(&[0x80, payload_type | (u8::from(header.marker) << 7)]);
            let [.., a, b] = header.sequence.to_be_bytes();
            bytes.extend_from_slice(&[a, b]);
            bytes.extend_from_slice(&header.timestamp.to_be_bytes());
            bytes.extend_from_slice(&ssrc.to_be_bytes());
            bytes.extend_from_slice(packet.payload());
            match kind {
                MediaKind::Audio => self.receive_opus(generation, binding, &bytes),
                MediaKind::Video => self.receive_h264(generation, binding, &bytes),
            }
        }
        Ok(())
    }

    fn accept(&mut self, event: HostEvent) -> Result<(), Error> {
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
        let retry_payload_turn = self
            .offer_pending_media()
            .or_else(|| self.offer_pending_video());
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
        let input = tokio::select! {
            http = work(&mut self.request) => Input::Http(http),
            dial = work(&mut self.dialing) => Input::Dial(dial),
            timer = timer => Input::Timer(timer),
            event = socket_turn(first, &self.core, retry_payload_turn) => Input::Socket(event),
            event = socket_turn(second, &self.core, retry_payload_turn) => Input::Socket(event),
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
            Input::Socket(Some(event)) => self.accept_socket(event)?,
            Input::Socket(None) => {}
        }
        Ok(())
    }
}

fn media_send_error(error: SendError) -> SendPacketError {
    SendPacketError::Datagram(match error {
        SendError::Size => DatagramAdmission::Size,
        SendError::Busy => DatagramAdmission::Full,
        _ => DatagramAdmission::Unsupported,
    })
}

enum Input {
    Http(HttpEvent),
    Dial(DialResult),
    Timer(TimerId),
    Socket(Option<Event>),
}

async fn work<Id, T>(work: &mut Option<(Id, Work<T>)>) -> T {
    match work {
        Some((_, future)) => future.as_mut().await,
        None => pending().await,
    }
}

async fn socket_turn(
    slot: &mut Option<Box<UdpAssociation>>,
    core: &CoreAgent,
    retry_payload_turn: Option<Generation>,
) -> Option<Event> {
    let Some(socket) = slot.as_mut() else {
        return pending().await;
    };
    let generation = socket.generation();
    let retry = retry_payload_turn == Some(generation);
    let authorize = |binding| {
        if let Some((_, ready)) = core.quic_data_receiver(generation, binding) {
            return Some((ReceiveBinding::Data, ready));
        }
        [
            (MediaKind::Audio, Profile::Opus),
            (MediaKind::Video, Profile::H264),
        ]
        .into_iter()
        .find_map(|(kind, profile)| {
            let receiver = core
                .quic_rtp_receiver_bindings(kind)
                .find(|live| live.generation == generation && live.binding == binding)?;
            Some((
                ReceiveBinding::Media {
                    profile,
                    ssrc: receiver.ssrc,
                    payload_type: receiver.payload_type,
                },
                receiver.ready,
            ))
        })
    };
    let Ok(event) = socket.progress(retry, authorize).await else {
        let event = socket.io_failure();
        *slot = None;
        return Some(event.into());
    };
    if matches!(event, Some(Event::Core(QuicEvent::Closed { .. }))) {
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
            Some((
                u16::MAX,
                vec![0, 0, 0, 1, 0x65, 0xa5, 0, 0, 0, 1, 0x65, 0xa5]
            ))
        );
        assert_eq!(rx.push(0, 10, true, &nal), None);
        assert_eq!(rx.push(1, 20, false, &nal), None);
        assert_eq!(rx.push(3, 20, true, &nal), None);
        assert_eq!(rx.push(2, 20, false, &nal), None);
        assert_eq!(
            rx.push(4, 30, true, &nal),
            Some((4, vec![0, 0, 0, 1, 0x65, 0xa5]))
        );
    }
}
