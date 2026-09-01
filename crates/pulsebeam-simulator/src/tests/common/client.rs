use super::decoder::{DecodeError, H264ReferenceDecoder, OpusReferenceDecoder, ReferenceError};
use super::media::{VbrProfile, VbrSource, VideoSource};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, AudioSubscription, ConnectionState, DesiredState, MediaKind, MediaTopology,
    PublicationIntent, TopicMessage, TopicMode, TopicNotification, TopicPublisher, TopicSend,
    TopicSubscriber, VideoSubscription,
};
use pulsebeam_agent_native::{Agent, AgentEvent, Config, Host, MediaFrame, SimulcastLayer};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;
// The process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per
// host, so a timestamp taken here cannot be compared with one taken on the coordinator. See
// `sim_clock`, which shims `clock_gettime` for the whole process.
use std::time::Instant;

pub struct SimClientBuilder {
    ip: IpAddr,
    host: Host,
    endpoint: String,
    tcp_server: Option<SocketAddr>,
    video_layers: Option<Vec<SimulcastLayer>>,
    video_slots: usize,
    audio_slots: usize,
    manual_subscriptions: bool,
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
    /// Make the payload opaque (SFrame/E2EE) so the SFU forwards on DD alone.
    opaque_payload: bool,
    quality_video: Option<(
        pulsebeam_testdata::QualityVideoSource,
        pulsebeam_testdata::QualityVideoLayer,
    )>,
    corrupt_video_payload: bool,
    corrupt_audio_payload: bool,
    suppress_natural_keyframe_repeats: bool,
    initial_topics: pulsebeam_agent_native::agent_core::TopicRegistrations,
    quality_references: Arc<Mutex<BTreeMap<String, QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<BTreeSet<String>>>,
}

pub(crate) struct QualityVideoReference {
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
    corpus: pulsebeam_testdata::QualityCorpusVideo,
    decoded: Vec<u8>,
    encoded_frames: Vec<(Vec<u8>, usize)>,
}

