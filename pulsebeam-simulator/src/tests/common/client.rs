use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use openh264::decoder::Decoder as H264Decoder;
use openh264::formats::YUVSource;
use pulsebeam_agent::actor::AgentBuilder;
use pulsebeam_agent::agent::{
    DataPublisher, DataSubscriber, OrderedTopicPublisher, OrderedTopicSubscriber,
};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::{AudioLooper, H264Looper, VbrLooper, VbrProfile};
use pulsebeam_agent::{
    Agent, LocalTrack, ParticipantChange, Participants, RemoteTrack, SimulcastLayer,
};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;
// The process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per
// host, so a timestamp taken here cannot be compared with one taken on the coordinator. See
// `sim_clock`, which shims `clock_gettime` for the whole process.
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub struct SimClientBuilder {
    ip: IpAddr,
    agent_builder: AgentBuilder,
    video_rx: Option<Arc<Mutex<VideoReceiveLog>>>,
    audio_rx: Option<Arc<Mutex<AudioReceiveLog>>>,
    paused_publishers: Option<Arc<Mutex<std::collections::BTreeSet<String>>>>,
    publishes_video: bool,
    /// When set, publish with a variable-bitrate source instead of the constant-rate looper.
    vbr_profile: Option<VbrProfile>,
    /// When set, attach a synthetic L1T{n} temporal Dependency Descriptor per frame.
    temporal_dd: Option<u8>,
    /// Publish audio at this loudness, in negative dBov. `None` publishes no audio.
    audio_level_dbov: Option<i8>,
    audio_phase_offset: u64,
    receives_audio: bool,
    /// Make the payload opaque (SFrame/E2EE) so the SFU forwards on DD alone.
    opaque_payload: bool,
    reject_vp8: bool,
}

