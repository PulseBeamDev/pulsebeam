use super::decoder::{DecodeError, H264ReferenceDecoder, OpusReferenceDecoder, ReferenceError};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent::actor::AgentBuilder;
use pulsebeam_agent::agent::{
    DataPublisher, DataSubscriber, OrderedTopicPublisher, OrderedTopicSubscriber,
};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::{H264Looper, VbrLooper, VbrProfile};
use pulsebeam_agent::{
    Agent, LocalTrack, ParticipantChange, Participants, RemoteTrack, SimulcastLayer,
};
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
    quality_video: Option<(
        pulsebeam_testdata::QualityVideoSource,
        pulsebeam_testdata::QualityVideoLayer,
    )>,
    corrupt_video_payload: bool,
    corrupt_audio_payload: bool,
    suppress_natural_keyframe_repeats: bool,
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
            quality_video: None,
            corrupt_video_payload: false,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            quality_references: Arc::new(Mutex::new(BTreeMap::new())),
            h264_publishers: Arc::new(Mutex::new(BTreeSet::new())),
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
            quality_video: None,
            corrupt_video_payload: false,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            quality_references: Arc::new(Mutex::new(BTreeMap::new())),
            h264_publishers: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    pub fn publish_video(mut self, simulcast_layers: Option<Vec<SimulcastLayer>>) -> Self {
        self.agent_builder = self.agent_builder.video_upstream_slots(1, simulcast_layers);
        self.publishes_video = true;
        self
    }

    pub fn publish_quality_video(
        mut self,
        source: pulsebeam_testdata::QualityVideoSource,
        layer: pulsebeam_testdata::QualityVideoLayer,
    ) -> Self {
        self.agent_builder = self.agent_builder.video_upstream_slots(1, None);
        self.publishes_video = true;
        self.quality_video = Some((source, layer));
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
        let (agent, runner) = self.agent_builder.connect_unmanaged(room).await?;
        if self.publishes_video && !self.opaque_payload {
            self.h264_publishers
                .lock()
                .unwrap()
                .insert(agent.participant_id().to_string());
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
                agent.participant_id().clone(),
                QualityVideoReference {
                    source,
                    layer,
                    corpus,
                    decoded,
                    encoded_frames,
                },
            );
        }
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
            video_rx,
            audio_rx,
            quality_references: self.quality_references.clone(),
            h264_publishers: self.h264_publishers.clone(),
            corrupt_video_payload: self.corrupt_video_payload,
            local_publications: local_video.into_iter().collect(),
        };
        if let Some(mut audio_tracks) = audio_tracks {
            let log = ctx.audio_rx.clone();
            let corrupt_payload = self.corrupt_audio_payload;
            join_set.spawn(async move {
                while let Ok(mut track) = audio_tracks.next().await {
                    let publisher = track.publisher_id().to_owned();
                    let log = log.clone();
                    tokio::spawn(async move {
                        let mut decoder = OpusReceiver::new(&publisher);
                        while let Ok(rtp) = track.recv().await {
                            let mut rtp = rtp;
                            if corrupt_payload {
                                let payload = Arc::make_mut(&mut rtp.payload);
                                payload.fill(0xff);
                            }
                            log.lock().unwrap().record(
                                &publisher,
                                rtp.ssrc.map_or(0, |s| *s),
                                *rtp.seq,
                                rtp.payload.len(),
                                Instant::now(),
                            );
                            decoder.push(&rtp, &log, &publisher);
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
                if let Some((source, layer)) = self.quality_video {
                    let looper = create_quality_h264_looper(source, layer);
                    join_set.spawn(looper.run(sender));
                } else {
                    match self.vbr_profile {
                        Some(profile) => {
                            let looper = create_vbr_looper_for_rid(rid, profile);
                            join_set.spawn(looper.run(sender));
                        }
                        None => {
                            let mut looper = create_h264_looper_for_rid(rid);
                            if self.suppress_natural_keyframe_repeats {
                                looper = looper.without_natural_keyframe_repeats();
                            }
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
        }
        if let Some((publication, level)) = local_audio {
            for sender in publication.encodings().iter().cloned() {
                join_set.spawn(
                    QualityAudioLooper {
                        level_dbov: level,
                        phase_offset: self.audio_phase_offset,
                    }
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

    fn record(&mut self, publisher: &str, frame: &pulsebeam_agent::MediaFrame) {
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

type SubscribedTopics = Arc<Mutex<HashMap<(String, Option<String>), DataSubscriber>>>;

struct QualityAudioLooper {
    level_dbov: i8,
    phase_offset: u64,
}

fn create_quality_h264_looper(
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
) -> H264Looper {
    let corpus = pulsebeam_testdata::quality_corpus_video(source, layer);
    debug_assert!(!corpus.is_empty());
    let mut data = Vec::new();
    for index in 0..corpus.len() {
        let Some(frame) = corpus.frame(index) else {
            debug_assert!(false, "quality H.264 corpus cursor escaped its bounds");
            continue;
        };
        debug_assert!(!frame.encoded.is_empty());
        data.extend_from_slice(frame.encoded);
    }
    debug_assert!(!data.is_empty());
    H264Looper::new(&data, pulsebeam_testdata::QUALITY_VIDEO_FPS)
}

impl QualityAudioLooper {
    async fn run(self, sender: pulsebeam_agent::agent::LocalEncoding) {
        let corpus =
            pulsebeam_testdata::quality_corpus_audio(pulsebeam_testdata::QualityAudioSource::Zero);
        debug_assert!(!corpus.is_empty());
        let mut packetizer = pulsebeam_agent::pipeline::FrameSender::without_dependency_descriptor(
            str0m::media::Mid::from("a0"),
            None,
            1,
        );
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
            let frame = pulsebeam_agent::MediaFrame {
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
                abs_capture_time: Some(pulsebeam_agent::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            for packet in packetizer.packetize(&frame) {
                if sender.send(packet).await.is_err() {
                    return;
                }
            }
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
    frame: pulsebeam_agent::MediaFrame,
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
        let frame = pulsebeam_agent::MediaFrame {
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
        packet: &pulsebeam_agent::RtpPacket,
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
    quality_references: Arc<Mutex<BTreeMap<String, QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<BTreeSet<String>>>,
    corrupt_video_payload: bool,
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
                        let quality_references = self.ctx.quality_references.clone();
                        let h264_publishers = self.ctx.h264_publishers.clone();
                        let corrupt_payload = self.ctx.corrupt_video_payload;
                        self.join_set.spawn(async move {
                            // The agent forwards RTP; reassemble frames here (the
                            // "higher layer") before logging QoE.
                            let mut receiver = None;
                            let mut decoder = H264ReferenceDecoder::new(&publisher_id, "video");
                            let mut decoder_ready = false;
                            let mut last_keyframe_request = None;
                            while let Ok(rtp) = track.recv().await {
                                let receiver = receiver.get_or_insert_with(|| {
                                    let h264_packetized = h264_publishers
                                        .lock()
                                        .unwrap()
                                        .contains(&publisher_id);
                                    if h264_packetized {
                                        pulsebeam_agent::FrameReceiver::with_h264()
                                    } else {
                                        pulsebeam_agent::FrameReceiver::new()
                                    }
                                });
                                let frames = receiver.push(rtp);
                                if !receiver.needs_keyframe() {
                                    last_keyframe_request = None;
                                } else if last_keyframe_request.is_none_or(|last| {
                                    tokio::time::Instant::now().duration_since(last)
                                        >= Duration::from_millis(500)
                                }) && track.request_keyframe()
                                {
                                    last_keyframe_request = Some(tokio::time::Instant::now());
                                }
                                for frame in frames {
                                    record_video_frame(
                                        &log,
                                        &quality_references,
                                        &mut decoder,
                                        &mut decoder_ready,
                                        &publisher_id,
                                        frame,
                                        corrupt_payload,
                                    );
                                }
                            }
                            if let Some(mut receiver) = receiver {
                                for frame in receiver.flush() {
                                    record_video_frame(
                                        &log,
                                        &quality_references,
                                        &mut decoder,
                                        &mut decoder_ready,
                                        &publisher_id,
                                        frame,
                                        corrupt_payload,
                                    );
                                }
                            }
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