fn canonical_annex_b(data: &[u8]) -> Vec<u8> {
    let mut canonical = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        let start_code_len = if data.get(index..index.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
            4
        } else if data.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
            3
        } else {
            canonical.push(data[index]);
            index = index.saturating_add(1);
            continue;
        };
        canonical.extend_from_slice(&[0, 0, 0, 1]);
        index = index.saturating_add(start_code_len);
    }
    canonical
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
        let socket = UdpSocket::bind(unspecified_addr(ip)).await?;
        Ok(Self {
            ip,
            host: Host::new(create_http_client(), socket),
            endpoint: http_base_uri(server_ip, 7070),
            tcp_server: None,
            video_layers: None,
            video_slots: 0,
            audio_slots: 0,
            manual_subscriptions: false,
            video_rx: None,
            audio_rx: None,
            paused_publishers: None,
            publishes_video: false,
            vbr_profile: None,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            opaque_payload: false,
            quality_video: None,
            corrupt_video_payload: false,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            initial_topics: Default::default(),
            quality_references: Arc::new(Mutex::new(BTreeMap::new())),
            h264_publishers: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    /// Like `bind` but also configures a TCP active stream to the server's ICE
    /// port (3478).  Use with `start_sfu_node_tcp_only` to test TCP connectivity.
    pub async fn bind_tcp(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind(unspecified_addr(ip)).await?;
        Ok(Self {
            ip,
            host: Host::new(create_http_client(), socket),
            endpoint: http_base_uri(server_ip, 7070),
            tcp_server: Some(SocketAddr::new(server_ip, 3478)),
            video_layers: None,
            video_slots: 0,
            audio_slots: 0,
            manual_subscriptions: false,
            video_rx: None,
            audio_rx: None,
            paused_publishers: None,
            publishes_video: false,
            vbr_profile: None,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            opaque_payload: false,
            quality_video: None,
            corrupt_video_payload: false,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            initial_topics: Default::default(),
            quality_references: Arc::new(Mutex::new(BTreeMap::new())),
            h264_publishers: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    pub fn publish_video(mut self, simulcast_layers: Option<Vec<SimulcastLayer>>) -> Self {
        self.video_layers = simulcast_layers;
        self.publishes_video = true;
        self
    }

    pub fn publish_quality_video(
        mut self,
        source: pulsebeam_testdata::QualityVideoSource,
        layer: pulsebeam_testdata::QualityVideoLayer,
    ) -> Self {
        self.publishes_video = true;
        self.quality_video = Some((source, layer));
        self
    }

    /// Receive audio, reserving `capacity` downstream slots.
    ///
    /// The SFU forwards only the loudest few speakers, so this is how many it can send at once -
    /// the receiving end of `TopNAudioSelector`'s slots.
    pub fn receive_audio(mut self, capacity: usize) -> Self {
        self.audio_slots = capacity;
        self
    }

    /// Publish audio at the given loudness in negative dBov: around -30 is ordinary speech,
    /// below about -60 reads as a quiet room.
    pub fn publish_audio(mut self, level_dbov: i8, phase_offset: u64) -> Self {
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

    pub fn with_corrupt_video_payload(mut self) -> Self {
        self.corrupt_video_payload = true;
        self
    }

    pub fn with_corrupt_audio_payload(mut self) -> Self {
        self.corrupt_audio_payload = true;
        self
    }

    pub fn suppress_natural_keyframe_repeats(mut self) -> Self {
        self.suppress_natural_keyframe_repeats = true;
        self
    }

    pub(crate) fn with_initial_topics(
        mut self,
        topics: pulsebeam_agent_native::agent_core::TopicRegistrations,
    ) -> Self {
        self.initial_topics = topics;
        self
    }

    pub fn receive_video(mut self, capacity: usize) -> Self {
        self.video_slots = capacity;
        self
    }

    pub fn manual_subscriptions(mut self) -> Self {
        self.manual_subscriptions = true;
        self
    }

    /// Model a marker/deep-inspection-only peer that never negotiates DD.
    pub fn without_dependency_descriptor(mut self) -> Self {
        self.temporal_dd = Some(0);
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

    pub(crate) fn with_quality_references(
        mut self,
        references: Arc<Mutex<BTreeMap<String, QualityVideoReference>>>,
    ) -> Self {
        self.quality_references = references;
        self
    }

    pub(crate) fn with_h264_publishers(mut self, publishers: Arc<Mutex<BTreeSet<String>>>) -> Self {
        self.h264_publishers = publishers;
        self
    }

    pub fn with_video_rx(mut self, rx: Arc<Mutex<VideoReceiveLog>>) -> Self {
        self.video_rx = Some(rx);
        self
    }

    pub(crate) async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let topology = MediaTopology {
            local_video: self
                .publishes_video
                .then(|| "video".to_owned())
                .into_iter()
                .collect(),
            local_audio: self
                .audio_level_dbov
                .map(|_| "audio".to_owned())
                .into_iter()
                .collect(),
            remote_video: u8::try_from(self.video_slots)?,
            remote_audio: u8::try_from(self.audio_slots)?,
        };
        let session = AgentConfig {
            endpoint: self.endpoint,
            room_id: room.to_owned(),
            request_headers: Vec::new(),
            topology,
            manual_subscriptions: self.manual_subscriptions,
            retry: Default::default(),
        };
        let mut config = Config::new(session);
        config.local_ips.push(self.ip);
        config.tcp_server = self.tcp_server;
        config.dependency_descriptor = self.temporal_dd != Some(0);
        if let Some(layers) = self.video_layers.clone() {
            config.video_encodings.insert("video".into(), layers);
        }
        if let Some(layers) = self.temporal_dd.filter(|layers| *layers > 0) {
            config.video_temporal_layers.insert("video".into(), layers);
        }

        let agent = Agent::spawn(config, self.host).await?;
        let desired = DesiredState {
            revision: 1,
            connected: true,
            publications: self
                .publishes_video
                .then(|| PublicationIntent {
                    slot: "video".into(),
                    active: true,
                })
                .into_iter()
                .chain(self.audio_level_dbov.map(|_| PublicationIntent {
                    slot: "audio".into(),
                    active: true,
                }))
                .collect(),
            topics: self.initial_topics,
            ..DesiredState::default()
        };
        agent.replace_desired(desired.clone()).await?;

        let mut snapshots = agent.snapshots();
        while snapshots.borrow().participant_id.is_none()
            || snapshots.borrow().connection != ConnectionState::Connected
        {
            snapshots.changed().await?;
        }
        let participant_id = snapshots
            .borrow()
            .participant_id
            .clone()
            .expect("connected snapshot has an identity");
        if self.publishes_video && !self.opaque_payload {
            self.h264_publishers
                .lock()
                .unwrap()
                .insert(participant_id.clone());
        }
        if let Some((source, layer)) = self.quality_video {
            let corpus = pulsebeam_testdata::quality_corpus_video(source, layer);
            let decoded = corpus
                .decode_reference()
                .unwrap_or_else(|error| panic!("quality H.264 reference failed: {error}"));
            let encoded_frames = (0..corpus.len())
                .filter_map(|index| {
                    corpus
                        .frame(index)
                        .map(|frame| (canonical_annex_b(frame.encoded), frame.index))
                })
                .collect();
            self.quality_references.lock().unwrap().insert(
                participant_id,
                QualityVideoReference {
                    source,
                    layer,
                    corpus,
                    decoded,
                    encoded_frames,
                },
            );
        }

        let video_rx = self
            .video_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(VideoReceiveLog::default())));
        let audio_rx = self
            .audio_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(AudioReceiveLog::default())));
        let mut ctx = ClientContext {
            agent: agent.clone(),
            desired,
            video_capacity: self.video_slots,
            auto_subscribe: !self.manual_subscriptions,
            requested_video: None,
            discovered_tracks: HashSet::new(),
            remote_tracks: HashMap::new(),
            paused_publishers: self
                .paused_publishers
                .unwrap_or_else(|| Arc::new(Mutex::new(BTreeSet::new()))),
            received_data: Vec::new(),
            media_kinds: HashMap::new(),
            video_rx,
            audio_rx,
            quality_references: self.quality_references.clone(),
            h264_publishers: self.h264_publishers.clone(),
            corrupt_video_payload: self.corrupt_video_payload,
            events: agent.events(),
        };
        let mut join_set = JoinSet::new();

        for slot in 0..self.video_slots {
            let remote = agent.remote_video(u8::try_from(slot)?).await?;
            spawn_video_receiver(&mut join_set, remote, &ctx, agent.clone());
        }
        for slot in 0..self.audio_slots {
            let mut remote = agent.remote_audio(u8::try_from(slot)?).await?;
            let log = ctx.audio_rx.clone();
            let observed_agent = agent.clone();
            let corrupt = self.corrupt_audio_payload;
            join_set.spawn(async move {
                let mut decoders = HashMap::<String, OpusReceiver>::new();
                while let Ok(mut packet) = remote.recv_packet().await {
                    let Some(binding) = remote.audio_binding() else {
                        continue;
                    };
                    let snapshot = observed_agent.snapshot();
                    let Some(publisher) = snapshot
                        .publications
                        .get(&binding.track_id)
                        .map(|publication| publication.participant_id.clone())
                    else {
                        continue;
                    };
                    if corrupt {
                        Arc::make_mut(&mut packet.payload).fill(0xff);
                    }
                    log.lock().unwrap().record(
                        &publisher,
                        packet.ssrc.map_or(0, |ssrc| *ssrc),
                        *packet.seq,
                        packet.payload.len(),
                        Instant::now(),
                    );
                    decoders
                        .entry(publisher.clone())
                        .or_insert_with(|| OpusReceiver::new(&publisher))
                        .push(&packet, &log, &publisher);
                }
            });
        }

        if self.publishes_video {
            let media = agent.local_media("video");
            let encodings: Vec<Option<String>> = self
                .video_layers
                .as_ref()
                .map(|layers| {
                    layers
                        .iter()
                        .map(|layer| Some(layer.rid.to_string()))
                        .collect()
                })
                .unwrap_or_else(|| vec![None]);
            for encoding in encodings {
                if let Some((source, layer)) = self.quality_video {
                    join_set.spawn(create_quality_video_source(source, layer).run(
                        media.clone(),
                        encoding,
                        agent.events(),
                    ));
                } else if let Some(profile) = self.vbr_profile {
                    join_set.spawn(create_vbr_source(encoding.as_deref(), profile).run(
                        media.clone(),
                        encoding,
                        agent.events(),
                    ));
                } else {
                    let mut source = create_video_source(encoding.as_deref());
                    if self.suppress_natural_keyframe_repeats {
                        source = source.without_natural_keyframe_repeats();
                    }
                    if self.opaque_payload {
                        source = source.opaque();
                    }
                    join_set.spawn(source.run(media.clone(), encoding, agent.events()));
                }
            }
        }
        if let Some(level) = self.audio_level_dbov {
            join_set.spawn(
                QualityAudioLooper {
                    level_dbov: level,
                    phase_offset: self.audio_phase_offset,
                }
                .run(agent.local_audio("audio")),
            );
        }
        ctx.refresh().await?;
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
    pub decoded_by_publisher: BTreeMap<String, DecodedVideoStream>,
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
    pub decoder_errors: u64,
    pub reference_mismatches: u64,
    /// Keyframes whose complete access unit failed to decode.
    pub decoder_error_keyframes: u64,
    /// Keyframes rejected before decode because the access unit had no SPS/PPS.
    pub missing_parameter_set_keyframes: u64,
    pub bytes: u64,
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

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedVideoStream {
    pub frames: u64,
    pub width: usize,
    pub height: usize,
    pub source: Option<pulsebeam_testdata::QualityVideoSource>,
    pub layer: Option<pulsebeam_testdata::QualityVideoLayer>,
    pub decoder_errors: u64,
    pub reference_mismatches: u64,
    pub reference_error: ReferenceError,
}

/// What arrived from one speaker, as the listener heard it.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStream {
    pub packets: u64,
    pub bytes: u64,
    pub decoded_samples: u64,
    pub decoder_errors: u64,
    pub reference_mismatches: u64,
    pub reference_error: ReferenceError,
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
    decoded_first_at: Option<Instant>,
    decoded_last_at: Option<Instant>,
}

impl DecodedVideoStream {
    fn record(
        &mut self,
        width: usize,
        height: usize,
        reference: Option<ReferenceError>,
        quality_identity: Option<(
            pulsebeam_testdata::QualityVideoSource,
            pulsebeam_testdata::QualityVideoLayer,
        )>,
    ) {
        debug_assert!(width > 0 && height > 0);
        self.frames = self.frames.saturating_add(1);
        self.width = width;
        self.height = height;
        if let Some((source, layer)) = quality_identity {
            self.source = Some(source);
            self.layer = Some(layer);
        }
        if let Some(error) = reference {
            self.reference_error.sum = self.reference_error.sum.saturating_add(error.sum);
            self.reference_error.samples =
                self.reference_error.samples.saturating_add(error.samples);
            self.reference_error.max = self.reference_error.max.max(error.max);
        }
    }
}

impl AudioStream {
    /// How long this speaker was on the wire, first packet to last.
    pub fn audible_for(&self) -> Duration {
        match (self.decoded_first_at, self.decoded_last_at) {
            (Some(first), Some(last)) => last.saturating_duration_since(first),
            _ => Duration::ZERO,
        }
    }

    fn packet_audible_for(&self) -> Duration {
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

    fn record_decoded(&mut self, samples: usize, now: Instant, reference: Option<ReferenceError>) {
        debug_assert!(samples > 0);
        self.decoded_first_at.get_or_insert(now);
        self.decoded_last_at = Some(now);
        self.decoded_samples = self.decoded_samples.saturating_add(samples as u64);
        if let Some(error) = reference {
            self.reference_error.sum = self.reference_error.sum.saturating_add(error.sum);
            self.reference_error.samples =
                self.reference_error.samples.saturating_add(error.samples);
            self.reference_error.max = self.reference_error.max.max(error.max);
        }
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
    /// Largest forward jump in sequence number. Any hole is loss to the receiver, whether the
    /// network caused it or the SFU spliced two speakers together badly.
    pub max_seq_gap: u64,
    last_seq: Option<u64>,
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

    fn record_rank(&mut self, publisher: &str, rank: u32) {
        let entry = self.by_publisher.entry(publisher.to_owned()).or_default();
        entry.last_rank = Some(rank);
    }

    fn record_decoded(
        &mut self,
        publisher: &str,
        samples: usize,
        reference: Option<ReferenceError>,
    ) {
        self.by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record_decoded(samples, Instant::now(), reference);
    }

    fn record_decoder_error(&mut self, publisher: &str) {
        let stream = self.by_publisher.entry(publisher.to_owned()).or_default();
        stream.decoder_errors = stream.decoder_errors.saturating_add(1);
    }

    fn record_reference_mismatch(&mut self, publisher: &str) {
        let stream = self.by_publisher.entry(publisher.to_owned()).or_default();
        stream.reference_mismatches = stream.reference_mismatches.saturating_add(1);
    }

    pub fn decoded_from(&self, publisher: &str) -> Option<AudioStream> {
        self.by_publisher.get(publisher).copied()
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

    pub fn packet_heard_from(&self) -> std::collections::BTreeSet<String> {
        let longest = self
            .by_publisher
            .values()
            .map(AudioStream::packet_audible_for)
            .max()
            .unwrap_or_default();
        let floor = MIN_AUDIBLE.max(longest / SUSTAINED_SHARE_DIVISOR);
        self.by_publisher
            .iter()
            .filter(|(_, stream)| stream.packet_audible_for() >= floor)
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
    pub decoder_errors: u64,
    pub decoder_error_keyframes: u64,
    pub missing_parameter_set_keyframes: u64,
    pub reference_mismatches: u64,
    pub non_contiguous: u64,
    pub duplicate_ts_frames: u64,
    pub ts_regression_count: u64,
    pub max_ts_regression: u64,
    pub longest_frame_gap: Duration,
    pub first_frame_at: Option<Instant>,
    pub last_frame_at: Option<Instant>,
    pub frozen_time: Duration,
}

impl VideoReceiveStats {
    pub fn since(self, baseline: Self) -> Self {
        Self {
            frames: self.frames.saturating_sub(baseline.frames),
            keyframes: self.keyframes.saturating_sub(baseline.keyframes),
            decoder_errors: self.decoder_errors.saturating_sub(baseline.decoder_errors),
            reference_mismatches: self
                .reference_mismatches
                .saturating_sub(baseline.reference_mismatches),
            decoder_error_keyframes: self
                .decoder_error_keyframes
                .saturating_sub(baseline.decoder_error_keyframes),
            missing_parameter_set_keyframes: self
                .missing_parameter_set_keyframes
                .saturating_sub(baseline.missing_parameter_set_keyframes),
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
        }
    }
}

impl VideoReceiveLog {
    pub fn frames_from(&self, publisher: &str) -> u64 {
        self.by_publisher.get(publisher).copied().unwrap_or(0)
    }

    pub fn decoded_from(&self, publisher: &str) -> Option<DecodedVideoStream> {
        self.decoded_by_publisher.get(publisher).copied()
    }

    pub fn stats(&self) -> VideoReceiveStats {
        VideoReceiveStats {
            frames: self.frames,
            keyframes: self.keyframes,
            decoder_errors: self.decoder_errors,
            reference_mismatches: self.reference_mismatches,
            decoder_error_keyframes: self.decoder_error_keyframes,
            missing_parameter_set_keyframes: self.missing_parameter_set_keyframes,
            non_contiguous: self.non_contiguous,
            duplicate_ts_frames: self.duplicate_ts_frames,
            ts_regression_count: self.ts_regression_count,
            max_ts_regression: self.max_ts_regression,
            longest_frame_gap: self.longest_frame_gap,
            first_frame_at: self.first_frame_at,
            last_frame_at: self.last_frame_at,
            frozen_time: self.frozen_time,
        }
    }

    fn record(&mut self, publisher: &str, frame: &MediaFrame) {
        *self.by_publisher.entry(publisher.to_owned()).or_default() += 1;
        self.bytes += frame.data.len() as u64;
        if !frame.contiguous {
            self.non_contiguous += 1;
        }
        let ts = frame.ts.numer();
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

    fn record_decoded(
        &mut self,
        publisher: &str,
        width: usize,
        height: usize,
        reference: Option<ReferenceError>,
        is_keyframe: bool,
        quality_identity: Option<(
            pulsebeam_testdata::QualityVideoSource,
            pulsebeam_testdata::QualityVideoLayer,
        )>,
    ) {
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
        self.frames = self.frames.saturating_add(1);
        if is_keyframe {
            self.keyframes = self.keyframes.saturating_add(1);
        }
        self.decoded_by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record(width, height, reference, quality_identity);
    }

    fn record_decoder_error(&mut self, publisher: &str, is_keyframe: bool) {
        let stream = self
            .decoded_by_publisher
            .entry(publisher.to_owned())
            .or_default();
        self.decoder_errors = self.decoder_errors.saturating_add(1);
        stream.decoder_errors = stream.decoder_errors.saturating_add(1);
        if is_keyframe {
            self.decoder_error_keyframes = self.decoder_error_keyframes.saturating_add(1);
        }
    }

    fn record_reference_mismatch(&mut self, publisher: &str) {
        self.reference_mismatches = self.reference_mismatches.saturating_add(1);
        let stream = self
            .decoded_by_publisher
            .entry(publisher.to_owned())
            .or_default();
        stream.reference_mismatches = stream.reference_mismatches.saturating_add(1);
    }

    fn record_missing_parameter_set(&mut self, publisher: &str) {
        self.missing_parameter_set_keyframes =
            self.missing_parameter_set_keyframes.saturating_add(1);
        self.record_decoder_error(publisher, true);
    }
}

struct QualityAudioLooper {
    level_dbov: i8,
    phase_offset: u64,
}

impl QualityAudioLooper {
    async fn run(self, sender: pulsebeam_agent_native::LocalMedia) {
        let corpus =
            pulsebeam_testdata::quality_corpus_audio(pulsebeam_testdata::QualityAudioSource::Zero);
        debug_assert!(!corpus.is_empty());
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        let mut counter = 0u64;
        loop {
            let capture_time = interval.tick().await;
            let phase = counter.saturating_add(self.phase_offset);
            let payload_index = usize::try_from(phase)
                .unwrap_or(usize::MAX)
                .checked_rem(corpus.len())
                .unwrap_or(0);
            let Some(corpus_frame) = corpus.frame(payload_index) else {
                debug_assert!(false, "quality Opus corpus cursor escaped its bounds");
                return;
            };
            let speaking = self.level_dbov > -60
                && matches!(
                    corpus_frame.region,
                    pulsebeam_testdata::QualityAudioRegion::Active
                );
            let frame = MediaFrame {
                audio_level: Some(if speaking { self.level_dbov } else { -70 }),
                voice_activity: Some(speaking),
                ts: str0m::media::MediaTime::new(
                    phase.saturating_mul(
                        u64::try_from(pulsebeam_testdata::QUALITY_AUDIO_FRAME_SAMPLES)
                            .unwrap_or(u64::MAX),
                    ),
                    str0m::media::Frequency::FORTY_EIGHT_KHZ,
                ),
                data: Arc::from(corpus_frame.opus_packet),
                capture_time,
                abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            let _ = sender.send(frame).await;
            counter = counter.saturating_add(1);
        }
    }
}

struct OpusReceiver {
    decoder: OpusReferenceDecoder,
    corpus: pulsebeam_testdata::QualityCorpusAudio,
    reference: Vec<u8>,
}

fn record_video_frame(
    log: &Arc<Mutex<VideoReceiveLog>>,
    quality_references: &Arc<Mutex<BTreeMap<String, QualityVideoReference>>>,
    decoder: &mut H264ReferenceDecoder,
    decoder_ready: &mut bool,
    publisher: &str,
    frame: MediaFrame,
    corrupt_payload: bool,
) {
    log.lock().unwrap().record(publisher, &frame);
    if !frame.contiguous {
        *decoder_ready = false;
    }
    let is_keyframe = frame.is_keyframe || annex_b_has_nal_type(&frame.data, 5);
    if !is_keyframe && !*decoder_ready {
        return;
    }
    let has_parameter_sets =
        annex_b_has_nal_type(&frame.data, 7) && annex_b_has_nal_type(&frame.data, 8);
    if is_keyframe && !*decoder_ready && !corrupt_payload && !has_parameter_sets {
        log.lock().unwrap().record_missing_parameter_set(publisher);
        return;
    }
    let stream_reset = is_keyframe && has_parameter_sets;
    if stream_reset {
        decoder.reset();
    }
    let quality_reference_required = quality_references.lock().unwrap().contains_key(publisher);
    let reference = quality_references
        .lock()
        .unwrap()
        .get(publisher)
        .and_then(|entry| {
            entry.encoded_frames.iter().find_map(|(encoded, index)| {
                (encoded.as_slice() == frame.data.as_ref()).then(|| {
                    entry
                        .corpus
                        .reference_frame(&entry.decoded, *index)
                        .map(|reference| (reference.to_vec(), entry.source, entry.layer))
                })?
            })
        });
    if quality_reference_required && reference.is_none() {
        log.lock().unwrap().record_reference_mismatch(publisher);
    }
    let mut decode_frame = frame;
    if corrupt_payload {
        decode_frame.data = Arc::from([0, 0, 0, 1, 0x65, 0xff]);
    }
    let decode_result = decoder.try_decode_observation(
        &decode_frame.data,
        reference
            .as_ref()
            .map(|(reference, _, _)| reference.as_slice()),
    );
    let decoded_ok = decode_result.is_ok();
    match decode_result {
        Ok(observation) => log.lock().unwrap().record_decoded(
            publisher,
            observation.width,
            observation.height,
            observation.reference_error,
            is_keyframe,
            reference.map(|(_, source, layer)| (source, layer)),
        ),
        Err(DecodeError::Decoder(_)) | Err(DecodeError::ReferenceMismatch(_)) => {
            log.lock()
                .unwrap()
                .record_decoder_error(publisher, is_keyframe);
        }
    }
    if is_keyframe && decoded_ok {
        *decoder_ready = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyframe_without_parameter_sets_is_recorded_as_undecodable() {
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let references = Arc::new(Mutex::new(BTreeMap::new()));
        let mut decoder = H264ReferenceDecoder::new("source", "video");
        let mut decoder_ready = false;
        let frame = MediaFrame {
            audio_level: None,
            voice_activity: None,
            ts: str0m::media::MediaTime::from_90khz(0),
            data: Arc::from([0, 0, 0, 1, 0x65, 0]),
            capture_time: tokio::time::Instant::now(),
            abs_capture_time: None,
            contiguous: true,
            is_keyframe: true,
            target_bitrate_bps: None,
            resolution: None,
            dependency_descriptor: None,
            temporal_layers: None,
        };

        record_video_frame(
            &log,
            &references,
            &mut decoder,
            &mut decoder_ready,
            "source",
            frame,
            false,
        );

        let log = log.lock().unwrap();
        assert_eq!(log.frames, 0);
        assert_eq!(log.decoder_errors, 1);
        assert_eq!(log.decoder_error_keyframes, 1);
        assert_eq!(log.decoded_from("source").unwrap().decoder_errors, 1);
    }
}

fn annex_b_has_nal_type(data: &[u8], wanted: u8) -> bool {
    let mut index = 0usize;
    while index.saturating_add(3) < data.len() {
        let header = if data.get(index..index.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
            index.saturating_add(4)
        } else if data.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
            index.saturating_add(3)
        } else {
            index = index.saturating_add(1);
            continue;
        };
        if data.get(header).is_some_and(|byte| byte & 0x1f == wanted) {
            return true;
        }
        index = header;
    }
    false
}

impl OpusReceiver {
    fn new(publisher: &str) -> Self {
        let corpus =
            pulsebeam_testdata::quality_corpus_audio(pulsebeam_testdata::QualityAudioSource::Zero);
        let reference = corpus
            .decode_reference()
            .unwrap_or_else(|error| panic!("quality Opus reference failed: {error}"));
        Self {
            decoder: OpusReferenceDecoder::new(publisher, "audio-mono"),
            corpus,
            reference,
        }
    }

    fn push(
        &mut self,
        packet: &pulsebeam_agent_native::RtpPacket,
        log: &Arc<Mutex<AudioReceiveLog>>,
        publisher: &str,
    ) {
        let reference = self
            .corpus
            .frame_for_rtp_timestamp(packet.ts.numer())
            .and_then(|frame| self.corpus.reference_frame(&self.reference, frame.index));
        match self
            .decoder
            .try_decode_observation(&packet.payload, reference)
        {
            Ok(observation) => log.lock().unwrap().record_decoded(
                publisher,
                observation.samples,
                observation.reference_error,
            ),
            Err(DecodeError::Decoder(_)) => log.lock().unwrap().record_decoder_error(publisher),
            Err(DecodeError::ReferenceMismatch(_)) => {
                log.lock().unwrap().record_reference_mismatch(publisher);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestedVideo {
    pub participant_id: String,
    pub height: u32,
    pub min_height: u32,
    pub priority: u32,
}

pub struct ClientContext {
    pub agent: Agent,
    desired: DesiredState,
    video_capacity: usize,
    auto_subscribe: bool,
    requested_video: Option<Vec<RequestedVideo>>,
    pub video_rx: Arc<Mutex<VideoReceiveLog>>,
    pub audio_rx: Arc<Mutex<AudioReceiveLog>>,
    quality_references: Arc<Mutex<BTreeMap<String, QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<BTreeSet<String>>>,
    corrupt_video_payload: bool,
    pub discovered_tracks: HashSet<String>,
    pub remote_tracks: HashMap<String, String>,
    pub paused_publishers: Arc<Mutex<BTreeSet<String>>>,
    pub received_data: Vec<(String, Vec<u8>)>,
    pub media_kinds: HashMap<String, (bool, bool)>,
    events: tokio::sync::broadcast::Receiver<AgentEvent>,
}

impl ClientContext {
    pub fn participant_id(&self) -> Option<String> {
        self.agent.snapshot().participant_id
    }

    pub fn statistics(&self) -> pulsebeam_agent_native::TransportStatistics {
        self.agent.statistics().borrow().clone()
    }

    pub fn connected(&self) -> bool {
        self.agent.snapshot().connection == ConnectionState::Connected
    }

    pub async fn refresh(&mut self) -> anyhow::Result<()> {
        while let Ok(event) = self.events.try_recv() {
            match event {
                AgentEvent::Core(pulsebeam_agent_native::agent_core::Notification::Topic(
                    TopicNotification::Message(message),
                )) => match message {
                    TopicMessage::Latest { topic, payload, .. }
                    | TopicMessage::Ordered { topic, payload, .. } => {
                        self.received_data.push((topic, payload));
                    }
                },
                AgentEvent::RuntimeFailed(message) => anyhow::bail!(message),
                _ => {}
            }
        }

        let snapshot = self.agent.snapshot();
        self.discovered_tracks.clear();
        self.media_kinds.clear();
        for publication in snapshot.publications.values() {
            if snapshot.participant_id.as_deref() == Some(&publication.participant_id) {
                continue;
            }
            self.discovered_tracks
                .insert(publication.participant_id.clone());
            let kinds = self
                .media_kinds
                .entry(publication.participant_id.clone())
                .or_insert((false, false));
            match publication.kind {
                MediaKind::Video => kinds.0 = true,
                MediaKind::Audio => kinds.1 = true,
            }
        }
        self.remote_tracks = snapshot
            .video
            .values()
            .filter_map(|binding| {
                snapshot
                    .publications
                    .get(&binding.track_id)
                    .map(|publication| {
                        if binding.paused {
                            self.paused_publishers
                                .lock()
                                .unwrap()
                                .insert(publication.participant_id.clone());
                        }
                        (publication.participant_id.clone(), binding.track_id.clone())
                    })
            })
            .collect();
        for (rank, binding) in snapshot.audio.iter().enumerate() {
            if let Some(publication) = snapshot.publications.get(&binding.track_id) {
                self.audio_rx.lock().unwrap().record_rank(
                    &publication.participant_id,
                    u32::try_from(rank).unwrap_or(u32::MAX),
                );
            }
        }
        self.sync_video_desired(&snapshot).await.map(|_| ())
    }

    pub async fn set_video_subscriptions(
        &mut self,
        subscriptions: Vec<RequestedVideo>,
    ) -> anyhow::Result<bool> {
        self.requested_video = Some(subscriptions);
        let snapshot = self.agent.snapshot();
        self.sync_video_desired(&snapshot).await
    }

    async fn sync_video_desired(
        &mut self,
        snapshot: &pulsebeam_agent_native::agent_core::Snapshot,
    ) -> anyhow::Result<bool> {
        let mut requests = self.requested_video.clone().unwrap_or_default();
        if self.auto_subscribe {
            let requested = requests
                .iter()
                .map(|request| request.participant_id.as_str())
                .collect::<HashSet<_>>();
            let mut automatic = self
                .discovered_tracks
                .iter()
                .filter(|participant| !requested.contains(participant.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            automatic.sort();
            requests.extend(automatic.into_iter().map(|participant_id| RequestedVideo {
                participant_id,
                height: 720,
                min_height: 0,
                priority: 0,
            }));
        }
        let mut resolved = Vec::with_capacity(requests.len());
        let mut all_resolved = true;
        for request in requests {
            if resolved.len() == self.video_capacity {
                break;
            }
            let track_id = snapshot
                .publications
                .values()
                .find(|publication| {
                    publication.participant_id == request.participant_id
                        && publication.kind == MediaKind::Video
                })
                .map(|publication| publication.id.clone());
            let Some(track_id) = track_id else {
                all_resolved = false;
                continue;
            };
            resolved.push(VideoSubscription {
                slot: u8::try_from(resolved.len())?,
                track_id,
                height: request.height,
                min_height: request.min_height,
                min_fps: 0,
                priority: request.priority,
            });
        }
        if self.desired.video != resolved {
            let mut desired = self.desired.clone();
            desired.video = resolved;
            self.replace_desired(desired).await?;
        }
        Ok(all_resolved)
    }

    pub async fn set_audio_intent(
        &mut self,
        participants: &[String],
        automatic: bool,
    ) -> anyhow::Result<bool> {
        let snapshot = self.agent.snapshot();
        let mut pinned = Vec::new();
        for participant in participants {
            let Some(publication) = snapshot.publications.values().find(|publication| {
                publication.participant_id == *participant && publication.kind == MediaKind::Audio
            }) else {
                return Ok(false);
            };
            pinned.push(publication.id.clone());
        }
        let audio = AudioSubscription { pinned, automatic };
        if self.desired.audio != audio {
            let mut desired = self.desired.clone();
            desired.audio = audio;
            self.replace_desired(desired).await?;
        }
        Ok(true)
    }

    pub async fn register_publisher(
        &mut self,
        topic: String,
        mode: TopicMode,
    ) -> anyhow::Result<bool> {
        let registration = TopicPublisher { topic, mode };
        if !self.desired.topics.publishers.contains(&registration) {
            let mut desired = self.desired.clone();
            desired.topics.publishers.push(registration.clone());
            self.replace_desired(desired).await?;
        }
        Ok(self
            .agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .any(|status| status.registration == registration && status.channel.is_some()))
    }

    pub async fn register_subscriber(
        &mut self,
        topic: String,
        mode: TopicMode,
        publisher_id: Option<String>,
    ) -> anyhow::Result<bool> {
        let registration = TopicSubscriber {
            topic,
            mode,
            publisher_id,
        };
        if !self.desired.topics.subscribers.contains(&registration) {
            let mut desired = self.desired.clone();
            desired.topics.subscribers.push(registration.clone());
            self.replace_desired(desired).await?;
        }
        Ok(self
            .agent
            .snapshot()
            .topics
            .subscribers
            .iter()
            .any(|status| status.registration == registration && status.channel.is_some()))
    }

    pub async fn send_topic(
        &self,
        topic: &str,
        mode: TopicMode,
        payload: Vec<u8>,
    ) -> anyhow::Result<bool> {
        let publisher = TopicPublisher {
            topic: topic.to_owned(),
            mode,
        };
        let ready = self
            .agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .any(|status| status.registration == publisher && status.channel.is_some());
        if !ready {
            return Ok(false);
        }
        self.agent
            .send_topic(TopicSend { publisher, payload })
            .await?;
        Ok(true)
    }

    async fn replace_desired(&mut self, mut desired: DesiredState) -> anyhow::Result<()> {
        desired.revision = self.desired.revision.saturating_add(1);
        self.agent.replace_desired(desired.clone()).await?;
        self.desired = desired;
        Ok(())
    }
}

pub struct SimClient {
    pub ctx: ClientContext,
    join_set: JoinSet<()>,
}

impl SimClient {
    pub async fn tick(&mut self) -> anyhow::Result<()> {
        self.ctx.refresh().await
    }

    pub async fn close(mut self) -> anyhow::Result<()> {
        self.join_set.abort_all();
        self.ctx.agent.close().await?;
        Ok(())
    }

    pub async fn abort(mut self) -> anyhow::Result<()> {
        self.join_set.abort_all();
        self.ctx.agent.abort().await?;
        Ok(())
    }
}

fn spawn_video_receiver(
    join_set: &mut JoinSet<()>,
    mut remote: pulsebeam_agent_native::RemoteMedia,
    ctx: &ClientContext,
    agent: Agent,
) {
    let log = ctx.video_rx.clone();
    let references = ctx.quality_references.clone();
    let h264_publishers = ctx.h264_publishers.clone();
    let corrupt = ctx.corrupt_video_payload;
    join_set.spawn(async move {
        let mut streams = HashMap::<String, VideoStreamReceiver>::new();
        while let Ok(packet) = remote.recv_packet().await {
            let Some(binding) = remote.video_binding() else {
                continue;
            };
            let snapshot = agent.snapshot();
            let Some(publisher) = snapshot
                .publications
                .get(&binding.track_id)
                .map(|publication| publication.participant_id.clone())
            else {
                continue;
            };
            let stream = streams
                .entry(publisher.clone())
                .or_insert_with(|| VideoStreamReceiver {
                    frames: if h264_publishers.lock().unwrap().contains(&publisher) {
                        pulsebeam_agent_native::FrameReceiver::with_h264()
                    } else {
                        pulsebeam_agent_native::FrameReceiver::new()
                    },
                    decoder: H264ReferenceDecoder::new(&publisher, "video"),
                    decoder_ready: false,
                    last_keyframe_request: None,
                });
            let ssrc = packet.ssrc;
            let frames = stream.frames.push(packet);
            let now = tokio::time::Instant::now();
            let should_request_keyframe = stream.frames.needs_keyframe()
                && stream
                    .last_keyframe_request
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500));
            if should_request_keyframe
                && let Some(ssrc) = ssrc
                && remote.request_keyframe(ssrc).await.is_ok()
            {
                stream.last_keyframe_request = Some(now);
            } else if !stream.frames.needs_keyframe() {
                stream.last_keyframe_request = None;
            }
            for frame in frames {
                record_video_frame(
                    &log,
                    &references,
                    &mut stream.decoder,
                    &mut stream.decoder_ready,
                    &publisher,
                    frame,
                    corrupt,
                );
            }
        }
    });
}

struct VideoStreamReceiver {
    frames: pulsebeam_agent_native::FrameReceiver,
    decoder: H264ReferenceDecoder,
    decoder_ready: bool,
    last_keyframe_request: Option<tokio::time::Instant>,
}

fn create_video_source(encoding: Option<&str>) -> VideoSource {
    let data = match encoding {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
    };
    VideoSource::new(data, 30)
}

fn create_quality_video_source(
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
) -> VideoSource {
    let corpus = pulsebeam_testdata::quality_corpus_video(source, layer);
    let mut data = Vec::new();
    for index in 0..corpus.len() {
        if let Some(frame) = corpus.frame(index) {
            data.extend_from_slice(frame.encoded);
        }
    }
    VideoSource::new(&data, pulsebeam_testdata::QUALITY_VIDEO_FPS)
}

fn create_vbr_source(encoding: Option<&str>, profile: VbrProfile) -> VbrSource {
    debug_assert_eq!(encoding, Some("f"));
    VbrSource::scheduled(
        pulsebeam_testdata::RAW_H264_SCREEN_FULL_VBR,
        pulsebeam_testdata::RAW_H264_SCREEN_FULL_TIMING,
        profile,
    )
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
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