fn http_base_uri(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

fn unspecified_addr(ip: IpAddr) -> SocketAddr {
    match ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

impl SimClientBuilder {
    pub async fn bind(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = http_base_uri(server_ip, 7070);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind(unspecified_addr(ip)).await?;

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket).with_local_ip(ip),
            video_rx: None,
            audio_rx: None,
            paused_publishers: None,
            publishes_video: false,
            vbr_profile: None,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            receives_audio: false,
            opaque_payload: false,
            reject_vp8: false,
        })
    }

    /// Like `bind` but also configures a TCP active stream to the server's ICE
    /// port (3478).  Use with `start_sfu_node_tcp_only` to test TCP connectivity.
    pub async fn bind_tcp(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = http_base_uri(server_ip, 7070);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind(unspecified_addr(ip)).await?;
        let server_tcp_addr = std::net::SocketAddr::new(server_ip, 3478);

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket)
                .with_local_ip(ip)
                .with_tcp_server_addr(server_tcp_addr),
            video_rx: None,
            audio_rx: None,
            paused_publishers: None,
            publishes_video: false,
            vbr_profile: None,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            receives_audio: false,
            opaque_payload: false,
            reject_vp8: false,
        })
    }

    pub fn publish_video(mut self, simulcast_layers: Option<Vec<SimulcastLayer>>) -> Self {
        self.agent_builder = self.agent_builder.video_upstream_slots(1, simulcast_layers);
        self.publishes_video = true;
        self
    }

    /// Receive audio, reserving `capacity` downstream slots.
    ///
    /// The SFU forwards only the loudest few speakers, so this is how many it can send at once -
    /// the receiving end of `TopNAudioSelector`'s slots.
    pub fn receive_audio(mut self, capacity: usize) -> Self {
        self.agent_builder = self.agent_builder.audio_downstream_slots(capacity);
        self.receives_audio = true;
        self
    }

    /// Publish audio at the given loudness in negative dBov: around -30 is ordinary speech,
    /// below about -60 reads as a quiet room.
    pub fn publish_audio(mut self, level_dbov: i8, phase_offset: u64) -> Self {
        self.agent_builder = self.agent_builder.audio_upstream_slots(1);
        self.audio_level_dbov = Some(level_dbov);
        self.audio_phase_offset = phase_offset;
        self
    }

    /// Publish with a VBR source (see [`VbrProfile`]) rather than the constant-rate looper.
    pub fn with_vbr(mut self, profile: VbrProfile) -> Self {
        self.vbr_profile = Some(profile);
        self
    }

    /// Attach a synthetic L1T{layers} temporal Dependency Descriptor per frame.
    pub fn with_temporal_dd(mut self, layers: u8) -> Self {
        self.temporal_dd = Some(layers);
        self
    }

    /// Make the published payload opaque, simulating SFrame/E2EE.
    pub fn with_opaque_payload(mut self) -> Self {
        self.opaque_payload = true;
        self
    }

    pub fn receive_video(mut self, capacity: usize) -> Self {
        self.agent_builder = self.agent_builder.video_downstream_slots(capacity);
        self
    }

    pub fn manual_subscriptions(mut self) -> Self {
        self.agent_builder = self.agent_builder.manual_subscriptions();
        self
    }

    /// Model a marker/deep-inspection-only peer that never negotiates DD.
    pub fn without_dependency_descriptor(mut self) -> Self {
        self.agent_builder = self.agent_builder.without_dependency_descriptor();
        self
    }

    pub fn prefer_vp8(mut self) -> Self {
        self.agent_builder = self.agent_builder.prefer_vp8();
        self.reject_vp8 = true;
        self
    }

    /// Inject a shared `VideoReceiveLog` so the harness can read it externally.
    /// If not called, `connect()` allocates a private one.
    pub fn with_paused_publishers(
        mut self,
        seen: Arc<Mutex<std::collections::BTreeSet<String>>>,
    ) -> Self {
        self.paused_publishers = Some(seen);
        self
    }

    pub fn with_audio_rx(mut self, rx: Arc<Mutex<AudioReceiveLog>>) -> Self {
        self.audio_rx = Some(rx);
        self
    }

    pub fn with_video_rx(mut self, rx: Arc<Mutex<VideoReceiveLog>>) -> Self {
        self.video_rx = Some(rx);
        self
    }

    pub async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let (agent, runner) = self.agent_builder.connect_unmanaged(room).await?;
        let mut join_set = JoinSet::new();
        join_set.spawn(async move {
            runner.run().await.expect("agent runner failed");
        });
        let local_video = if self.publishes_video {
            Some(agent.media().publish_video().await?)
        } else {
            None
        };
        let local_audio = match self.audio_level_dbov {
            Some(level) => Some((agent.media().publish_audio().await?, level)),
            None => None,
        };
        let (incoming_track_tx, incoming_tracks) = tokio::sync::mpsc::channel(32);
        let participants = agent.participants();
        let audio_tracks = if self.receives_audio {
            // Registered before anything is heard: audio has no per-speaker subscription, so
            // whoever the SFU picks arrives unasked and there is nowhere to put them otherwise.
            Some(agent.media().receive_audio().await?)
        } else {
            None
        };
        let speakers = self.receives_audio.then(|| agent.media().speakers());
        tracing::info!("connected to {room}");
        let video_rx = self
            .video_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(VideoReceiveLog::default())));
        let audio_rx = self
            .audio_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(AudioReceiveLog::default())));
        let ctx_paused_publishers = self
            .paused_publishers
            .unwrap_or_else(|| Arc::new(Mutex::new(std::collections::BTreeSet::new())));
        let mut ctx = ClientContext {
            ip: self.ip,
            agent,
            incoming_tracks,
            incoming_track_tx,
            participants,
            discovered_tracks: HashSet::new(),
            published_topics: Arc::new(Mutex::new(HashMap::new())),
            subscribed_topics: Arc::new(Mutex::new(HashMap::new())),
            ordered_publishers: Arc::new(Mutex::new(HashMap::new())),
            ordered_subscribers: Arc::new(Mutex::new(HashMap::new())),
            remote_tracks: HashMap::new(),
            paused_publishers: ctx_paused_publishers,
            requested_tracks: HashSet::new(),
            received_data: Vec::new(),
            reject_vp8: self.reject_vp8,
            video_rx,
            audio_rx,
            local_publications: local_video.into_iter().collect(),
        };
        if let Some(mut audio_tracks) = audio_tracks {
            let log = ctx.audio_rx.clone();
            join_set.spawn(async move {
                while let Ok(mut track) = audio_tracks.next().await {
                    let publisher = track.publisher_id().to_owned();
                    let log = log.clone();
                    tokio::spawn(async move {
                        let mut receiver = BrowserAudioReceiver::new();
                        while let Ok(rtp) = track.recv().await {
                            receiver.push(rtp, &publisher, &mut log.lock().unwrap());
                        }
                    });
                }
            });
        }
        if let Some(mut speakers) = speakers {
            let log = ctx.audio_rx.clone();
            join_set.spawn(async move {
                loop {
                    for speaker in speakers.current().iter() {
                        log.lock()
                            .unwrap()
                            .record_rank(&speaker.participant_id, speaker.rank);
                    }
                    if speakers.changed().await.is_err() {
                        return;
                    }
                }
            });
        }
        for publication in &ctx.local_publications {
            for sender in publication.encodings().iter().cloned() {
                let rid = sender.rid();
                match self.vbr_profile {
                    Some(profile) => {
                        let looper = create_vbr_looper_for_rid(rid, profile);
                        join_set.spawn(looper.run(sender));
                    }
                    None => {
                        let mut looper = create_h264_looper_for_rid(rid);
                        if let Some(layers) = self.temporal_dd {
                            looper = looper.with_temporal_layers(layers);
                        }
                        if self.opaque_payload {
                            looper = looper.with_opaque_payload();
                        }
                        join_set.spawn(looper.run(sender));
                    }
                }
            }
        }
        if let Some((publication, level)) = local_audio {
            for sender in publication.encodings().iter().cloned() {
                join_set.spawn(
                    AudioLooper::speaking()
                        .with_level_dbov(level)
                        .with_phase_offset(self.audio_phase_offset)
                        .run(sender),
                );
            }
            // The handle has to outlive the loopers. Dropping a `LocalTrack` unpublishes it, so
            // letting it fall out of scope here declared the track inactive the moment it was
            // created: the packets still flowed into cloned senders, and the SFU - told the mid
            // was inactive - never registered a track to route them to.
            ctx.local_publications.push(publication);
        }
        Ok(SimClient { ctx, join_set })
    }
}

