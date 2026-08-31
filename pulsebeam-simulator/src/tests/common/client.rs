use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use openh264::decoder::Decoder as H264Decoder;
use openh264::formats::YUVSource;
use pulsebeam::entity::ParticipantId;
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
use pulsebeam_testdata::{
    QUALITY_AUDIO_FRAME_SAMPLES, QUALITY_VIDEO_FRAME_COUNT, QualityAudioSource, QualityVideoLayer,
    QualityVideoSource, quality_audio_fixture, quality_corpus_video, quality_video_frame,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::task::JoinSet;
// The process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per
// host, so a timestamp taken here cannot be compared with one taken on the coordinator. See
// `sim_clock`, which shims `clock_gettime` for the whole process.
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub const MAX_FIXTURE_PCM_MEAN_ABSOLUTE_ERROR: u64 = 5_500;
const BROWSER_VIDEO_MAX_GAP_WAIT: Duration = Duration::from_millis(500);

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
    quality_source: Option<QualityVideoSource>,
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
            quality_source: None,
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
            quality_source: None,
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

    pub fn with_quality_fixture(mut self, source: QualityVideoSource) -> Self {
        self.quality_source = Some(source);
        self.agent_builder = self.agent_builder.with_initial_send_bitrate_bps(5_000_000);
        self
    }

    pub fn with_initial_send_bitrate_bps(mut self, bitrate_bps: u64) -> Self {
        self.agent_builder = self
            .agent_builder
            .with_initial_send_bitrate_bps(bitrate_bps);
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
        let video_receivers = Arc::new(Mutex::new(BTreeMap::new()));
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
            video_receivers,
            audio_rx,
            local_publications: local_video.into_iter().collect(),
        };
        if let Some(mut audio_tracks) = audio_tracks {
            let subscriber_id: ParticipantId = ctx
                .agent
                .participant_id()
                .parse()
                .expect("agent participant id");
            let log = ctx.audio_rx.clone();
            join_set.spawn(async move {
                while let Ok(mut track) = audio_tracks.next().await {
                    let publisher = track.publisher_id().to_owned();
                    let log = log.clone();
                    tokio::spawn(async move {
                        let mut receiver = BrowserAudioReceiver::for_subscriber(subscriber_id);
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
                        let mut looper = if let Some(source) = self.quality_source {
                            let layer = match rid {
                                Some("q") => QualityVideoLayer::P180,
                                Some("h") => QualityVideoLayer::P360,
                                Some("f") => QualityVideoLayer::P720,
                                _ => {
                                    debug_assert!(false, "quality corpus uses q/h/f simulcast");
                                    QualityVideoLayer::P180
                                }
                            };
                            H264Looper::new(
                                quality_corpus_video(source, layer).encoded(),
                                pulsebeam_testdata::QUALITY_VIDEO_FPS,
                            )
                        } else {
                            create_h264_looper_for_rid(rid)
                        };
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
                let looper = if let Some(source) = self.quality_source {
                    AudioLooper::corpus(match source {
                        QualityVideoSource::Zero => QualityAudioSource::Zero,
                        QualityVideoSource::One => QualityAudioSource::One,
                    })
                    .with_level_dbov(level)
                } else {
                    AudioLooper::speaking()
                        .with_level_dbov(level)
                        .with_phase_offset(self.audio_phase_offset)
                };
                join_set.spawn(looper.run(sender));
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
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedVideoQuality {
    pub frames: u64,
    pub reference_frames: u64,
    pub reference_mismatches: u64,
    pub decoded_width: usize,
    pub decoded_height: usize,
    pub visual_error_sum: u64,
    pub visual_samples: u64,
    pub visual_max_error: u8,
    pub longest_frame_gap: Duration,
    first_frame_at: Option<Instant>,
    last_frame_at: Option<Instant>,
}

impl DecodedVideoQuality {
    pub fn mean_absolute_error(self) -> Option<u64> {
        self.visual_error_sum.checked_div(self.visual_samples)
    }

    fn record_frame(&mut self, width: usize, height: usize, error: Option<PlaneError>) {
        let now = Instant::now();
        if let Some(last_frame_at) = self.last_frame_at {
            self.longest_frame_gap = self
                .longest_frame_gap
                .max(now.saturating_duration_since(last_frame_at));
        }
        self.first_frame_at.get_or_insert(now);
        self.last_frame_at = Some(now);
        self.frames = self.frames.saturating_add(1);
        self.decoded_width = width;
        self.decoded_height = height;
        match error {
            Some(error) => {
                self.reference_frames = self.reference_frames.saturating_add(1);
                self.visual_error_sum = self.visual_error_sum.saturating_add(error.sum);
                self.visual_samples = self.visual_samples.saturating_add(error.samples);
                self.visual_max_error = self.visual_max_error.max(error.max);
            }
            None => self.reference_mismatches = self.reference_mismatches.saturating_add(1),
        }
    }
}

#[derive(Clone, Copy)]
struct PlaneError {
    sum: u64,
    samples: u64,
    max: u8,
}

fn plane_error(
    reference: &[u8],
    decoded: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<PlaneError> {
    debug_assert!(width <= stride);
    let expected_len = width.checked_mul(height)?;
    if reference.len() != expected_len {
        return None;
    }
    let mut sum = 0u64;
    let mut max = 0u8;
    for row in 0..height {
        let decoded_start = row.checked_mul(stride)?;
        let decoded_end = decoded_start.checked_add(width)?;
        let reference_start = row.checked_mul(width)?;
        let reference_end = reference_start.checked_add(width)?;
        let decoded_row = decoded.get(decoded_start..decoded_end)?;
        let reference_row = reference.get(reference_start..reference_end)?;
        for (&expected, &actual) in reference_row.iter().zip(decoded_row) {
            let error = expected.abs_diff(actual);
            sum = sum.saturating_add(u64::from(error));
            max = max.max(error);
        }
    }
    Some(PlaneError {
        sum,
        samples: u64::try_from(expected_len).ok()?,
        max,
    })
}

fn sampled_luma_error(
    reference: &[u8],
    decoded: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<u64> {
    let expected_len = width.checked_mul(height)?;
    if reference.len() != expected_len || width > stride {
        return None;
    }
    let mut sum = 0u64;
    for row in (0..height).step_by(8) {
        let decoded_row = row.checked_mul(stride)?;
        let reference_row = row.checked_mul(width)?;
        for column in (0..width).step_by(8) {
            let expected = *reference.get(reference_row.checked_add(column)?)?;
            let actual = *decoded.get(decoded_row.checked_add(column)?)?;
            sum = sum.saturating_add(u64::from(expected.abs_diff(actual)));
        }
    }
    Some(sum)
}

fn decoded_video_error(image: &impl YUVSource) -> Option<PlaneError> {
    let (width, height) = image.dimensions();
    let (y_stride, _, _) = image.strides();
    let y_len = width.checked_mul(height)?;
    let reference = (0..QUALITY_VIDEO_FRAME_COUNT)
        .filter_map(quality_video_frame)
        .filter(|reference| reference.width == width && reference.height == height)
        .filter_map(|reference| {
            let y_reference = reference.reference_yuv420p.get(..y_len)?;
            let error = sampled_luma_error(y_reference, image.y(), y_stride, width, height)?;
            Some((error, reference))
        })
        .min_by_key(|(error, _)| *error)?
        .1;
    decoded_video_reference_error(image, reference.reference_yuv420p)
}

fn decoded_video_reference_error(image: &impl YUVSource, reference: &[u8]) -> Option<PlaneError> {
    let (width, height) = image.dimensions();
    let (y_stride, u_stride, v_stride) = image.strides();
    let y_len = width.checked_mul(height)?;
    let chroma_len = y_len.checked_div(4)?;
    let y_reference = reference.get(..y_len)?;
    let u_start = y_len;
    let u_end = u_start.checked_add(chroma_len)?;
    let u_reference = reference.get(u_start..u_end)?;
    let v_end = u_end.checked_add(chroma_len)?;
    let v_reference = reference.get(u_end..v_end)?;
    debug_assert_eq!(v_end, reference.len());
    let y_error = plane_error(y_reference, image.y(), y_stride, width, height)?;
    let chroma_width = width.checked_div(2)?;
    let chroma_height = height.checked_div(2)?;
    let u_error = plane_error(
        u_reference,
        image.u(),
        u_stride,
        chroma_width,
        chroma_height,
    )?;
    let v_error = plane_error(
        v_reference,
        image.v(),
        v_stride,
        chroma_width,
        chroma_height,
    )?;
    Some(PlaneError {
        sum: y_error
            .sum
            .saturating_add(u_error.sum)
            .saturating_add(v_error.sum),
        samples: y_error
            .samples
            .saturating_add(u_error.samples)
            .saturating_add(v_error.samples),
        max: y_error.max.max(u_error.max).max(v_error.max),
    })
}

#[derive(Default, Debug, Clone)]
pub struct VideoReceiveLog {
    pub by_publisher: BTreeMap<String, u64>,
    pub quality_by_publisher: BTreeMap<String, DecodedVideoQuality>,
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
    pub damaged_frames: u64,
    pub missing_mid_packets: u64,
    pub missing_ssrc_packets: u64,
    pub missing_payload_type_packets: u64,
    pub changed_ssrc_packets: u64,
    pub changed_payload_type_packets: u64,
    pub crossed_frame_boundaries: u64,
    pub unexpected_frames: u64,
    pub wrong_origin_frames: u64,
    pub wrong_layer_frames: u64,
    pub wrong_content_frames: u64,
    /// When the very first frame reached the decoder. Time-to-first-frame is measured from this
    /// against the moment the viewer subscribed, which only the harness knows.
    pub first_frame_at: Option<Instant>,
    pub first_frame_since_measurement: Option<Instant>,
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
    pub capture_timed_frames: u64,
    pub max_capture_to_decode_latency: Duration,
    last_frame_at: Option<Instant>,
    interval_started_at: Option<Instant>,
    interval_longest_frame_gap: Duration,
    interval_min_decoded_width: usize,
    interval_min_decoded_height: usize,
    interval_max_decoded_width: usize,
    interval_max_decoded_height: usize,
    last_ts: Option<u64>,
    seen_ts: HashSet<u64>,
}

struct BrowserVideoReceiver {
    subscriber: Option<ParticipantId>,
    decoder: H264Decoder,
    jitter: pulsebeam_agent::JitterBuffer,
    expected_ssrc: Option<u32>,
    expected_payload_type: Option<u8>,
    open_frame_timestamp: Option<u64>,
    frame_capture_time: Option<SystemTime>,
    expected_seq: Option<u64>,
    access_unit: Vec<u8>,
    fu_header: Option<u8>,
    frame_is_keyframe: bool,
    frame_damaged: bool,
    reference_gap: bool,
    pending_gap: bool,
    has_rendered: bool,
    awaiting_keyframe: bool,
    references: BTreeMap<(QualityVideoSource, QualityVideoLayer), Vec<u8>>,
}

impl BrowserVideoReceiver {
    fn new() -> Self {
        Self {
            subscriber: None,
            decoder: H264Decoder::new().expect("bundled OpenH264 decoder initializes"),
            jitter: pulsebeam_agent::JitterBuffer::new(BROWSER_VIDEO_MAX_GAP_WAIT),
            expected_ssrc: None,
            expected_payload_type: None,
            open_frame_timestamp: None,
            frame_capture_time: None,
            expected_seq: None,
            access_unit: Vec::with_capacity(16 * 1024),
            fu_header: None,
            frame_is_keyframe: false,
            frame_damaged: false,
            reference_gap: false,
            pending_gap: false,
            has_rendered: false,
            awaiting_keyframe: false,
            references: BTreeMap::new(),
        }
    }

    fn for_subscriber(subscriber: ParticipantId) -> Self {
        Self {
            subscriber: Some(subscriber),
            ..Self::new()
        }
    }

    fn needs_keyframe(&self) -> bool {
        self.awaiting_keyframe
    }

    fn push(
        &mut self,
        rtp: pulsebeam_agent::RtpPacket,
        log: &mut VideoReceiveLog,
        publisher: &str,
    ) -> bool {
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
        self.jitter.push(rtp);
        let mut request_keyframe = false;
        while let Some(rtp) = self.jitter.pop() {
            request_keyframe |= self.process(rtp, log, publisher);
        }
        request_keyframe
    }

    fn process(
        &mut self,
        rtp: pulsebeam_agent::RtpPacket,
        log: &mut VideoReceiveLog,
        publisher: &str,
    ) -> bool {
        let sequence = *rtp.seq;
        if self
            .expected_seq
            .is_some_and(|expected| expected != sequence)
        {
            if self.access_unit.is_empty() {
                self.reference_gap = true;
            } else {
                self.frame_damaged = true;
            }
        }
        self.expected_seq = Some(sequence.wrapping_add(1));

        if rtp.payload.is_empty() {
            return false;
        }

        let timestamp = rtp.ts.numer();
        let mut request_keyframe = false;
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
            request_keyframe |= self.finish(log, publisher, timestamp);
        }
        if self.open_frame_timestamp.is_none() {
            self.frame_capture_time = rtp
                .ext_vals
                .abs_capture_time
                .map(|value| value.capture_time);
        }
        self.open_frame_timestamp = Some(timestamp);

        if !self.append_rtp_payload(&rtp.payload) {
            self.frame_damaged = true;
        }
        if rtp.marker {
            request_keyframe |= self.finish(log, publisher, timestamp);
        }
        request_keyframe
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

    fn finish(&mut self, log: &mut VideoReceiveLog, publisher: &str, timestamp: u64) -> bool {
        self.open_frame_timestamp = None;
        let complete =
            self.fu_header.is_none() && !self.frame_damaged && !self.access_unit.is_empty();
        let mut request_keyframe = false;
        if !complete {
            log.damaged_frames = log.damaged_frames.saturating_add(1);
            self.pending_gap = true;
            self.record_undecodable_keyframe(log);
            request_keyframe = !self.awaiting_keyframe;
            self.awaiting_keyframe = true;
        } else if (self.awaiting_keyframe || self.reference_gap) && !self.frame_is_keyframe {
            self.pending_gap = true;
            request_keyframe = !self.awaiting_keyframe;
            self.awaiting_keyframe = true;
        } else {
            match self.decoder.decode(&self.access_unit) {
                Ok(Some(image)) => {
                    let (width, height) = image.dimensions();
                    if width == 0 || height == 0 {
                        log.decoder_errors = log.decoder_errors.saturating_add(1);
                        self.pending_gap = true;
                        self.record_undecodable_keyframe(log);
                        request_keyframe = !self.awaiting_keyframe;
                        self.awaiting_keyframe = true;
                    } else {
                        self.jitter.note_frame_delivered();
                        if self.frame_is_keyframe {
                            self.awaiting_keyframe = false;
                        }
                        if self.has_rendered && self.pending_gap {
                            log.non_contiguous = log.non_contiguous.saturating_add(1);
                        }
                        self.pending_gap = false;
                        self.has_rendered = true;
                        let reference_error = expected_video_error(
                            self.subscriber,
                            &mut self.references,
                            publisher,
                            timestamp,
                            &image,
                            log,
                        );
                        log.record_decoded(
                            publisher,
                            timestamp,
                            self.frame_is_keyframe,
                            self.frame_capture_time,
                            &image,
                            reference_error,
                        );
                    }
                }
                Ok(None) => {
                    log.decoder_errors = log.decoder_errors.saturating_add(1);
                    self.pending_gap = true;
                    self.record_undecodable_keyframe(log);
                    request_keyframe = !self.awaiting_keyframe;
                    self.awaiting_keyframe = true;
                }
                Err(_) => {
                    log.decoder_errors = log.decoder_errors.saturating_add(1);
                    self.pending_gap = true;
                    self.record_undecodable_keyframe(log);
                    request_keyframe = !self.awaiting_keyframe;
                    self.awaiting_keyframe = true;
                }
            }
        }
        self.access_unit.clear();
        self.fu_header = None;
        self.frame_is_keyframe = false;
        self.frame_damaged = false;
        self.reference_gap = false;
        self.frame_capture_time = None;
        request_keyframe
    }

    fn record_undecodable_keyframe(&self, log: &mut VideoReceiveLog) {
        if self.frame_is_keyframe {
            log.undecodable_keyframes = log.undecodable_keyframes.saturating_add(1);
        }
    }
}

fn expected_video_error(
    subscriber: Option<ParticipantId>,
    references: &mut BTreeMap<(QualityVideoSource, QualityVideoLayer), Vec<u8>>,
    publisher: &str,
    timestamp: u64,
    image: &impl YUVSource,
    log: &mut VideoReceiveLog,
) -> Option<PlaneError> {
    let Some(subscriber) = subscriber else {
        return decoded_video_error(image);
    };
    let Ok(origin): Result<ParticipantId, _> = publisher.parse() else {
        log.wrong_origin_frames = log.wrong_origin_frames.saturating_add(1);
        return None;
    };
    let Some(expected) = pulsebeam::sim_metrics::expected_video(subscriber, origin, timestamp)
    else {
        log.unexpected_frames = log.unexpected_frames.saturating_add(1);
        return None;
    };
    if !expected.complete {
        log.unexpected_frames = log.unexpected_frames.saturating_add(1);
        return None;
    }
    pulsebeam::sim_metrics::record_decoded_video(subscriber, origin, timestamp);
    if expected.origin.to_string() != publisher {
        log.wrong_origin_frames = log.wrong_origin_frames.saturating_add(1);
    }
    let Some(source) = pulsebeam::sim_metrics::quality_source(expected.origin) else {
        return decoded_video_error(image);
    };
    let layer = match expected.height {
        180 => QualityVideoLayer::P180,
        360 => QualityVideoLayer::P360,
        720 => QualityVideoLayer::P720,
        _ => {
            log.wrong_layer_frames = log.wrong_layer_frames.saturating_add(1);
            return None;
        }
    };
    if image.dimensions() != layer.dimensions() {
        log.wrong_layer_frames = log.wrong_layer_frames.saturating_add(1);
        return None;
    }
    let source = match source {
        0 => QualityVideoSource::Zero,
        1 => QualityVideoSource::One,
        _ => {
            debug_assert!(false, "registered quality source is valid");
            return None;
        }
    };
    let fixture = quality_corpus_video(source, layer);
    let frame = fixture.frame_for_rtp_timestamp(expected.source_timestamp)?;
    let reference = references.entry((source, layer)).or_insert_with(|| {
        fixture
            .decode_reference()
            .expect("checked-in zstd video reference")
    });
    let reference = fixture.reference_frame(reference, frame.index)?;
    let error = decoded_video_reference_error(image, reference)?;
    let mean = error.sum.checked_div(error.samples).unwrap_or(u64::MAX);
    if mean > 2 || error.max > 32 {
        log.wrong_content_frames = log.wrong_content_frames.saturating_add(1);
    }
    Some(error)
}

struct ExpectedAudioDecoder {
    decoder: opus::Decoder,
    pcm: Box<[i16]>,
}

impl ExpectedAudioDecoder {
    fn new() -> Self {
        Self {
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono)
                .expect("bundled Opus reference decoder initializes"),
            pcm: vec![0; 5_760].into_boxed_slice(),
        }
    }

    fn conceal(&mut self) {
        debug_assert!(
            self.decoder.decode(&[], &mut self.pcm, false).is_ok(),
            "Opus PLC accepts an absent packet"
        );
    }
}

struct BrowserAudioReceiver {
    subscriber: Option<ParticipantId>,
    decoder: opus::Decoder,
    expected: ExpectedAudioDecoder,
    pcm: Box<[i16]>,
    expected_timestamp: Option<u64>,
    fixture: pulsebeam_testdata::QualityAudioFixture,
}

impl BrowserAudioReceiver {
    fn new() -> Self {
        Self {
            subscriber: None,
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono)
                .expect("bundled Opus decoder initializes"),
            expected: ExpectedAudioDecoder::new(),
            pcm: vec![0; 5_760].into_boxed_slice(),
            expected_timestamp: None,
            fixture: quality_audio_fixture(),
        }
    }

    fn for_subscriber(subscriber: ParticipantId) -> Self {
        Self {
            subscriber: Some(subscriber),
            ..Self::new()
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
        let timestamp = rtp.ts.numer();
        if rtp.payload.is_empty() {
            log.record_stream_packet(ssrc, sequence);
            return;
        }
        if let Some(expected) = self.expected_timestamp {
            let missing = timestamp
                .saturating_sub(expected)
                .checked_div(QUALITY_AUDIO_FRAME_SAMPLES as u64)
                .unwrap_or_default();
            for _ in 0..missing.min(64) {
                self.expected.conceal();
                self.decode(publisher, ssrc, None, &[], true, log);
            }
            if missing > 64 {
                log.record_decoder_error(ssrc);
            }
        }
        self.expected_timestamp =
            Some(timestamp.saturating_add(QUALITY_AUDIO_FRAME_SAMPLES as u64));
        let expected_publisher = self
            .subscriber
            .and_then(|subscriber| {
                pulsebeam::sim_metrics::expected_audio(subscriber, ssrc, timestamp)
            })
            .map(|expected| expected.origin.to_string());
        let publisher = expected_publisher.as_deref().unwrap_or(publisher);
        log.record(
            publisher,
            ssrc,
            sequence,
            timestamp,
            rtp.payload.len(),
            Instant::now(),
        );
        self.decode(publisher, ssrc, Some(timestamp), &rtp.payload, false, log);
    }

    fn decode(
        &mut self,
        publisher: &str,
        ssrc: u32,
        timestamp: Option<u64>,
        packet: &[u8],
        concealed: bool,
        log: &mut AudioReceiveLog,
    ) {
        match self.decoder.decode(packet, &mut self.pcm, false) {
            Ok(samples) => {
                let Some(pcm) = self.pcm.get(..samples) else {
                    log.record_decoder_error(ssrc);
                    return;
                };
                let energy = pcm.iter().fold(0u64, |total, sample| {
                    total.saturating_add(u64::from(sample.unsigned_abs()))
                });
                let quality_reference = timestamp.and_then(|timestamp| {
                    expected_audio_reference(
                        self.subscriber,
                        &mut self.expected,
                        ssrc,
                        timestamp,
                        packet,
                        samples,
                        log,
                    )
                });
                if let Some(reference) = quality_reference {
                    let error = pcm_error(pcm, &reference);
                    if error.is_none_or(|error| error.sum > 0 || error.max > 0) {
                        log.wrong_content_packets = log.wrong_content_packets.saturating_add(1);
                    }
                    log.record_pcm(publisher, ssrc, pcm, energy, concealed, Some(&reference));
                } else {
                    let reference = timestamp
                        .and_then(|timestamp| self.fixture.frame_for_rtp_timestamp(timestamp))
                        .map(|frame| frame.reference_pcm_s16le);
                    log.record_pcm(publisher, ssrc, pcm, energy, concealed, reference);
                }
            }
            Err(_) => log.record_decoder_error(ssrc),
        }
    }
}

fn expected_audio_reference(
    subscriber: Option<ParticipantId>,
    expected_decoder: &mut ExpectedAudioDecoder,
    ssrc: u32,
    timestamp: u64,
    packet: &[u8],
    samples: usize,
    log: &mut AudioReceiveLog,
) -> Option<Vec<u8>> {
    let subscriber = subscriber?;
    let Some(expected) = pulsebeam::sim_metrics::expected_audio(subscriber, ssrc, timestamp) else {
        log.unexpected_packets = log.unexpected_packets.saturating_add(1);
        return None;
    };
    let source = match pulsebeam::sim_metrics::quality_source(expected.origin)? {
        0 => QualityAudioSource::Zero,
        1 => QualityAudioSource::One,
        _ => {
            debug_assert!(false, "registered quality source is valid");
            return None;
        }
    };
    let fixture = pulsebeam_testdata::quality_corpus_audio(source);
    let frame = fixture.frame_for_rtp_timestamp(expected.source_timestamp)?;
    if packet != frame.opus_packet {
        log.wrong_content_packets = log.wrong_content_packets.saturating_add(1);
    }
    let expected_samples = expected_decoder
        .decoder
        .decode(frame.opus_packet, &mut expected_decoder.pcm, false)
        .ok()?;
    if expected_samples != samples {
        log.wrong_content_packets = log.wrong_content_packets.saturating_add(1);
        return None;
    }
    let mut bytes = Vec::with_capacity(expected_samples.saturating_mul(2));
    for sample in expected_decoder.pcm.get(..expected_samples)? {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    Some(bytes)
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
    pub quality_by_publisher: std::collections::BTreeMap<String, DecodedAudioQuality>,
    /// Every RTP stream this listener was sent, and whether each stayed whole.
    ///
    /// Keyed by SSRC rather than by speaker on purpose. Several speakers share a slot's stream
    /// over a call, spliced onto one timeline, and that is the design: a browser cannot route by
    /// SSRC, and libwebrtc answers an SSRC it did not see in the SDP by building a whole new
    /// receive stream. What has to hold is that the shared stream is unbroken across the changes.
    pub by_stream: std::collections::BTreeMap<u32, AudioStreamContinuity>,
    pub unexpected_packets: u64,
    pub wrong_content_packets: u64,
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
    pub max_timestamp_gap: u64,
    last_seq: Option<u64>,
    last_timestamp: Option<u64>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedAudioQuality {
    pub packets: u64,
    pub reference_packets: u64,
    pub reference_mismatches: u64,
    pub concealed_packets: u64,
    pub pcm_error_sum: u64,
    pub pcm_samples: u64,
    pub pcm_max_error: u16,
    pub longest_packet_gap: Duration,
    last_packet_at: Option<Instant>,
}

impl DecodedAudioQuality {
    pub fn mean_absolute_error(self) -> Option<u64> {
        self.pcm_error_sum.checked_div(self.pcm_samples)
    }

    fn record_pcm(&mut self, pcm: &[i16], concealed: bool, reference: Option<&[u8]>) {
        let now = Instant::now();
        if let Some(last_packet_at) = self.last_packet_at {
            self.longest_packet_gap = self
                .longest_packet_gap
                .max(now.saturating_duration_since(last_packet_at));
        }
        self.last_packet_at = Some(now);
        self.packets = self.packets.saturating_add(1);
        if concealed {
            self.concealed_packets = self.concealed_packets.saturating_add(1);
            return;
        }
        let Some(reference) = reference else {
            self.reference_mismatches = self.reference_mismatches.saturating_add(1);
            return;
        };
        let Some(error) = pcm_error(pcm, reference) else {
            self.reference_mismatches = self.reference_mismatches.saturating_add(1);
            return;
        };
        self.reference_packets = self.reference_packets.saturating_add(1);
        self.pcm_error_sum = self.pcm_error_sum.saturating_add(error.sum);
        self.pcm_samples = self.pcm_samples.saturating_add(error.samples);
        self.pcm_max_error = self.pcm_max_error.max(error.max);
    }
}

#[derive(Clone, Copy)]
struct PcmError {
    sum: u64,
    samples: u64,
    max: u16,
}

fn pcm_error(pcm: &[i16], reference: &[u8]) -> Option<PcmError> {
    let expected_len = pcm.len().checked_mul(2)?;
    if reference.len() != expected_len {
        return None;
    }
    let mut sum = 0u64;
    let mut max = 0u16;
    for (actual, bytes) in pcm.iter().zip(reference.chunks_exact(2)) {
        let expected = i16::from_le_bytes(<[u8; 2]>::try_from(bytes).ok()?);
        let error = actual.abs_diff(expected);
        sum = sum.saturating_add(u64::from(error));
        max = max.max(error);
    }
    Some(PcmError {
        sum,
        samples: u64::try_from(pcm.len()).ok()?,
        max,
    })
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
    fn record_stream_packet(&mut self, ssrc: u32, seq: u64) {
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

    fn record(
        &mut self,
        publisher: &str,
        ssrc: u32,
        seq: u64,
        timestamp: u64,
        bytes: usize,
        now: Instant,
    ) {
        self.by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record(bytes, now);

        self.record_stream_packet(ssrc, seq);
        let stream = self.by_stream.entry(ssrc).or_default();
        if let Some(previous) = stream.last_timestamp {
            let gap = timestamp
                .saturating_sub(previous)
                .checked_div(QUALITY_AUDIO_FRAME_SAMPLES as u64)
                .unwrap_or_default()
                .saturating_sub(1);
            stream.max_timestamp_gap = stream.max_timestamp_gap.max(gap);
        }
        if stream
            .last_timestamp
            .is_none_or(|previous| timestamp > previous)
        {
            stream.last_timestamp = Some(timestamp);
        }
    }

    fn record_pcm(
        &mut self,
        publisher: &str,
        ssrc: u32,
        pcm: &[i16],
        energy: u64,
        concealed: bool,
        reference: Option<&[u8]>,
    ) {
        self.by_stream
            .entry(ssrc)
            .or_default()
            .record_pcm(pcm.len(), energy, concealed);
        self.quality_by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record_pcm(pcm, concealed, reference);
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
    pub capture_timed_frames: u64,
    pub max_capture_to_decode_latency: Duration,
    pub first_frame_at: Option<Instant>,
    pub last_frame_at: Option<Instant>,
    pub frozen_time: Duration,
    pub browser_packet_errors: u64,
    pub decoder_errors: u64,
    pub damaged_frames: u64,
    pub unexpected_frames: u64,
    pub wrong_origin_frames: u64,
    pub wrong_layer_frames: u64,
    pub wrong_content_frames: u64,
    pub min_decoded_width: usize,
    pub min_decoded_height: usize,
    pub max_decoded_width: usize,
    pub max_decoded_height: usize,
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
            capture_timed_frames: self
                .capture_timed_frames
                .saturating_sub(baseline.capture_timed_frames),
            max_capture_to_decode_latency: self
                .max_capture_to_decode_latency
                .max(baseline.max_capture_to_decode_latency),
            first_frame_at: self.first_frame_at.or(baseline.first_frame_at),
            last_frame_at: self.last_frame_at,
            frozen_time: self.frozen_time.saturating_sub(baseline.frozen_time),
            browser_packet_errors: self
                .browser_packet_errors
                .saturating_sub(baseline.browser_packet_errors),
            decoder_errors: self.decoder_errors.saturating_sub(baseline.decoder_errors),
            damaged_frames: self.damaged_frames.saturating_sub(baseline.damaged_frames),
            unexpected_frames: self
                .unexpected_frames
                .saturating_sub(baseline.unexpected_frames),
            wrong_origin_frames: self
                .wrong_origin_frames
                .saturating_sub(baseline.wrong_origin_frames),
            wrong_layer_frames: self
                .wrong_layer_frames
                .saturating_sub(baseline.wrong_layer_frames),
            wrong_content_frames: self
                .wrong_content_frames
                .saturating_sub(baseline.wrong_content_frames),
            min_decoded_width: self.min_decoded_width,
            min_decoded_height: self.min_decoded_height,
            max_decoded_width: self.max_decoded_width,
            max_decoded_height: self.max_decoded_height,
        }
    }
}

/// Scans an Annex-B frame for the H.264 NAL unit types it contains, using the
/// same `pulsebeam_core::h264::classify()` classifier as the production SFU forwarder.
impl VideoReceiveLog {
    pub fn begin_interval(&mut self, now: Instant) {
        self.interval_started_at = Some(now);
        self.interval_longest_frame_gap = Duration::ZERO;
        self.interval_min_decoded_width = 0;
        self.interval_min_decoded_height = 0;
        self.interval_max_decoded_width = 0;
        self.interval_max_decoded_height = 0;
    }

    pub fn interval_stats_since(&self, baseline: VideoReceiveStats) -> VideoReceiveStats {
        let mut stats = self.stats().since(baseline);
        stats.longest_frame_gap = self.interval_longest_frame_gap;
        stats.min_decoded_width = self.interval_min_decoded_width;
        stats.min_decoded_height = self.interval_min_decoded_height;
        stats.max_decoded_width = self.interval_max_decoded_width;
        stats.max_decoded_height = self.interval_max_decoded_height;
        if let Some(started_at) = self.interval_started_at {
            stats.last_frame_at = Some(
                self.last_frame_at
                    .map_or(started_at, |last| last.max(started_at)),
            );
        }
        stats
    }

    pub fn begin_first_frame_measurement(&mut self) {
        self.first_frame_since_measurement = None;
    }

    pub fn frames_from(&self, publisher: &str) -> u64 {
        self.by_publisher.get(publisher).copied().unwrap_or(0)
    }

    pub fn stats(&self) -> VideoReceiveStats {
        let min_decoded_width = self
            .quality_by_publisher
            .values()
            .map(|quality| quality.decoded_width)
            .min()
            .unwrap_or(0);
        let min_decoded_height = self
            .quality_by_publisher
            .values()
            .map(|quality| quality.decoded_height)
            .min()
            .unwrap_or(0);
        let max_decoded_width = self
            .quality_by_publisher
            .values()
            .map(|quality| quality.decoded_width)
            .max()
            .unwrap_or(0);
        let max_decoded_height = self
            .quality_by_publisher
            .values()
            .map(|quality| quality.decoded_height)
            .max()
            .unwrap_or(0);
        VideoReceiveStats {
            frames: self.frames,
            keyframes: self.keyframes,
            undecodable_keyframes: self.undecodable_keyframes,
            non_contiguous: self.non_contiguous,
            duplicate_ts_frames: self.duplicate_ts_frames,
            ts_regression_count: self.ts_regression_count,
            max_ts_regression: self.max_ts_regression,
            longest_frame_gap: self.longest_frame_gap,
            capture_timed_frames: self.capture_timed_frames,
            max_capture_to_decode_latency: self.max_capture_to_decode_latency,
            first_frame_at: self.first_frame_at,
            last_frame_at: self.last_frame_at,
            frozen_time: self.frozen_time,
            browser_packet_errors: self.browser_packet_errors(),
            decoder_errors: self.decoder_errors,
            damaged_frames: self.damaged_frames,
            unexpected_frames: self.unexpected_frames,
            wrong_origin_frames: self.wrong_origin_frames,
            wrong_layer_frames: self.wrong_layer_frames,
            wrong_content_frames: self.wrong_content_frames,
            min_decoded_width,
            min_decoded_height,
            max_decoded_width,
            max_decoded_height,
        }
    }

    fn record_decoded(
        &mut self,
        publisher: &str,
        ts: u64,
        is_keyframe: bool,
        capture_time: Option<SystemTime>,
        image: &impl YUVSource,
        reference_error: Option<PlaneError>,
    ) {
        let now = Instant::now();
        if let Some(capture_time) = capture_time
            && let Ok(latency) =
                pulsebeam_agent::clock::wallclock_at(now.into()).duration_since(capture_time)
        {
            self.capture_timed_frames = self.capture_timed_frames.saturating_add(1);
            self.max_capture_to_decode_latency = self.max_capture_to_decode_latency.max(latency);
        }
        if let Some(previous) = self.last_frame_at {
            let gap = now.saturating_duration_since(previous);
            self.longest_frame_gap = self.longest_frame_gap.max(gap);
            if gap > FREEZE_THRESHOLD {
                self.frozen_time = self.frozen_time.saturating_add(gap);
            }
        }
        if let Some(started_at) = self.interval_started_at {
            let previous = self
                .last_frame_at
                .map_or(started_at, |last| last.max(started_at));
            let gap = now.saturating_duration_since(previous);
            self.interval_longest_frame_gap = self.interval_longest_frame_gap.max(gap);
        }
        self.first_frame_at.get_or_insert(now);
        self.first_frame_since_measurement.get_or_insert(now);
        self.last_frame_at = Some(now);
        *self.by_publisher.entry(publisher.to_owned()).or_default() += 1;
        let (width, height) = image.dimensions();
        if self.interval_started_at.is_some() {
            self.interval_min_decoded_width = if self.interval_min_decoded_width == 0 {
                width
            } else {
                self.interval_min_decoded_width.min(width)
            };
            self.interval_min_decoded_height = if self.interval_min_decoded_height == 0 {
                height
            } else {
                self.interval_min_decoded_height.min(height)
            };
            self.interval_max_decoded_width = self.interval_max_decoded_width.max(width);
            self.interval_max_decoded_height = self.interval_max_decoded_height.max(height);
        }
        self.quality_by_publisher
            .entry(publisher.to_owned())
            .or_default()
            .record_frame(width, height, reference_error);
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
    video_receivers: Arc<Mutex<BTreeMap<String, BrowserVideoReceiver>>>,
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
                            .insert(publication_id.clone(), publication_id.clone());
                        let publisher_id = track.publisher_id().to_owned();
                        let subscriber_id: ParticipantId = self
                            .ctx
                            .agent
                            .participant_id()
                            .parse()
                            .expect("agent participant id");
                        let log = self.ctx.video_rx.clone();
                        let receivers = self.ctx.video_receivers.clone();
                        self.join_set.spawn(async move {
                            let mut keyframe_retry =
                                tokio::time::interval(Duration::from_secs(1));
                            keyframe_retry.set_missed_tick_behavior(
                                tokio::time::MissedTickBehavior::Skip,
                            );
                            loop {
                                enum Next {
                                    Packet(Result<pulsebeam_agent::RtpPacket, pulsebeam_agent::agent::RecvError>),
                                    Retry,
                                }
                                let next = tokio::select! {
                                    packet = track.recv() => Next::Packet(packet),
                                    _ = keyframe_retry.tick() => Next::Retry,
                                };
                                let request_keyframe = match next {
                                    Next::Packet(Ok(rtp)) => {
                                        let mut receivers = receivers.lock().unwrap();
                                        let receiver = receivers
                                            .entry(publisher_id.clone())
                                            .or_insert_with(|| {
                                                BrowserVideoReceiver::for_subscriber(subscriber_id)
                                            });
                                        let mut log = log.lock().unwrap();
                                        receiver.push(rtp, &mut log, &publisher_id)
                                    }
                                    Next::Packet(Err(_)) => break,
                                    Next::Retry => receivers
                                        .lock()
                                        .unwrap()
                                        .get(&publisher_id)
                                        .is_some_and(BrowserVideoReceiver::needs_keyframe),
                                };
                                if request_keyframe {
                                    track.request_keyframe();
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

#[cfg(test)]
mod decoder_tests {
    use super::*;

    #[test]
    fn bundled_openh264_decodes_the_h264_fixture() {
        let mut decoder = H264Decoder::new().unwrap();
        let image = decoder
            .decode(
                pulsebeam_testdata::quality_video_frame(0)
                    .expect("quality fixture frame")
                    .encoded,
            )
            .unwrap();
        assert!(image.is_some());
    }

    #[test]
    fn bundled_openh264_decodes_the_full_simulcast_fixture_continuously() {
        let mut decoder = H264Decoder::new().unwrap();
        for (index, frame) in
            pulsebeam_agent::media::H264FrameSlicer::new(pulsebeam_testdata::RAW_H264_FULL_CBR)
                .take(1_000)
                .enumerate()
        {
            assert!(
                decoder.decode(frame).is_ok(),
                "OpenH264 rejected full-resolution source frame {index}"
            );
        }
    }

    #[test]
    fn bundled_openh264_decodes_the_scheduled_screenshare_fixture_continuously() {
        let mut decoder = H264Decoder::new().unwrap();
        for (index, frame) in pulsebeam_agent::media::H264FrameSlicer::new(
            pulsebeam_testdata::RAW_H264_SCREEN_FULL_VBR,
        )
        .enumerate()
        {
            assert!(
                decoder.decode(frame).is_ok_and(|image| image.is_some()),
                "OpenH264 did not render screen-share source frame {index}"
            );
        }
    }

    #[test]
    fn bundled_openh264_decodes_a_full_half_full_idr_sequence() {
        let mut decoder = H264Decoder::new().unwrap();
        for (label, fixture) in [
            ("full", pulsebeam_testdata::RAW_H264_FULL_CBR),
            ("half", pulsebeam_testdata::RAW_H264_HALF_CBR),
            ("full", pulsebeam_testdata::RAW_H264_FULL_CBR),
        ] {
            for (index, frame) in pulsebeam_agent::media::H264FrameSlicer::new(fixture)
                .take(300)
                .enumerate()
            {
                assert!(
                    decoder.decode(frame).is_ok(),
                    "OpenH264 rejected {label} source frame {index} after a simulcast switch"
                );
            }
        }
    }

    #[test]
    fn h264_packetization_preserves_the_full_simulcast_fixture() {
        let packetizer =
            pulsebeam_core::h264::Packetizer::new(pulsebeam_core::framing::DEFAULT_MTU_PAYLOAD);
        let mut receiver = BrowserVideoReceiver::new();
        for (index, frame) in
            pulsebeam_agent::media::H264FrameSlicer::new(pulsebeam_testdata::RAW_H264_FULL_CBR)
                .take(1_000)
                .enumerate()
        {
            receiver.access_unit.clear();
            receiver.fu_header = None;
            for packet in packetizer.packetize(frame) {
                assert!(
                    receiver.append_rtp_payload(&packet.payload),
                    "could not depacketize full-resolution source frame {index}"
                );
            }
            assert!(receiver.fu_header.is_none());
            assert!(
                receiver.decoder.decode(&receiver.access_unit).is_ok(),
                "OpenH264 rejected packetized full-resolution source frame {index}"
            );
        }
    }

    #[test]
    fn h264_packetization_preserves_a_full_half_full_sequence() {
        let packetizer =
            pulsebeam_core::h264::Packetizer::new(pulsebeam_core::framing::DEFAULT_MTU_PAYLOAD);
        let mut receiver = BrowserVideoReceiver::new();
        for (label, fixture) in [
            ("full", pulsebeam_testdata::RAW_H264_FULL_CBR),
            ("half", pulsebeam_testdata::RAW_H264_HALF_CBR),
            ("full", pulsebeam_testdata::RAW_H264_FULL_CBR),
        ] {
            for (index, frame) in pulsebeam_agent::media::H264FrameSlicer::new(fixture)
                .take(300)
                .enumerate()
            {
                receiver.access_unit.clear();
                receiver.fu_header = None;
                for packet in packetizer.packetize(frame) {
                    assert!(receiver.append_rtp_payload(&packet.payload));
                }
                assert!(receiver.fu_header.is_none());
                assert!(
                    receiver.decoder.decode(&receiver.access_unit).is_ok(),
                    "OpenH264 rejected packetized {label} source frame {index} after a simulcast switch"
                );
            }
        }
    }

    #[test]
    fn a_whole_lost_delta_frame_waits_for_an_idr_without_poisoning_the_decoder() {
        let frames: Vec<_> =
            pulsebeam_agent::media::H264FrameSlicer::new(pulsebeam_testdata::RAW_H264_FULL_CBR)
                .take(3)
                .collect();
        assert_eq!(frames.len(), 3);
        let packetizer =
            pulsebeam_core::h264::Packetizer::new(pulsebeam_core::framing::DEFAULT_MTU_PAYLOAD);
        let mut next_seq = 0u64;
        let mut packets = |frame: &[u8], timestamp: u64| {
            packetizer
                .packetize(frame)
                .into_iter()
                .map(|packet| {
                    let sequence = next_seq;
                    next_seq = next_seq.wrapping_add(1);
                    pulsebeam_agent::RtpPacket {
                        mid: pulsebeam_agent::Mid::from("v0"),
                        rid: None,
                        seq: pulsebeam_agent::SeqNo::from(sequence),
                        ts: pulsebeam_agent::MediaTime::from_90khz(timestamp),
                        marker: packet.end_of_frame,
                        payload_type: Some(96),
                        ssrc: Some(pulsebeam_agent::Ssrc::from(1)),
                        payload: Arc::from(packet.payload),
                        ext_vals: pulsebeam_agent::ExtensionValues::default(),
                        arrival: tokio::time::Instant::now(),
                    }
                })
                .collect::<Vec<_>>()
        };

        let first = packets(frames[0], 0);
        let _lost = packets(frames[1], 3_000);
        let after_loss = packets(frames[2], 6_000);
        let recovery = packets(frames[0], 9_000);
        let mut receiver = BrowserVideoReceiver::new();
        let mut log = VideoReceiveLog::default();

        for packet in first {
            assert!(!receiver.process(packet, &mut log, "publisher"));
        }
        assert_eq!(log.frames, 1);
        let mut requested = false;
        for packet in after_loss {
            requested |= receiver.process(packet, &mut log, "publisher");
        }
        assert!(requested);
        assert_eq!(log.frames, 1);
        assert_eq!(log.decoder_errors, 0);
        for packet in recovery {
            let _ = receiver.process(packet, &mut log, "publisher");
        }
        assert_eq!(log.frames, 2);
        assert_eq!(log.decoder_errors, 0);
        assert_eq!(log.undecodable_keyframes, 0);
    }

    #[test]
    fn bundled_opus_decodes_the_audio_fixture() {
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut pcm = Box::<[i16]>::from([0; 5_760]);
        let fixture = pulsebeam_testdata::quality_audio_fixture();
        let frame = fixture.frame(0).expect("quality fixture frame");
        let samples = decoder.decode(frame.opus_packet, &mut pcm, false).unwrap();
        assert_eq!(samples, 960);
        assert!(pcm[..samples].iter().any(|sample| *sample != 0));
        let error =
            pcm_error(&pcm[..samples], frame.reference_pcm_s16le).expect("fixture PCM reference");
        assert!(error.samples > 0);
        assert!(
            error.sum.checked_div(error.samples).unwrap_or(u64::MAX)
                <= MAX_FIXTURE_PCM_MEAN_ABSOLUTE_ERROR
        );
    }

    #[test]
    fn decoded_video_records_fixture_fidelity_and_resolution() {
        let frame = pulsebeam_testdata::quality_video_frame(0).expect("quality fixture frame");
        let mut decoder = H264Decoder::new().unwrap();
        let image = decoder
            .decode(frame.encoded)
            .unwrap()
            .expect("decoded frame");
        let mut log = VideoReceiveLog::default();
        log.record_decoded(
            "publisher",
            frame.rtp_timestamp,
            true,
            None,
            &image,
            decoded_video_error(&image),
        );
        let quality = log
            .quality_by_publisher
            .get("publisher")
            .copied()
            .expect("publisher quality");
        assert_eq!(quality.frames, 1);
        assert_eq!(quality.reference_frames, 1);
        assert_eq!(quality.reference_mismatches, 0);
        assert_eq!(quality.decoded_width, frame.width);
        assert_eq!(quality.decoded_height, frame.height);
        assert!(quality.mean_absolute_error().is_some());
    }

    #[test]
    fn fidelity_metrics_reject_missing_or_wrong_references() {
        assert!(plane_error(&[0, 0], &[0, 0], 2, 2, 1).is_some());
        assert!(plane_error(&[0], &[0, 0], 2, 2, 1).is_none());
        assert!(pcm_error(&[1], &[1]).is_none());
        let mut video = DecodedVideoQuality::default();
        video.record_frame(1, 1, None);
        assert_eq!(video.reference_mismatches, 1);
        let mut audio = DecodedAudioQuality::default();
        audio.record_pcm(&[1], false, Some(&[0, 0]));
        assert_eq!(audio.reference_packets, 1);
        assert!(audio.mean_absolute_error().is_some());
    }

    #[test]
    fn exact_video_oracle_classifies_missing_layer_source_and_stale_output() {
        let subscriber = ParticipantId::from_bytes([31; 16]);
        let origin = ParticipantId::from_bytes([32; 16]);
        pulsebeam::sim_metrics::register_quality_source(origin, 0);
        let publisher = origin.to_string();
        let fixture = quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P180);
        let mut references = BTreeMap::new();
        let mut log = VideoReceiveLog::default();
        let mut decoder = H264Decoder::new().expect("OpenH264");

        let first = fixture.frame(0).expect("first frame");
        pulsebeam::sim_metrics::record_expected_video(
            subscriber,
            origin,
            10,
            first.rtp_timestamp,
            180,
            true,
        );
        let image = decoder
            .decode(first.encoded)
            .expect("decode")
            .expect("image");
        let error = expected_video_error(
            Some(subscriber),
            &mut references,
            &publisher,
            10,
            &image,
            &mut log,
        )
        .expect("reference error");
        assert_eq!(error.sum, 0);
        assert_eq!(log.wrong_content_frames, 0);

        pulsebeam::sim_metrics::record_expected_video(
            subscriber,
            origin,
            11,
            first.rtp_timestamp,
            720,
            true,
        );
        let _ = expected_video_error(
            Some(subscriber),
            &mut references,
            &publisher,
            11,
            &image,
            &mut log,
        );
        assert_eq!(log.wrong_layer_frames, 1);

        let mut wrong_source_decoder = H264Decoder::new().expect("OpenH264");
        let wrong_source = quality_corpus_video(QualityVideoSource::One, QualityVideoLayer::P180)
            .frame(0)
            .expect("wrong source frame");
        let wrong_source_image = wrong_source_decoder
            .decode(wrong_source.encoded)
            .expect("decode")
            .expect("image");
        pulsebeam::sim_metrics::record_expected_video(
            subscriber,
            origin,
            12,
            first.rtp_timestamp,
            180,
            true,
        );
        let _ = expected_video_error(
            Some(subscriber),
            &mut references,
            &publisher,
            12,
            &wrong_source_image,
            &mut log,
        );
        assert_eq!(log.wrong_content_frames, 1);

        let second = fixture.frame(1).expect("second frame");
        let stale_image = decoder
            .decode(second.encoded)
            .expect("decode")
            .expect("image");
        pulsebeam::sim_metrics::record_expected_video(
            subscriber,
            origin,
            13,
            first.rtp_timestamp,
            180,
            true,
        );
        let _ = expected_video_error(
            Some(subscriber),
            &mut references,
            &publisher,
            13,
            &stale_image,
            &mut log,
        );
        assert_eq!(log.wrong_content_frames, 2);

        let _ = expected_video_error(
            Some(subscriber),
            &mut references,
            &publisher,
            14,
            &stale_image,
            &mut log,
        );
        assert_eq!(log.unexpected_frames, 1);
    }

    #[test]
    fn exact_video_progress_is_complete_decoded_and_window_scoped() {
        pulsebeam::sim_metrics::reset();
        let subscriber = ParticipantId::from_bytes([51; 16]);
        let origin = ParticipantId::from_bytes([52; 16]);
        pulsebeam::sim_metrics::record_expected_video(subscriber, origin, 1, 10, 180, false);
        pulsebeam::sim_metrics::record_expected_video(subscriber, origin, 2, 20, 180, true);
        pulsebeam::sim_metrics::record_expected_video(subscriber, origin, 3, 30, 180, true);
        pulsebeam::sim_metrics::record_decoded_video(subscriber, origin, 2);

        assert_eq!(
            pulsebeam::sim_metrics::expected_video_progress(subscriber, Duration::ZERO),
            (2, 1)
        );

        pulsebeam::sim_metrics::reset();
        assert_eq!(
            pulsebeam::sim_metrics::expected_video_progress(subscriber, Duration::ZERO),
            (0, 0)
        );
    }

    #[test]
    fn exact_audio_oracle_uses_the_accepted_source_packet_and_decoder_state() {
        let subscriber = ParticipantId::from_bytes([41; 16]);
        let origin = ParticipantId::from_bytes([42; 16]);
        pulsebeam::sim_metrics::register_quality_source(origin, 0);
        let fixture = pulsebeam_testdata::quality_corpus_audio(QualityAudioSource::Zero);
        let first = fixture.frame(0).expect("first packet");
        pulsebeam::sim_metrics::record_expected_audio(
            subscriber,
            7,
            origin,
            20,
            first.rtp_timestamp,
        );
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).expect("decoder");
        let mut expected_decoder = ExpectedAudioDecoder::new();
        let mut pcm = [0i16; QUALITY_AUDIO_FRAME_SAMPLES];
        let samples = decoder
            .decode(first.opus_packet, &mut pcm, false)
            .expect("decode");
        let mut log = AudioReceiveLog::default();
        let reference = expected_audio_reference(
            Some(subscriber),
            &mut expected_decoder,
            7,
            20,
            first.opus_packet,
            samples,
            &mut log,
        )
        .expect("expected PCM");
        let error = pcm_error(&pcm[..samples], &reference).expect("PCM error");
        assert_eq!((error.sum, error.max), (0, 0));

        let second = fixture.frame(1).expect("second packet");
        pulsebeam::sim_metrics::record_expected_audio(
            subscriber,
            7,
            origin,
            21,
            second.rtp_timestamp,
        );
        let wrong = pulsebeam_testdata::quality_corpus_audio(QualityAudioSource::One)
            .frame(1)
            .expect("wrong packet");
        let _ = expected_audio_reference(
            Some(subscriber),
            &mut expected_decoder,
            7,
            21,
            wrong.opus_packet,
            QUALITY_AUDIO_FRAME_SAMPLES,
            &mut log,
        );
        assert_eq!(log.wrong_content_packets, 1);

        let _ = expected_audio_reference(
            Some(subscriber),
            &mut expected_decoder,
            7,
            22,
            second.opus_packet,
            QUALITY_AUDIO_FRAME_SAMPLES,
            &mut log,
        );
        assert_eq!(log.unexpected_packets, 1);
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