/// What the subscriber's depacketizer made of the stream the SFU sent it.
///
/// `contiguous` is str0m's own reassembly verdict: it is false whenever a frame
/// was preceded by a sequence-number hole, which is exactly what a botched
/// switch produces. `is_keyframe` lets a test assert that each switch actually
/// delivered a decodable entry point.
#[derive(Default, Debug, Clone)]
pub struct VideoReceiveLog {
    pub by_publisher: BTreeMap<String, u64>,
    pub frames: u64,
    pub keyframes: u64,
    pub non_contiguous: u64,
    /// Frames whose RTP timestamp had already been used by an earlier frame.
    /// str0m re-emits a frame when a retransmission for it lands after it was
    /// already delivered, so this is bounded by the NACK count rather than zero.
    pub duplicate_ts_frames: u64,
    /// Number of backwards RTP-timestamp jumps seen. Ordinary reordering may
    /// cause one; a broken simulcast switch causes one per botched transition.
    pub ts_regression_count: u64,
    /// Largest backwards jump in RTP time, in 90kHz ticks.
    pub max_ts_regression: u64,
    pub undecodable_keyframes: u64,
    pub decoder_errors: u64,
    pub unexpected_vp8_packets: u64,
    pub missing_mid_packets: u64,
    pub missing_ssrc_packets: u64,
    pub missing_payload_type_packets: u64,
    pub changed_ssrc_packets: u64,
    pub changed_payload_type_packets: u64,
    pub crossed_frame_boundaries: u64,
    pub unterminated_frames: u64,
    /// When the very first frame reached the decoder. Time-to-first-frame is measured from this
    /// against the moment the viewer subscribed, which only the harness knows.
    pub first_frame_at: Option<Instant>,
    /// Time spent in stretches longer than [`FREEZE_THRESHOLD`] with no frame.
    ///
    /// Distinct from the longest gap: one ten-second freeze and fifty two-hundred-millisecond
    /// freezes are different experiences and a maximum cannot tell them apart. Short gaps are
    /// excluded because ordinary jitter and a keyframe wait are not freezes.
    pub frozen_time: Duration,
    /// Longest wall gap between consecutive delivered frames.
    ///
    /// Measured per frame rather than per plan step, because a step is tens of seconds long and a
    /// freeze inside one still leaves bytes in the window: sampled at step boundaries this reads
    /// zero however badly the stream stalled. A viewer notices the gap, not the total.
    pub longest_frame_gap: Duration,
    last_frame_at: Option<Instant>,
    last_ts: Option<u64>,
    seen_ts: HashSet<u64>,
}

struct BrowserVideoReceiver {
    decoder: H264Decoder,
    jitter: pulsebeam_agent::JitterBuffer,
    expected_ssrc: Option<u32>,
    expected_payload_type: Option<u8>,
    open_frame_timestamp: Option<u64>,
    rejects_vp8_payload_type: bool,
    expected_seq: Option<u64>,
    access_unit: Vec<u8>,
    fu_header: Option<u8>,
    frame_is_keyframe: bool,
    frame_damaged: bool,
    pending_gap: bool,
    has_rendered: bool,
}

impl BrowserVideoReceiver {
    fn new(rejects_vp8_payload_type: bool) -> Self {
        Self {
            decoder: H264Decoder::new().expect("bundled OpenH264 decoder initializes"),
            jitter: pulsebeam_agent::JitterBuffer::new(
                pulsebeam_agent::pipeline::DEFAULT_JITTER_MAX_WAIT,
            ),
            expected_ssrc: None,
            expected_payload_type: None,
            open_frame_timestamp: None,
            rejects_vp8_payload_type,
            expected_seq: None,
            access_unit: Vec::with_capacity(16 * 1024),
            fu_header: None,
            frame_is_keyframe: false,
            frame_damaged: false,
            pending_gap: false,
            has_rendered: false,
        }
    }

    fn push(
        &mut self,
        rtp: pulsebeam_agent::RtpPacket,
        log: &mut VideoReceiveLog,
        publisher: &str,
    ) {
        if rtp.mid.to_string().is_empty() {
            log.missing_mid_packets = log.missing_mid_packets.saturating_add(1);
        }
        let ssrc = rtp.ssrc.map(|ssrc| *ssrc);
        match (self.expected_ssrc, ssrc) {
            (_, None) => {
                log.missing_ssrc_packets = log.missing_ssrc_packets.saturating_add(1);
            }
            (Some(expected), Some(actual)) if expected != actual => {
                log.changed_ssrc_packets = log.changed_ssrc_packets.saturating_add(1);
            }
            (None, Some(actual)) => self.expected_ssrc = Some(actual),
            _ => {}
        }
        match (self.expected_payload_type, rtp.payload_type) {
            (_, None) => {
                log.missing_payload_type_packets =
                    log.missing_payload_type_packets.saturating_add(1);
            }
            (Some(expected), Some(actual)) if expected != actual => {
                log.changed_payload_type_packets =
                    log.changed_payload_type_packets.saturating_add(1);
            }
            (None, Some(actual)) => self.expected_payload_type = Some(actual),
            _ => {}
        }
        if self.rejects_vp8_payload_type && rtp.payload_type == Some(96) {
            log.unexpected_vp8_packets = log.unexpected_vp8_packets.saturating_add(1);
        }
        self.jitter.push(rtp);
        while let Some(rtp) = self.jitter.pop() {
            self.process(rtp, log, publisher);
        }
    }

    fn process(
        &mut self,
        rtp: pulsebeam_agent::RtpPacket,
        log: &mut VideoReceiveLog,
        publisher: &str,
    ) {
        let sequence = *rtp.seq;
        if self
            .expected_seq
            .is_some_and(|expected| expected != sequence)
            && !self.access_unit.is_empty()
        {
            self.frame_damaged = true;
        }
        self.expected_seq = Some(sequence.wrapping_add(1));

        let timestamp = rtp.ts.numer();
        if self
            .open_frame_timestamp
            .is_some_and(|open| open != timestamp)
        {
            log.crossed_frame_boundaries = log.crossed_frame_boundaries.saturating_add(1);
        }
        if self
            .open_frame_timestamp
            .is_some_and(|open| open != timestamp)
        {
            self.finish(log, publisher, timestamp);
        }
        self.open_frame_timestamp = Some(timestamp);

        if !self.append_rtp_payload(&rtp.payload) {
            self.frame_damaged = true;
            log.decoder_errors = log.decoder_errors.saturating_add(1);
        }
        if rtp.marker {
            self.finish(log, publisher, timestamp);
        }
    }

    fn flush(&mut self, log: &mut VideoReceiveLog, publisher: &str) {
        let remaining: Vec<_> = self.jitter.drain().collect();
        for rtp in remaining {
            self.process(rtp, log, publisher);
        }
        if let Some(timestamp) = self.open_frame_timestamp.take() {
            log.unterminated_frames = log.unterminated_frames.saturating_add(1);
            self.finish(log, publisher, timestamp);
        }
    }

    fn append_rtp_payload(&mut self, payload: &[u8]) -> bool {
        let Some(&header) = payload.first() else {
            return false;
        };
        match header & 0x1f {
            1..=23 => self.append_nalu(payload),
            24 => {
                let mut offset = 1usize;
                while offset < payload.len() {
                    let Some(length) = payload
                        .get(offset..offset.saturating_add(2))
                        .and_then(|bytes| <&[u8; 2]>::try_from(bytes).ok())
                        .map(|bytes| usize::from(u16::from_be_bytes(*bytes)))
                    else {
                        return false;
                    };
                    offset = offset.saturating_add(2);
                    let Some(nalu) = payload.get(offset..offset.saturating_add(length)) else {
                        return false;
                    };
                    if nalu.is_empty() {
                        return false;
                    }
                    self.append_nalu(nalu);
                    offset = offset.saturating_add(length);
                }
                true
            }
            28 => {
                let Some((&indicator, rest)) = payload.split_first() else {
                    return false;
                };
                let Some((&fu_header, fragment)) = rest.split_first() else {
                    return false;
                };
                if fragment.is_empty() {
                    return false;
                }
                let nal_header = (indicator & 0xe0) | (fu_header & 0x1f);
                let start = fu_header & 0x80 != 0;
                let end = fu_header & 0x40 != 0;
                if start {
                    if self.fu_header.replace(nal_header).is_some() {
                        return false;
                    }
                    self.append_nalu_header(nal_header);
                } else if self.fu_header != Some(nal_header) {
                    return false;
                }
                self.access_unit.extend_from_slice(fragment);
                if end {
                    self.fu_header = None;
                }
                true
            }
            _ => false,
        }
    }

    fn append_nalu(&mut self, nalu: &[u8]) -> bool {
        let Some(&header) = nalu.first() else {
            return false;
        };
        self.append_nalu_header(header);
        self.access_unit
            .extend_from_slice(nalu.get(1..).unwrap_or_default());
        true
    }

    fn append_nalu_header(&mut self, header: u8) {
        self.access_unit.extend_from_slice(&[0, 0, 0, 1, header]);
        self.frame_is_keyframe |= header & 0x1f == 5;
    }

    fn finish(&mut self, log: &mut VideoReceiveLog, publisher: &str, timestamp: u64) {
        self.open_frame_timestamp = None;
        let complete =
            self.fu_header.is_none() && !self.frame_damaged && !self.access_unit.is_empty();
        if !complete {
            self.pending_gap = true;
            self.record_undecodable_keyframe(log);
        } else {
            match self.decoder.decode(&self.access_unit) {
                Ok(Some(image)) => {
                    let (width, height) = image.dimensions();
                    if width == 0 || height == 0 {
                        log.decoder_errors = log.decoder_errors.saturating_add(1);
                        self.pending_gap = true;
                        self.record_undecodable_keyframe(log);
                    } else {
                        self.jitter.note_frame_delivered();
                        if self.has_rendered && self.pending_gap {
                            log.non_contiguous = log.non_contiguous.saturating_add(1);
                        }
                        self.pending_gap = false;
                        self.has_rendered = true;
                        log.record_decoded(publisher, timestamp, self.frame_is_keyframe);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    log.decoder_errors = log.decoder_errors.saturating_add(1);
                    self.pending_gap = true;
                    self.record_undecodable_keyframe(log);
                }
            }
        }
        self.access_unit.clear();
        self.fu_header = None;
        self.frame_is_keyframe = false;
        self.frame_damaged = false;
    }

    fn record_undecodable_keyframe(&self, log: &mut VideoReceiveLog) {
        if self.frame_is_keyframe {
            log.undecodable_keyframes = log.undecodable_keyframes.saturating_add(1);
        }
    }
}

struct BrowserAudioReceiver {
    decoder: opus::Decoder,
    pcm: Box<[i16]>,
    expected_seq: Option<u64>,
}

impl BrowserAudioReceiver {
    fn new() -> Self {
        Self {
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono)
                .expect("bundled Opus decoder initializes"),
            pcm: vec![0; 5_760].into_boxed_slice(),
            expected_seq: None,
        }
    }

    fn push(
        &mut self,
        rtp: pulsebeam_agent::RtpPacket,
        publisher: &str,
        log: &mut AudioReceiveLog,
    ) {
        let ssrc = rtp.ssrc.map_or(0, |value| *value);
        let sequence = *rtp.seq;
        log.record(publisher, ssrc, sequence, rtp.payload.len(), Instant::now());
        if let Some(expected) = self.expected_seq {
            let missing = sequence.saturating_sub(expected);
            for _ in 0..missing.min(64) {
                self.decode(ssrc, &[], true, log);
            }
            if missing > 64 {
                log.record_decoder_error(ssrc);
            }
        }
        self.expected_seq = Some(sequence.wrapping_add(1));
        self.decode(ssrc, &rtp.payload, false, log);
    }

    fn decode(&mut self, ssrc: u32, packet: &[u8], concealed: bool, log: &mut AudioReceiveLog) {
        match self.decoder.decode(packet, &mut self.pcm, false) {
            Ok(samples) => {
                let Some(pcm) = self.pcm.get(..samples) else {
                    log.record_decoder_error(ssrc);
                    return;
                };
                let energy = pcm.iter().fold(0u64, |total, sample| {
                    total.saturating_add(u64::from(sample.unsigned_abs()))
                });
                log.record_pcm(ssrc, samples, energy, concealed);
            }
            Err(_) => log.record_decoder_error(ssrc),
        }
    }
}

/// What arrived from one speaker, as the listener heard it.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStream {
    pub packets: u64,
    pub bytes: u64,
    /// Longest stretch with no packet, once the stream had started.
    ///
    /// Audio is far less forgiving than video here. A picture that misses 200ms is a stutter
    /// nobody remarks on; a voice that drops 200ms loses a syllable.
    pub longest_gap: Duration,
    /// The most recent rank the SFU gave this speaker, as signalled.
    ///
    /// Packets alone say a speaker got through; the rank says where the SFU placed them. A test
    /// that the loudest voice wins needs both, because "heard" and "heard as the loudest" are
    /// different claims and only the second is the selector's contract.
    ///
    /// The *latest* rank, not the best one ever held. A room does not form instantly, and while
    /// only one person has connected they are trivially rank 0 - so a minimum over the whole run
    /// reports the join order rather than the selector's judgement.
    pub last_rank: Option<u32>,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
}

impl AudioStream {
    /// How long this speaker was on the wire, first packet to last.
    pub fn audible_for(&self) -> Duration {
        match (self.first_at, self.last_at) {
            (Some(first), Some(last)) => last.saturating_duration_since(first),
            _ => Duration::ZERO,
        }
    }

    fn record(&mut self, bytes: usize, now: Instant) {
        self.first_at.get_or_insert(now);
        if let Some(previous) = self.last_at {
            self.longest_gap = self
                .longest_gap
                .max(now.saturating_duration_since(previous));
        }
        self.last_at = Some(now);
        self.packets = self.packets.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }
}

/// What a listener heard, and from whom.
///
/// Keyed by publisher because that is the question the SFU's speaker selection raises: with more
/// speakers in a room than a subscriber has slots, *which* of them got through is the whole claim,
/// and a total byte count cannot answer it.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct AudioReceiveLog {
    pub by_publisher: std::collections::BTreeMap<String, AudioStream>,
    /// Every RTP stream this listener was sent, and whether each stayed whole.
    ///
    /// Keyed by SSRC rather than by speaker on purpose. Several speakers share a slot's stream
    /// over a call, spliced onto one timeline, and that is the design: a browser cannot route by
    /// SSRC, and libwebrtc answers an SSRC it did not see in the SDP by building a whole new
    /// receive stream. What has to hold is that the shared stream is unbroken across the changes.
    pub by_stream: std::collections::BTreeMap<u32, AudioStreamContinuity>,
}

/// One inbound RTP stream, whoever happens to be on it.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamContinuity {
    pub packets: u64,
    pub decoded_samples: u64,
    pub concealed_samples: u64,
    pub decoder_errors: u64,
    pub pcm_energy: u64,
    /// Largest forward jump in sequence number. Any hole is loss to the receiver, whether the
    /// network caused it or the SFU spliced two speakers together badly.
    pub max_seq_gap: u64,
    last_seq: Option<u64>,
}

impl AudioStreamContinuity {
    fn record_pcm(&mut self, samples: usize, energy: u64, concealed: bool) {
        self.decoded_samples = self.decoded_samples.saturating_add(samples as u64);
        self.pcm_energy = self.pcm_energy.saturating_add(energy);
        if concealed {
            self.concealed_samples = self.concealed_samples.saturating_add(samples as u64);
        }
    }
}

impl AudioReceiveLog {
    fn record(&mut self, publisher: &str, ssrc: u32, seq: u64, bytes: usize, now: Instant) {
        self.by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record(bytes, now);

        let stream = self.by_stream.entry(ssrc).or_default();
        stream.packets = stream.packets.saturating_add(1);
        if let Some(previous) = stream.last_seq {
            let gap = seq.saturating_sub(previous).saturating_sub(1);
            stream.max_seq_gap = stream.max_seq_gap.max(gap);
        }
        if stream.last_seq.is_none_or(|previous| seq > previous) {
            stream.last_seq = Some(seq);
        }
    }

    fn record_pcm(&mut self, ssrc: u32, samples: usize, energy: u64, concealed: bool) {
        self.by_stream
            .entry(ssrc)
            .or_default()
            .record_pcm(samples, energy, concealed);
    }

    fn record_decoder_error(&mut self, ssrc: u32) {
        let stream = self.by_stream.entry(ssrc).or_default();
        stream.decoder_errors = stream.decoder_errors.saturating_add(1);
    }

    fn record_rank(&mut self, publisher: &str, rank: u32) {
        let entry = self.by_publisher.entry(publisher.to_owned()).or_default();
        entry.last_rank = Some(rank);
    }

    /// Speakers this listener was told about, whether or not media arrived.
    pub fn ranked(&self) -> std::collections::BTreeMap<String, u32> {
        self.by_publisher
            .iter()
            .filter_map(|(publisher, s)| s.last_rank.map(|rank| (publisher.clone(), rank)))
            .collect()
    }

    /// Speakers this listener heard for a meaningful part of the call.
    ///
    /// Not "sent us a packet once", and not merely "was audible briefly". A room does not form
    /// instantly: every slot is empty when a call starts, so the first voices to arrive are
    /// forwarded whoever they are, and somebody can hold a slot simply because nobody louder has
    /// connected yet. Counting them reports join order, not the selector's judgement.
    ///
    /// So a speaker is heard if they were audible at all *and* for a decent share of however long
    /// the most-heard speaker managed. One the selector genuinely keeps runs for the length of the
    /// call; one that only occupied an empty slot while the room filled does not come close.
    /// Measured at seed 9: 1.0s against 9.8s, evicted the instant the third talker connected.
    pub fn heard_from(&self) -> std::collections::BTreeSet<String> {
        let longest = self
            .by_publisher
            .values()
            .map(AudioStream::audible_for)
            .max()
            .unwrap_or_default();
        let floor = MIN_AUDIBLE.max(longest / SUSTAINED_SHARE_DIVISOR);
        self.by_publisher
            .iter()
            .filter(|(_, stream)| stream.audible_for() >= floor)
            .map(|(publisher, _)| publisher.clone())
            .collect()
    }
}

/// How much of one stream may be missing before a listener would notice.
///
/// Not zero: a subscriber's audio slots are provisioned as their mids finish negotiating, so a
/// speaker the SFU starts forwarding into a slot that does not exist yet loses the packets in
/// between. That is a handful at the very start of a call and a receiver conceals it inaudibly. It
/// is a different thing from the stream itself being torn, which is what the bound exists for.
pub const MAX_CONCEALABLE_GAP: u64 = 2;

/// What share of the most-heard speaker's airtime counts as having been heard too.
///
/// Half. Deliberately blunt: the gap between a speaker the selector keeps and one that held an
/// empty slot while the room formed is an order of magnitude, not a few percent, so the threshold
/// only has to land somewhere in the middle of it.
const SUSTAINED_SHARE_DIVISOR: u32 = 2;

/// How long a voice must be forwarded before a listener can be said to have heard it.
///
/// Matched to `TopNAudioSelector`'s newborn immunity, which is the window in which the selector
/// makes no promise about who holds a slot - and about the shortest stretch in which a listener
/// could recognise a voice at all. Below this, a speaker is a start-up transient.
pub const MIN_AUDIBLE: Duration = Duration::from_millis(300);

/// How long a stream may deliver nothing before a viewer perceives a freeze rather than jitter.
///
/// Below this, a gap is a late packet, a keyframe wait or a layer switch - all normal. Above it,
/// the picture has visibly stopped.
pub const FREEZE_THRESHOLD: Duration = Duration::from_millis(500);

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoReceiveStats {
    pub frames: u64,
    pub keyframes: u64,
    pub undecodable_keyframes: u64,
    pub non_contiguous: u64,
    pub duplicate_ts_frames: u64,
    pub ts_regression_count: u64,
    pub max_ts_regression: u64,
    pub longest_frame_gap: Duration,
    pub first_frame_at: Option<Instant>,
    pub last_frame_at: Option<Instant>,
    pub frozen_time: Duration,
    pub browser_packet_errors: u64,
    pub decoder_errors: u64,
}

impl VideoReceiveStats {
    pub fn since(self, baseline: Self) -> Self {
        Self {
            frames: self.frames.saturating_sub(baseline.frames),
            keyframes: self.keyframes.saturating_sub(baseline.keyframes),
            undecodable_keyframes: self
                .undecodable_keyframes
                .saturating_sub(baseline.undecodable_keyframes),
            non_contiguous: self.non_contiguous.saturating_sub(baseline.non_contiguous),
            duplicate_ts_frames: self
                .duplicate_ts_frames
                .saturating_sub(baseline.duplicate_ts_frames),
            ts_regression_count: self
                .ts_regression_count
                .saturating_sub(baseline.ts_regression_count),
            max_ts_regression: self.max_ts_regression.max(baseline.max_ts_regression),
            longest_frame_gap: self.longest_frame_gap.max(baseline.longest_frame_gap),
            first_frame_at: self.first_frame_at.or(baseline.first_frame_at),
            last_frame_at: self.last_frame_at,
            frozen_time: self.frozen_time.saturating_sub(baseline.frozen_time),
            browser_packet_errors: self
                .browser_packet_errors
                .saturating_sub(baseline.browser_packet_errors),
            decoder_errors: self.decoder_errors.saturating_sub(baseline.decoder_errors),
        }
    }
}

/// Scans an Annex-B frame for the H.264 NAL unit types it contains, using the
/// same `pulsebeam_core::h264::classify()` classifier as the production SFU forwarder.
impl VideoReceiveLog {
    pub fn frames_from(&self, publisher: &str) -> u64 {
        self.by_publisher.get(publisher).copied().unwrap_or(0)
    }

    pub fn stats(&self) -> VideoReceiveStats {
        VideoReceiveStats {
            frames: self.frames,
            keyframes: self.keyframes,
            undecodable_keyframes: self.undecodable_keyframes,
            non_contiguous: self.non_contiguous,
            duplicate_ts_frames: self.duplicate_ts_frames,
            ts_regression_count: self.ts_regression_count,
            max_ts_regression: self.max_ts_regression,
            longest_frame_gap: self.longest_frame_gap,
            first_frame_at: self.first_frame_at,
            last_frame_at: self.last_frame_at,
            frozen_time: self.frozen_time,
            browser_packet_errors: self.browser_packet_errors(),
            decoder_errors: self.decoder_errors,
        }
    }

    fn record_decoded(&mut self, publisher: &str, ts: u64, is_keyframe: bool) {
        let now = Instant::now();
        if let Some(previous) = self.last_frame_at {
            let gap = now.saturating_duration_since(previous);
            self.longest_frame_gap = self.longest_frame_gap.max(gap);
            if gap > FREEZE_THRESHOLD {
                self.frozen_time = self.frozen_time.saturating_add(gap);
            }
        }
        self.first_frame_at.get_or_insert(now);
        self.last_frame_at = Some(now);
        *self.by_publisher.entry(publisher.to_owned()).or_default() += 1;
        self.frames += 1;
        if is_keyframe {
            self.keyframes += 1;
        }
        if !self.seen_ts.insert(ts) {
            self.duplicate_ts_frames += 1;
        }
        if let Some(prev) = self.last_ts
            && ts < prev
        {
            let delta = prev - ts;
            self.max_ts_regression = self.max_ts_regression.max(delta);
            // Only count as a "switch regression" if the jump is small (< 1s at 90kHz).
            // Video loops in test data cause large backwards jumps (~entire clip duration)
            // which are not stream quality issues.
            if delta < 90_000 {
                self.ts_regression_count += 1;
            }
        }
        self.last_ts = Some(ts);
    }

    pub fn browser_packet_errors(&self) -> u64 {
        self.missing_mid_packets
            .saturating_add(self.missing_ssrc_packets)
            .saturating_add(self.missing_payload_type_packets)
            .saturating_add(self.changed_ssrc_packets)
            .saturating_add(self.changed_payload_type_packets)
    }
}

type SubscribedTopics = Arc<Mutex<HashMap<(String, Option<String>), DataSubscriber>>>;

pub struct ClientContext {
    pub ip: IpAddr,
    pub agent: Agent,
    incoming_tracks: tokio::sync::mpsc::Receiver<RemoteTrack>,
    pub(crate) incoming_track_tx: tokio::sync::mpsc::Sender<RemoteTrack>,
    participants: Participants,
    /// Aggregated decode-side view of every remote video track.
    pub video_rx: Arc<Mutex<VideoReceiveLog>>,
    /// What this listener heard, per speaker. Shared with the harness like `video_rx`.
    pub audio_rx: Arc<Mutex<AudioReceiveLog>>,
    local_publications: Vec<LocalTrack>,

    /// Remote track IDs that have been discovered from signaling updates.
    pub discovered_tracks: HashSet<String>,
    /// Remote tracks that have been assigned to a slot and are actively streaming.
    pub remote_tracks: HashMap<String, String>,
    /// Publishers the SFU told this viewer it had stopped forwarding, at any point in the run.
    ///
    /// The distinction the whole pause signal exists for: a stream can stop because the SFU shed
    /// it or because the connection died, and from the media alone those are identical. Recording
    /// the signal lets a plan assert the viewer was *told*, not merely that packets stopped.
    ///
    /// Shared with the harness the same way `video_rx` is, so a plan can read it after the run.
    pub paused_publishers: Arc<Mutex<std::collections::BTreeSet<String>>>,
    pub(crate) requested_tracks: HashSet<String>,
    pub published_topics: Arc<Mutex<HashMap<String, DataPublisher>>>,
    pub subscribed_topics: SubscribedTopics,
    pub ordered_publishers: Arc<Mutex<HashMap<String, OrderedTopicPublisher>>>,
    pub ordered_subscribers: Arc<Mutex<HashMap<String, OrderedTopicSubscriber>>>,
    /// Data channel payloads received by topic.
    #[allow(dead_code)]
    pub received_data: Vec<(String, Vec<u8>)>,
    reject_vp8: bool,
}

pub struct SimClient {
    pub ctx: ClientContext,
    join_set: JoinSet<()>,
}

#[allow(dead_code)]
impl SimClient {
    pub async fn drive(&mut self, token: CancellationToken) -> anyhow::Result<()> {
        self.drive_until_cancelled(token, |_| false).await
    }

    pub async fn drive_for(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let mut driver = Box::pin(self.drive_until_cancelled(token.clone(), |_| false));

        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                token.cancel();
            }
            res = &mut driver => {
                return res;
            }
        }

        driver.await
    }

    pub async fn drive_until<F>(&mut self, timeout: Duration, predicate: F) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        let token = CancellationToken::new();
        let _guard = token.clone().drop_guard();
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let stats = self.ctx.agent.stats().current();
                anyhow::bail!(
                    "Client {} timed out ({:?}). Final Stats:\n{:?}\nDiscovered: {:?}\nRemoteTracks: {:?}",
                    self.ctx.ip,
                    timeout,
                    stats,
                    self.ctx.discovered_tracks,
                    self.ctx.remote_tracks
                );
            }
            result = self.drive_until_cancelled(token, predicate) => result
        }
    }

    pub async fn drive_with<F>(&mut self, predicate: F) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(
            CancellationToken::new(),
            Duration::from_millis(200),
            predicate,
        )
        .await
    }

    pub async fn drive_with_interval<F>(
        &mut self,
        check_interval: Duration,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(
            CancellationToken::new(),
            check_interval,
            predicate,
        )
        .await
    }

    pub async fn drive_until_cancelled<F>(
        &mut self,
        token: CancellationToken,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(token, Duration::from_millis(10), predicate)
            .await
    }

    async fn drive_until_cancelled_with_interval<F>(
        &mut self,
        token: CancellationToken,
        check_every: Duration,
        mut predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        let span = tracing::info_span!("drive_until_cancelled", ip = %self.ctx.ip, participant_id = %self.ctx.agent.participant_id());
        async move {
            let mut check_interval = tokio::time::interval(check_every);
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        return Ok(());
                    }
                    result = self.ctx.participants.next() => {
                        // The change feed errors when this agent is torn down
                        // (e.g. an abrupt exit racing teardown); the drive is done.
                        let Ok(change) = result else {
                            return Ok(());
                        };
                        match change {
                            ParticipantChange::Joined(participant)
                            | ParticipantChange::Updated(participant) => {
                                if participant.video_paused()
                                    && let Ok(mut seen) = self.ctx.paused_publishers.lock()
                                {
                                    seen.insert(participant.id().to_string());
                                }
                                self.ctx
                                    .discovered_tracks
                                    .insert(participant.id().clone());
                            }
                            ParticipantChange::Left(participant_id) => {
                                self.ctx.discovered_tracks.remove(&participant_id);
                            }
                        }
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                    Some(mut track) = self.ctx.incoming_tracks.recv() => {
                        let publication_id = track.publisher_id().to_owned();
                        self.ctx
                            .remote_tracks
                            .insert(publication_id.clone(), publication_id);
                        let publisher_id = track.publisher_id().to_owned();
                        let log = self.ctx.video_rx.clone();
                        let reject_vp8 = self.ctx.reject_vp8;
                        self.join_set.spawn(async move {
                            let mut receiver = BrowserVideoReceiver::new(reject_vp8);
                            while let Ok(rtp) = track.recv().await {
                                let mut log = log.lock().unwrap();
                                receiver.push(rtp, &mut log, &publisher_id);
                            }
                            let mut log = log.lock().unwrap();
                            receiver.flush(&mut log, &publisher_id);
                        });

                        // Re-check the predicate after processing an event, since a new
                        // event may indicate the desired state has been reached.
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                    _ = check_interval.tick() => {
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        .instrument(span)
        .await
    }
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
}

pub fn create_h264_looper_for_rid(rid: Option<&str>) -> H264Looper {
    let data = match rid {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
    };
    H264Looper::new(data, 30)
}

pub fn create_vbr_looper_for_rid(rid: Option<&str>, profile: VbrProfile) -> VbrLooper {
    debug_assert_eq!(rid, Some("f"));
    VbrLooper::new_scheduled(
        pulsebeam_testdata::RAW_H264_SCREEN_FULL_VBR,
        pulsebeam_testdata::RAW_H264_SCREEN_FULL_TIMING,
        profile,
    )
}

#[cfg(test)]
mod decoder_tests {
    use super::*;

    #[test]
    fn bundled_openh264_decodes_the_h264_fixture() {
        let mut decoder = H264Decoder::new().unwrap();
        let image = decoder
            .decode(pulsebeam_testdata::RAW_H264_QUARTER_CBR)
            .unwrap();
        assert!(image.is_some());
    }

    #[test]
    fn bundled_opus_decodes_the_audio_fixture() {
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut pcm = Box::<[i16]>::from([0; 5_760]);
        let samples = decoder
            .decode(pulsebeam_testdata::RAW_OPUS_20MS_MONO, &mut pcm, false)
            .unwrap();
        assert_eq!(samples, 960);
        assert!(pcm[..samples].iter().any(|sample| *sample != 0));
    }
}

pub struct HyperClientWrapper<C>(pub Client<C, Full<Bytes>>);

impl<C> AsyncHttpClient for HyperClientWrapper<C>
where
    // These bounds are required for Hyper to actually send a request
    C: tower::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read
        + hyper::rt::Write
        + hyper_util::client::legacy::connect::Connection
        + Send
        + Unpin,
    C::Future: Send + Unpin,
    C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
        let client = self.0.clone();

        Box::pin(async move {
            // 1. Convert http::Request<Vec<u8>> -> http::Request<Full<Bytes>>
            let (parts, body) = req.into_parts();
            let hyper_req = http::Request::from_parts(parts, Full::new(Bytes::from(body)));

            // 2. Execute via Hyper
            let res = client
                .request(hyper_req)
                .await
                .map_err(|e| Box::new(e) as HttpError)?;

            // 3. Buffer the streaming body back into a Vec<u8>
            let (parts, res_body) = res.into_parts();
            let bytes = res_body
                .collect()
                .await
                .map_err(|e| Box::new(e) as HttpError)?
                .to_bytes();

            Ok(http::Response::from_parts(parts, bytes.to_vec()))
        })
    }
}

mod connector {
    use hyper::Uri;
    use pin_project_lite::pin_project;
    use std::{future::Future, io::Error, pin::Pin};
    use tokio::io::AsyncWrite;
    use tower::Service;
    use turmoil::net::TcpStream;

    type Fut = Pin<Box<dyn Future<Output = Result<TurmoilConnection, Error>> + Send>>;

    pub fn connector()
    -> impl Service<Uri, Response = TurmoilConnection, Error = Error, Future = Fut> + Clone {
        tower::service_fn(|uri: Uri| {
            Box::pin(async move {
                let conn = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
                Ok::<_, Error>(TurmoilConnection { fut: conn })
            }) as Fut
        })
    }

    pin_project! {
        pub struct TurmoilConnection{
            #[pin]
            fut: turmoil::net::TcpStream
        }
    }

    impl hyper::rt::Read for TurmoilConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            // Use a stack buffer for reads to avoid unsafe operations on the
            // underlying `ReadBufCursor`. This avoids UB while allowing compatibility
            // with Hyper's legacy runtime traits.
            let mut temp = [0u8; 8192];
            let mut tbuf = tokio::io::ReadBuf::new(&mut temp);

            match tokio::io::AsyncRead::poll_read(self.project().fut, cx, &mut tbuf) {
                std::task::Poll::Ready(Ok(())) => {
                    let n = tbuf.filled().len();
                    if n > 0 {
                        buf.put_slice(tbuf.filled());
                    }
                    std::task::Poll::Ready(Ok(()))
                }
                other => other,
            }
        }
    }

    impl hyper::rt::Write for TurmoilConnection {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, Error>> {
            Pin::new(&mut self.fut).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_shutdown(cx)
        }
    }

    impl hyper_util::client::legacy::connect::Connection for TurmoilConnection {
        fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
            hyper_util::client::legacy::connect::Connected::new()
        }
    }
}
