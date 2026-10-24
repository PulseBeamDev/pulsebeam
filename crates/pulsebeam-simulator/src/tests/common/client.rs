use super::decoder::{DecodeError, H264ReferenceDecoder, OpusReferenceDecoder, ReferenceError};
use super::media::{
    FixtureEncoding, KeyframeRecoveryProbe, VbrProfile, VbrSource, VideoSource, video_frames,
};
use super::native::NativeHost;
use super::provenance::{self, FixturePublisher, FixturePublishers};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, AudioSubscription, ConnectionState, DesiredState, MediaKind, MediaTopology,
    PublicationIntent, ResourceCommand, ResourceResult, TopicMode, TopicNotification,
    TopicPublisher, TopicSend, TopicSubscriber, TrackSpec, VideoSubscription,
};
use pulsebeam_agent_native::{
    Agent, AgentEvent, AsyncHttpClient, AudioInput, Config, Encoding, Host, InputFrame,
    MediaConfig, VideoInput, VideoPublicationConfig, VideoSourceConfig,
};
use pulsebeam_core::identity::{ParticipantExternalId, RoomExternalId};
use pulsebeam_core::net::{HttpError, HttpRequest, HttpResult};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::task::JoinSet;
// The process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per
// host, so a timestamp taken here cannot be compared with one taken on the coordinator. See
// `sim_clock`, which shims `clock_gettime` for the whole process.
use std::time::Instant;

pub struct SimClientBuilder {
    ip: IpAddr,
    admission: Arc<Mutex<Option<pulsebeam::entity::ConnectionId>>>,
    host: Host,
    native_host: NativeHost,
    endpoint: String,
    video_layers: Option<Vec<FixtureEncoding>>,
    video_slots: usize,
    audio_slots: usize,
    manual_subscriptions: bool,
    driver_admission_retry: bool,
    video_rx: Option<Arc<Mutex<VideoReceiveLog>>>,
    audio_rx: Option<Arc<Mutex<AudioReceiveLog>>>,
    publishes_video: bool,
    owned_video_label: Option<String>,
    /// When set, publish with a variable-bitrate source instead of the constant-rate looper.
    vbr_profile: Option<VbrProfile>,
    /// Request an actual stock-encoder temporal fixture. Zero disables DD.
    temporal_dd: Option<u8>,
    video_receive_mode: pulsebeam_agent_native::ReceiveMode,
    native_playback_only: bool,
    audio_receive_mode: pulsebeam_agent_native::ReceiveMode,
    audio_input: AudioInput,
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
    keyframe_recovery: Arc<Mutex<KeyframeRecoveryProbe>>,
    initial_topics: pulsebeam_agent_native::agent_core::TopicRegistrations,
    quality_references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<FixturePublishers>>,
}

/// Exact encoded-input evidence. Pixel-quality references are optional: generic
/// CBR fixtures prove content origin, not corpus identity or pixel fidelity.
pub(crate) struct QualityVideoReference {
    quality: Option<QualityVideoCorpus>,
    encoded_frames: BTreeMap<Vec<u8>, usize>,
    frames: Arc<[Arc<[u8]>]>,
}

struct QualityVideoCorpus {
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
    corpus: pulsebeam_testdata::QualityCorpusVideo,
    decoded: Vec<u8>,
}

fn quality_video_reference(
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
) -> &'static QualityVideoReference {
    use pulsebeam_testdata::{QualityVideoLayer::*, QualityVideoSource::*};
    // Share immutable corpus data only. Every receiver still owns its decoder.
    static REFERENCES: [OnceLock<QualityVideoReference>; 6] = [const { OnceLock::new() }; 6];
    let index = match (source, layer) {
        (Zero, P180) => 0,
        (Zero, P360) => 1,
        (Zero, P720) => 2,
        (One, P180) => 3,
        (One, P360) => 4,
        (One, P720) => 5,
    };
    REFERENCES
        .get(index)
        .expect("quality corpus index")
        .get_or_init(|| {
            let corpus = pulsebeam_testdata::quality_corpus_video(source, layer);
            let decoded = corpus
                .decode_reference()
                .unwrap_or_else(|error| panic!("quality H.264 reference failed: {error}"));
            let mut encoded_frames = BTreeMap::new();
            let mut data = Vec::new();
            for index in 0..corpus.len() {
                let frame = corpus.frame(index).expect("quality H.264 frame");
                // Retain the first exact match, including for repeated corpus frames.
                encoded_frames
                    .entry(canonical_annex_b(frame.encoded))
                    .or_insert(frame.index);
                data.extend_from_slice(frame.encoded);
            }
            QualityVideoReference {
                quality: Some(QualityVideoCorpus {
                    source,
                    layer,
                    corpus,
                    decoded,
                }),
                encoded_frames,
                frames: video_frames(&data),
            }
        })
}

fn quality_encoded_frames(
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
) -> Arc<[Arc<[u8]>]> {
    use pulsebeam_testdata::{QualityVideoLayer::*, QualityVideoSource::*};
    type EncodedFrames = Arc<[Arc<[u8]>]>;
    static FRAMES: [OnceLock<EncodedFrames>; 6] = [const { OnceLock::new() }; 6];
    let index = match (source, layer) {
        (Zero, P180) => 0,
        (Zero, P360) => 1,
        (Zero, P720) => 2,
        (One, P180) => 3,
        (One, P360) => 4,
        (One, P720) => 5,
    };
    FRAMES
        .get(index)
        .expect("quality input index")
        .get_or_init(|| {
            let corpus = pulsebeam_testdata::quality_corpus_video(source, layer);
            let mut data = Vec::new();
            for index in 0..corpus.len() {
                data.extend_from_slice(corpus.frame(index).expect("quality H.264 input").encoded);
            }
            video_frames(&data)
        })
        .clone()
}

// A multi-encoding quality publisher offers genuine 180/360/720 corpus
// inputs. Their union proves exact encoded content origin, not one corpus's
// pixel-quality score across differently sized layers.
fn quality_ladder_reference(
    source: pulsebeam_testdata::QualityVideoSource,
) -> &'static QualityVideoReference {
    use pulsebeam_testdata::{QualityVideoLayer::*, QualityVideoSource::*};
    static REFERENCES: [OnceLock<QualityVideoReference>; 2] = [const { OnceLock::new() }; 2];
    REFERENCES
        .get(match source {
            Zero => 0,
            One => 1,
        })
        .expect("quality ladder source")
        .get_or_init(|| {
            let mut frames = Vec::new();
            let mut encoded_frames = BTreeMap::new();
            for layer in [P180, P360, P720] {
                let input = quality_encoded_frames(source, layer);
                for frame in input.iter() {
                    encoded_frames
                        .entry(canonical_annex_b(frame))
                        .or_insert(frames.len());
                    frames.push(frame.clone());
                }
            }
            QualityVideoReference {
                quality: None,
                encoded_frames,
                frames: frames.into(),
            }
        })
}

fn canonical_annex_b(data: &[u8]) -> Vec<u8> {
    provenance::inspect(data).map_or_else(|_| data.to_vec(), |(_, canonical)| canonical)
}

fn http_base_uri(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

impl SimClientBuilder {
    pub async fn bind(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let native_host = NativeHost::bind(ip)?;
        let admission = Arc::new(Mutex::new(None));
        Ok(Self {
            ip,
            admission: admission.clone(),
            host: Host::new(Box::new(AdmissionClient {
                inner: create_http_client(),
                admission,
            })),
            native_host,
            endpoint: http_base_uri(server_ip, 7070),
            video_layers: None,
            video_slots: 0,
            audio_slots: 0,
            manual_subscriptions: false,
            driver_admission_retry: false,
            video_rx: None,
            audio_rx: None,
            publishes_video: false,
            owned_video_label: None,
            vbr_profile: None,
            temporal_dd: None,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            native_playback_only: false,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: AudioInput::Pcm,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            opaque_payload: false,
            quality_video: None,
            corrupt_video_payload: false,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            keyframe_recovery: Arc::default(),
            initial_topics: Default::default(),
            quality_references: Arc::new(Mutex::new(BTreeMap::new())),
            h264_publishers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// The native ICE agent selects the TCP candidates advertised by the
    /// TCP-only SFU. The same runtime providers carry both UDP and TCP.
    pub async fn bind_tcp(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        Self::bind(ip, server_ip).await
    }

    pub fn publish_video(mut self, simulcast_layers: Option<Vec<FixtureEncoding>>) -> Self {
        self.video_layers = simulcast_layers;
        self.publishes_video = true;
        self
    }

    pub fn publish_owned_video(mut self, label: &str) -> Self {
        self.publishes_video = true;
        self.owned_video_label = Some(label.into());
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
    pub fn with_audio_input(mut self, input: AudioInput) -> Self {
        self.audio_input = input;
        self
    }

    pub fn with_audio_receive_mode(mut self, mode: pulsebeam_agent_native::ReceiveMode) -> Self {
        self.audio_receive_mode = mode;
        self
    }

    pub fn with_video_receive_mode(mut self, mode: pulsebeam_agent_native::ReceiveMode) -> Self {
        self.video_receive_mode = mode;
        self
    }

    pub fn with_temporal_dd(mut self, layers: u8) -> Self {
        self.temporal_dd = Some(layers);
        self
    }

    /// Make the published payload opaque, simulating SFrame/E2EE.
    pub fn with_opaque_payload(mut self) -> Self {
        self.opaque_payload = true;
        self
    }

    pub fn with_native_playback_only(mut self, enabled: bool) -> Self {
        self.native_playback_only = enabled;
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

    pub(crate) fn with_keyframe_recovery_probe(
        mut self,
        probe: Arc<Mutex<KeyframeRecoveryProbe>>,
    ) -> Self {
        self.keyframe_recovery = probe;
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

    pub fn with_driver_admission_retry(mut self, enabled: bool) -> Self {
        self.driver_admission_retry = enabled;
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

    pub fn with_audio_rx(mut self, rx: Arc<Mutex<AudioReceiveLog>>) -> Self {
        self.audio_rx = Some(rx);
        self
    }

    pub(crate) fn with_quality_references(
        mut self,
        references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    ) -> Self {
        self.quality_references = references;
        self
    }

    pub(crate) fn with_h264_publishers(
        mut self,
        publishers: Arc<Mutex<FixturePublishers>>,
    ) -> Self {
        self.h264_publishers = publishers;
        self
    }

    pub fn with_video_rx(mut self, rx: Arc<Mutex<VideoReceiveLog>>) -> Self {
        self.video_rx = Some(rx);
        self
    }

    pub(crate) async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let topology = MediaTopology {
            local_video: u8::from(self.publishes_video),
            local_audio: u8::from(self.audio_level_dbov.is_some()),
            remote_video: u8::try_from(self.video_slots)?,
            remote_audio: u8::try_from(self.audio_slots)?,
        };
        let participant = match self.ip {
            IpAddr::V4(ip) => format!("sim-{}", u32::from(ip)),
            IpAddr::V6(ip) => format!("sim-{ip:032X}", ip = u128::from(ip)),
        };
        let room = RoomExternalId::new(room)?;
        let participant = ParticipantExternalId::new(&participant)?;
        let session = AgentConfig {
            endpoint: self.endpoint,
            token: pulsebeam_server::sign_participant_token(
                &pulsebeam_server::insecure_dev_api_key().unwrap(),
                (room).as_str(),
                (participant).as_str(),
                u64::MAX,
            )?,
            topology,
            retry: Default::default(),
            log_level: Default::default(),
        };
        let encodings: Vec<Option<String>> = self
            .video_layers
            .as_ref()
            .map(|layers| layers.iter().map(|layer| Some(layer.rid.clone())).collect())
            .unwrap_or_else(|| vec![None]);
        let temporal_layers = self.temporal_dd.filter(|layers| *layers > 0).unwrap_or(1);
        anyhow::ensure!(
            temporal_layers == 1 || (self.quality_video.is_none() && self.vbr_profile.is_none()),
            "temporal fixtures need actual stock-encoder output, not relabelled corpus units"
        );
        let native_fixture = if self.publishes_video && temporal_layers > 1 {
            Some(self.native_host.h264_fixture(temporal_layers)?)
        } else {
            None
        };
        let vbr_source = if self.publishes_video && self.quality_video.is_none() {
            self.vbr_profile
                .map(|profile| create_vbr_source(encodings[0].as_deref(), profile))
        } else {
            None
        };
        anyhow::ensure!(
            vbr_source.is_none() || encodings.len() == 1,
            "VBR fixture has one native encoding"
        );
        let video_sources: Vec<_> = if self.publishes_video && vbr_source.is_none() {
            encodings
                .iter()
                .map(|encoding| {
                    let mut source = if let Some(fixture) = &native_fixture {
                        VideoSource::from_native(fixture, 30)
                    } else if let Some((source, layer)) = self.quality_video {
                        let layer = if encodings.len() > 1 {
                            use pulsebeam_testdata::QualityVideoLayer::*;
                            match encoding.as_deref() {
                                Some("q") => P180,
                                Some("h") => P360,
                                Some("f") => P720,
                                _ => layer,
                            }
                        } else {
                            layer
                        };
                        create_quality_video_source(source, layer)
                    } else {
                        create_video_source(encoding.as_deref())
                    };
                    source = source.with_recovery_probe(self.keyframe_recovery.clone());
                    if self.suppress_natural_keyframe_repeats {
                        source = source.without_natural_keyframe_repeats();
                    }
                    if self.opaque_payload {
                        source = source.opaque();
                    }
                    (encoding.clone(), source)
                })
                .collect()
        } else {
            Vec::new()
        };
        let dimensions: Vec<_> = if let Some(source) = &vbr_source {
            vec![source.dimensions()]
        } else {
            video_sources
                .iter()
                .map(|(_, source)| source.dimensions())
                .collect()
        };
        let max_width = dimensions
            .iter()
            .map(|(width, _)| *width)
            .max()
            .unwrap_or(1);
        let source_profile = VideoSourceConfig {
            dependency_descriptor: self.temporal_dd != Some(0),
            encodings: encodings
                .iter()
                .zip(&dimensions)
                .map(|(rid, (width, _))| Encoding {
                    rid: rid.clone(),
                    scale_down_by: f64::from(max_width) / f64::from(*width),
                    max_bitrate_bps: Some(match rid.as_deref() {
                        Some("q") => 100_000,
                        Some("f") => 1_000_000,
                        _ => 400_000,
                    }),
                    max_framerate: Some(30.0),
                    temporal_layers,
                })
                .collect(),
        };
        // Install the complete native callback collector before creating or
        // attaching a receiver. It never depends on catalog assignments.
        let video_rx = self
            .video_rx
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(VideoReceiveLog::default())));
        let dual =
            self.video_receive_mode == pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded;
        {
            let mut log = video_rx.lock().unwrap();
            anyhow::ensure!(
                !self.native_playback_only || (dual && !self.corrupt_video_payload),
                "native playback needs dual observations and cannot corrupt only a probe"
            );
            log.native_callback_enabled = true;
            log.native_origin_enabled = dual;
            log.native_playback_only = self.native_playback_only;
        }
        let mut config = Config::new(session);
        let callback_log = video_rx.clone();
        let callback_clock = super::native::callback_clock()?;
        let callback_failures = video_rx
            .lock()
            .unwrap()
            .native_decoded
            .callback_failure_counter();
        config.decoded_video_observer = Some(pulsebeam_agent_native::DecodedVideoObserver::new(
            move |observation| {
                // Never wait on application log retention from a native
                // callback. Any inability to collect a fact is a sticky failure.
                let Ok(mut log) = callback_log.try_lock() else {
                    callback_failures.fetch_add(1, std::sync::atomic::Ordering::Release);
                    return;
                };
                let Ok(at) = callback_clock.project(observation.native_observed_at_us) else {
                    callback_failures.fetch_add(1, std::sync::atomic::Ordering::Release);
                    return;
                };
                match observation.frame {
                    Ok(frame) if dual => {
                        log.record_dual_native_decoded(&frame, at);
                    }
                    Ok(frame) => log.record_native_decoded(&frame, at),
                    Err(error) => {
                        log.native_decoded.malformed_callback(&observation.track);
                        let _ = error; // Invalid is cumulative, never a fabricated frame.
                    }
                }
            },
        ));
        config.media = MediaConfig {
            audio_input: self.audio_input,
            video_input: VideoInput::EncodedH264,
            video_publications: self
                .publishes_video
                .then(|| VideoPublicationConfig {
                    slot: "v0".into(),
                    source: source_profile,
                })
                .into_iter()
                .collect(),
        };

        let join_set = JoinSet::new();
        let (agent, actor) =
            Agent::managed_with_services(config, self.host, move || self.native_host.runtime())?;
        let actor = tokio::task::spawn_local(actor);
        let owned_video = if let Some(label) = &self.owned_video_label {
            let ResourceResult::Publication { owner, slot, .. } = agent
                .resource(ResourceCommand::ClaimPublication {
                    spec: TrackSpec::Video {
                        label: label.clone(),
                    },
                })
                .await?
            else {
                anyhow::bail!("owned video claim returned the wrong resource")
            };
            anyhow::ensure!(slot == "v0", "owned video did not receive v0");
            agent
                .resource(ResourceCommand::UpdatePublication {
                    owner,
                    present: true,
                })
                .await?;
            Some(owner)
        } else {
            None
        };
        let desired = DesiredState {
            revision: 1,
            connected: true,
            publications: (self.publishes_video && self.owned_video_label.is_none())
                .then(|| PublicationIntent {
                    slot: "v0".into(),
                    label: "video".into(),
                    active: true,
                })
                .into_iter()
                .chain(self.audio_level_dbov.map(|_| PublicationIntent {
                    slot: "a0".into(),
                    label: "audio".into(),
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
            if self.driver_admission_retry
                && matches!(
                    snapshots.borrow().connection,
                    ConnectionState::RetryWaiting { .. } | ConnectionState::TerminalFailure
                )
            {
                agent.abort().await?;
                anyhow::bail!("admission failed; waiting for explicit driver retry");
            }
            snapshots.changed().await?;
        }
        let participant_id = snapshots
            .borrow()
            .participant_id
            .clone()
            .expect("connected snapshot has an identity");
        let reference = if let Some((source, layer)) = self.quality_video {
            Some(if encodings.len() > 1 {
                quality_ladder_reference(source)
            } else {
                quality_video_reference(source, layer)
            })
        } else if self.publishes_video
            && !self.opaque_payload
            && native_fixture.is_none()
            && vbr_source.is_none()
        {
            Some(generic_video_reference(&encodings))
        } else {
            None
        };
        if let Some(reference) = reference {
            self.quality_references
                .lock()
                .unwrap()
                .insert(participant_id, reference);
        }

        let audio_rx = self
            .audio_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(AudioReceiveLog::default())));
        let ctx = ClientContext {
            agent: agent.clone(),
            admission: self.admission,
            desired,
            video_capacity: self.video_slots,
            audio_capacity: self.audio_slots,
            auto_subscribe: !self.manual_subscriptions,
            requested_video: None,
            video_operation: 0,
            discovered_tracks: HashSet::new(),
            remote_tracks: HashMap::new(),
            topic_events: Vec::new(),
            media_kinds: HashMap::new(),
            video_rx,
            audio_rx,
            quality_references: self.quality_references.clone(),
            h264_publishers: self.h264_publishers.clone(),
            corrupt_video_payload: self.corrupt_video_payload,
            keyframe_recovery: self.keyframe_recovery.clone(),
            snapshots: {
                let mut snapshots = agent.snapshots();
                snapshots.mark_changed();
                snapshots
            },
            events: agent.events(),
            owned_video,
        };
        let mut client = SimClient {
            ctx,
            join_set,
            actor: Some(actor),
            media_generation: None,
            media: ClientMediaPlan {
                video_slots: self.video_slots,
                audio_slots: self.audio_slots,
                video_receive_mode: self.video_receive_mode,
                audio_receive_mode: self.audio_receive_mode,
                publishes_video: self.publishes_video,
                opaque_payload: self.opaque_payload,
                video_sources,
                vbr_source,
                audio_level_dbov: self.audio_level_dbov,
                audio_phase_offset: self.audio_phase_offset,
                audio_input: self.audio_input,
                corrupt_audio_payload: self.corrupt_audio_payload,
            },
        };
        client.rearm_media().await?;
        client.ctx.refresh().await?;
        Ok(client)
    }
}

/// Actual native frame observations and independent decode/reference facts.
/// Publisher attribution is recorded only for unambiguous fixture content,
/// never inferred from a receiver's current catalog mapping.
#[derive(Default, Debug, Clone)]
pub struct VideoReceiveLog {
    pub native_decoded: super::native_video::NativeDecodedVideoLog,
    pub native_origin: super::native_origin::NativeOriginCorrelation,
    native_origin_enabled: bool,
    native_callback_enabled: bool,
    native_playback_only: bool,
    pub by_publisher: BTreeMap<String, u64>,
    pub decoded_by_publisher: BTreeMap<String, DecodedVideoStream>,
    pub decoded_by_track: BTreeMap<String, u64>,
    /// Successful real decodes keyed by the wire-proven source admission/slot.
    pub decoded_by_incarnation: BTreeMap<String, DecodedVideoStream>,
    /// Native frames without unambiguous content-origin evidence. They cannot
    /// establish absence of any particular publisher.
    pub unattributed_frames: u64,
    pub frames: u64,
    pub keyframes: u64,
    /// Observed queue drops or DD references missing after a decoded entry
    /// point. Not an RTP packet-hole verdict: native video exposes none.
    pub non_contiguous: u64,
    /// Repeated native RTP timestamps within the same receiver/SSRC timeline.
    pub duplicate_ts_frames: u64,
    /// Number of backwards RTP-timestamp jumps seen. Ordinary reordering may
    /// cause one; a broken simulcast switch causes one per botched transition.
    pub ts_regression_count: u64,
    /// Largest backwards jump in RTP time, in 90kHz ticks.
    pub max_ts_regression: u64,
    /// Independent encoded-frame oracle errors, not an SDK decoder-error count.
    pub decoder_errors: u64,
    pub reference_mismatches: u64,
    /// Keyframes whose complete access unit failed to decode.
    pub decoder_error_keyframes: u64,
    /// Keyframes rejected before decode because the access unit had no SPS/PPS.
    pub missing_parameter_set_keyframes: u64,
    pub bytes: u64,
    pub native_queue_drops: u64,
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
    pub by_receiver: BTreeMap<NativeVideoStream, NativeVideoTimeline>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeVideoStream {
    pub generation: u64,
    pub receiver: u64,
    pub ssrc: Option<u32>,
}
impl From<&pulsebeam_agent_native::RemoteFrame> for NativeVideoStream {
    fn from(frame: &pulsebeam_agent_native::RemoteFrame) -> Self {
        Self {
            generation: frame.track.generation,
            receiver: frame.track.receiver,
            ssrc: frame.metadata.ssrc,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct NativeVideoTimeline {
    pub track: Option<pulsebeam_agent_native::RemoteTrack>,
    pub decoded: DecodedVideoStream,
    pub dd_frames: u64,
    pub temporal_indices: BTreeSet<i32>,
    last_ts: Option<u32>,
    seen_ts: HashSet<u32>,
    recent_ts: VecDeque<u32>,
    seen_keyframe: bool,
    seen_dependencies: HashSet<i64>,
    recent_dependencies: VecDeque<i64>,
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

/// Native audio progress or a separately observed speaker rank. Receiver
/// records carry media; catalog rank records do not assert sample provenance.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStream {
    pub packets: u64,
    pub bytes: u64,
    pub decoded_samples: u64,
    pub decoder_errors: u64,
    pub reference_mismatches: u64,
    pub reference_unavailable: u64,
    pub reference_error: ReferenceError,
    /// Longest stretch with no packet, once the stream had started.
    ///
    /// Audio is far less forgiving than video here. A picture that misses 200ms is a stutter
    /// nobody remarks on; a voice that drops 200ms loses a syllable.
    pub longest_gap: Duration,
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
    /// Actual independent decoded progress span for this receiver.
    pub fn decoded_span(&self) -> Duration {
        match (self.decoded_first_at, self.decoded_last_at) {
            (Some(first), Some(last)) => last.saturating_duration_since(first),
            _ => Duration::ZERO,
        }
    }

    pub fn packet_span(&self) -> Duration {
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

/// Actual native receiver observations, without per-sample publication identity.
/// Publisher selection and native decode require separate oracles: a catalog
/// mapping cannot identify the source of each buffered native sample.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct AudioReceiveLog {
    pub native_pcm: super::native_audio::NativePcmLog,
    pub encoded_observers: usize,
    pub by_receiver: BTreeMap<NativeAudioStream, AudioStream>,
    pub tracks: BTreeMap<NativeAudioStream, pulsebeam_agent_native::RemoteTrack>,
    /// Native encoded packet observations, fenced by generation/receiver/SSRC.
    /// Sequence holes can include padding omitted by the SFU, not missing audio.
    /// Native PCM and SDK statistics, not this diagnostic, qualify playout.
    pub by_stream: BTreeMap<NativeAudioStream, AudioStreamContinuity>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeAudioStream {
    pub generation: u64,
    pub receiver: u64,
    pub ssrc: u32,
}

/// Sequence numbers actually exposed by the native encoded observer.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamContinuity {
    pub packets: u64,
    /// Largest observed forward sequence hole. Not a native packet-loss verdict.
    pub max_seq_gap: u64,
    last_seq: Option<u64>,
}

impl AudioReceiveLog {
    fn record(
        &mut self,
        receiver: &NativeAudioStream,
        ssrc: u32,
        seq: u16,
        bytes: usize,
        now: Instant,
    ) {
        self.by_receiver
            .entry(receiver.clone())
            .or_default()
            .record(bytes, now);

        debug_assert_eq!(ssrc, receiver.ssrc);
        let stream = self.by_stream.entry(receiver.clone()).or_default();
        stream.packets = stream.packets.saturating_add(1);
        if let Some(previous) = stream.last_seq {
            let previous_wire = u16::try_from(previous & 0xffff).unwrap();
            let distance = seq.wrapping_sub(previous_wire);
            // Forward modulo-16-bit jumps only; late/reordered packets must not
            // turn a normal wrap into an invented 65k packet loss.
            if distance > 0 && distance < 0x8000 {
                stream.max_seq_gap = stream.max_seq_gap.max(u64::from(distance - 1));
                stream.last_seq = Some(previous + u64::from(distance));
            }
        } else {
            stream.last_seq = Some(u64::from(seq));
        }
    }

    fn record_decoded(
        &mut self,
        receiver: &NativeAudioStream,
        samples: usize,
        reference: Option<ReferenceError>,
    ) {
        self.by_receiver
            .entry(receiver.clone())
            .or_default()
            .record_decoded(samples, Instant::now(), reference);
    }

    fn record_decoder_error(&mut self, receiver: &NativeAudioStream) {
        let stream = self.by_receiver.entry(receiver.clone()).or_default();
        stream.decoder_errors = stream.decoder_errors.saturating_add(1);
    }

    fn record_reference_unavailable(&mut self, receiver: &NativeAudioStream) {
        self.by_receiver
            .entry(receiver.clone())
            .or_default()
            .reference_unavailable += 1;
    }

    fn record_reference_mismatch(&mut self, receiver: &NativeAudioStream) {
        let stream = self.by_receiver.entry(receiver.clone()).or_default();
        stream.reference_mismatches = stream.reference_mismatches.saturating_add(1);
    }
}

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
    fn record_native_decoded(&mut self, frame: &pulsebeam_agent_native::RemoteFrame, at: Instant) {
        let pulsebeam_agent_native::OutputFrame::I420 { .. } = &frame.frame else {
            panic!("native decoded video requires actual I420 output");
        };
        self.native_decoded.observe(frame, at);
        self.unattributed_frames += 1;
        // Count real decoded output for shared progress/timing reports. There is
        // no encoded keyframe classification or corpus-origin proof on this path.
        self.record_arrival(at);
        self.frames = self.frames.saturating_add(1);
    }

    pub fn native_decoded_only(&self) -> bool {
        self.native_playback_only
            || (self.native_decoded.has_observations() && !self.native_origin_enabled)
    }

    fn record_dual_native_decoded(
        &mut self,
        frame: &pulsebeam_agent_native::RemoteFrame,
        now: Instant,
    ) {
        let nontrivial = self.native_decoded.observe(frame, now);
        self.native_origin.decoded(frame, now, nontrivial);
        if self.native_playback_only {
            // Shared progress reports count actual native output, not encoded
            // identity samples or a now-disabled independent test decoder.
            self.record_arrival(now);
            self.frames = self.frames.saturating_add(1);
        }
        // Do not count a second copy of the same picture in encoded quality,
        // independent-decode totals, or unattributed encoded-source evidence.
    }

    pub fn frames_from(&self, publisher: &str) -> u64 {
        self.by_publisher.get(publisher).copied().unwrap_or(0)
    }

    pub fn decoded_from(&self, publisher: &str) -> Option<DecodedVideoStream> {
        self.decoded_by_publisher.get(publisher).copied()
    }

    pub fn require_decoded_from(
        &self,
        publisher: &str,
        baseline_frames: u64,
        min_frames: u64,
    ) -> anyhow::Result<DecodedVideoStream> {
        anyhow::ensure!(min_frames > 0, "decoded delivery needs a positive minimum");
        let decoded = self
            .decoded_from(publisher)
            .ok_or_else(|| anyhow::anyhow!("{publisher}: no independently decoded video"))?;
        let frames = decoded.frames.saturating_sub(baseline_frames);
        anyhow::ensure!(
            frames >= min_frames,
            "{publisher}: decoded {frames} frames since baseline {baseline_frames}, expected at least {min_frames}"
        );
        Ok(decoded)
    }

    pub fn decoded_from_incarnation(&self, origin: &str) -> Option<DecodedVideoStream> {
        self.decoded_by_incarnation.get(origin).copied()
    }

    pub fn require_decoded_from_incarnation(
        &self,
        origin: &str,
        baseline_frames: u64,
        min_frames: u64,
    ) -> anyhow::Result<DecodedVideoStream> {
        anyhow::ensure!(
            min_frames > 0,
            "incarnation delivery needs a positive minimum"
        );
        let decoded = self
            .decoded_from_incarnation(origin)
            .ok_or_else(|| anyhow::anyhow!("{origin}: no independently decoded incarnation"))?;
        let frames = decoded.frames.saturating_sub(baseline_frames);
        anyhow::ensure!(
            frames >= min_frames,
            "{origin}: decoded {frames} incarnation frames since baseline {baseline_frames}, expected at least {min_frames}"
        );
        Ok(decoded)
    }

    pub fn require_native_temporal_indices(
        &self,
        generation: u64,
        min_frames: u64,
        expected: &[i32],
    ) -> anyhow::Result<()> {
        let expected: BTreeSet<_> = expected.iter().copied().collect();
        anyhow::ensure!(
            self.by_receiver.iter().any(|(identity, timeline)| {
                identity.generation == generation
                    && timeline.dd_frames >= min_frames
                    && timeline.decoded.frames >= min_frames
                    && timeline.temporal_indices == expected
            }),
            "generation {generation}: no native receiver has >= {min_frames} DD frames, >= {min_frames} decoded frames and exactly temporal indices {expected:?}; receivers={:?}",
            self.by_receiver
        );
        Ok(())
    }

    pub fn require_not_received_from(&self, publisher: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.native_origin_enabled || !self.native_decoded.has_observations(),
            "{publisher}: bounded positive native-origin receipts cannot prove cumulative native source absence"
        );
        anyhow::ensure!(
            self.unattributed_frames == 0,
            "{publisher}: cannot prove source absence with {} unattributed native frames",
            self.unattributed_frames
        );
        let received = self.frames_from(publisher);
        let decoded = self
            .decoded_from(publisher)
            .map_or(0, |stream| stream.frames);
        anyhow::ensure!(
            received == 0 && decoded == 0,
            "{publisher}: expected no video, received {received} frames and decoded {decoded} frames"
        );
        Ok(())
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

    fn record(
        &mut self,
        publisher: Option<&str>,
        frame: &pulsebeam_agent_native::RemoteFrame,
    ) -> bool {
        let pulsebeam_agent_native::OutputFrame::H264 {
            timestamp,
            keyframe,
            payload,
        } = &frame.frame
        else {
            panic!("encoded video observer returned another media kind");
        };
        if let Some(publisher) = publisher {
            *self.by_publisher.entry(publisher.to_owned()).or_default() += 1;
        } else {
            self.unattributed_frames += 1;
        }
        self.bytes += u64::try_from(payload.len()).unwrap();
        let timeline = self
            .by_receiver
            .entry(NativeVideoStream::from(frame))
            .or_default();
        timeline.track = Some(frame.track.clone());
        const HISTORY: usize = 512;
        if !timeline.seen_ts.insert(*timestamp) {
            self.duplicate_ts_frames += 1;
        } else {
            timeline.recent_ts.push_back(*timestamp);
            if timeline.recent_ts.len() > HISTORY {
                timeline
                    .seen_ts
                    .remove(&timeline.recent_ts.pop_front().unwrap());
            }
        }
        if let Some(previous) = timeline.last_ts {
            let forward = timestamp.wrapping_sub(previous);
            if forward >= 0x8000_0000 {
                let backwards = u64::from(previous.wrapping_sub(*timestamp));
                self.max_ts_regression = self.max_ts_regression.max(backwards);
                if backwards < 90_000 {
                    self.ts_regression_count += 1;
                    if self.ts_regression_count <= 8 {
                        tracing::warn!(track = ?frame.track, ssrc = ?frame.metadata.ssrc, previous, timestamp, backwards, keyframe, metadata = ?frame.metadata, "native encoded observer timestamp regression");
                    }
                }
            }
        }
        timeline.last_ts = Some(*timestamp);
        if *keyframe {
            timeline.seen_keyframe = true;
            timeline.seen_dependencies.clear();
            timeline.recent_dependencies.clear();
        }
        let mut missing_dependency = false;
        if let Some(id) = frame.metadata.frame_id {
            if !frame.metadata.decode_target_indications.is_empty() {
                timeline.dd_frames += 1;
                if let Some(temporal) = frame.metadata.temporal_index {
                    timeline.temporal_indices.insert(temporal);
                }
            }
            missing_dependency = timeline.seen_keyframe
                && frame.metadata.dependencies.iter().any(|dependency| {
                    // Older-than-window references are unavailable, not
                    // evidence that a frame was absent from native output.
                    id.checked_sub(*dependency)
                        .is_some_and(|age| (1..=512).contains(&age))
                        && !timeline.seen_dependencies.contains(dependency)
                });
            if timeline.seen_dependencies.insert(id) {
                timeline.recent_dependencies.push_back(id);
                if timeline.recent_dependencies.len() > HISTORY {
                    timeline
                        .seen_dependencies
                        .remove(&timeline.recent_dependencies.pop_front().unwrap());
                }
            }
        }
        if missing_dependency {
            self.non_contiguous += 1;
        }
        missing_dependency
    }

    fn record_decoded(
        &mut self,
        publisher: Option<&str>,
        width: usize,
        height: usize,
        reference: Option<ReferenceError>,
        is_keyframe: bool,
        quality_identity: Option<(
            pulsebeam_testdata::QualityVideoSource,
            pulsebeam_testdata::QualityVideoLayer,
        )>,
    ) {
        self.record_arrival(Instant::now());
        self.frames = self.frames.saturating_add(1);
        if is_keyframe {
            self.keyframes = self.keyframes.saturating_add(1);
        }
        if let Some(publisher) = publisher {
            self.decoded_by_publisher
                .entry(publisher.to_owned())
                .or_default()
                .record(width, height, reference, quality_identity);
        }
    }

    fn record_arrival(&mut self, now: Instant) {
        if let Some(previous) = self.last_frame_at {
            let gap = now.saturating_duration_since(previous);
            self.longest_frame_gap = self.longest_frame_gap.max(gap);
            if gap > FREEZE_THRESHOLD {
                self.frozen_time = self.frozen_time.saturating_add(gap);
            }
        }
        self.first_frame_at.get_or_insert(now);
        self.last_frame_at = Some(now);
    }

    fn record_decoder_error(&mut self, publisher: Option<&str>, is_keyframe: bool) {
        self.decoder_errors = self.decoder_errors.saturating_add(1);
        if let Some(publisher) = publisher {
            let stream = self
                .decoded_by_publisher
                .entry(publisher.to_owned())
                .or_default();
            stream.decoder_errors = stream.decoder_errors.saturating_add(1);
        }
        if is_keyframe {
            self.decoder_error_keyframes = self.decoder_error_keyframes.saturating_add(1);
        }
    }

    fn record_reference_mismatch(&mut self, publisher: Option<&str>) {
        self.reference_mismatches = self.reference_mismatches.saturating_add(1);
        if let Some(publisher) = publisher {
            let stream = self
                .decoded_by_publisher
                .entry(publisher.to_owned())
                .or_default();
            stream.reference_mismatches = stream.reference_mismatches.saturating_add(1);
        }
    }

    fn record_missing_parameter_set(&mut self, publisher: Option<&str>) {
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
    async fn run(
        self,
        input: AudioInput,
        mut snapshots: tokio::sync::watch::Receiver<pulsebeam_agent_native::agent_core::Snapshot>,
        sender: pulsebeam_agent_native::LocalSource,
    ) {
        while snapshots.borrow().connection != ConnectionState::Connected {
            if snapshots.changed().await.is_err() || sender.is_closed() {
                return;
            }
        }
        assert!(
            (-127..=0).contains(&self.level_dbov),
            "fixture level must be negative dBov"
        );
        if input == AudioInput::Pcm {
            return self.run_pcm(sender).await;
        }
        let corpus = &quality_audio_reference().corpus;
        debug_assert!(!corpus.is_empty());
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        let mut counter = 0u64;
        let started_at = Instant::now();
        loop {
            interval.tick().await;
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
            let frame = pulsebeam_agent_native::OpusFrame {
                audio_level: Some(if speaking {
                    self.level_dbov.unsigned_abs()
                } else {
                    70
                }),
                voice_activity: speaking,
                timestamp: u32::try_from(
                    phase
                        .checked_mul(
                            u64::try_from(pulsebeam_testdata::QUALITY_AUDIO_FRAME_SAMPLES).unwrap(),
                        )
                        .unwrap(),
                )
                .expect("bounded fixture RTP timestamp"),
                samples_per_channel: u32::try_from(pulsebeam_testdata::QUALITY_AUDIO_FRAME_SAMPLES)
                    .unwrap(),
                payload: corpus_frame.opus_packet.to_vec(),
            };
            if sender.is_closed() {
                return;
            }
            sender
                .submit(InputFrame::Opus { frame })
                .await
                .unwrap_or_else(|error| {
                    let snapshot = snapshots.borrow();
                    panic!(
                        "native Opus fixture submission: packet={counter} elapsed={:?} generation={:?} connection={:?}: {error:?}",
                        started_at.elapsed(), snapshot.generation, snapshot.connection,
                    )
                });
            counter = counter.saturating_add(1);
        }
    }
}

impl QualityAudioLooper {
    async fn run_pcm(self, sender: pulsebeam_agent_native::LocalSource) {
        let prepared = quality_audio_reference();
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let target_rms = 32768.0 * 10_f64.powf(f64::from(self.level_dbov) / 20.0);
        let mut counter = 0_u64;
        loop {
            interval.tick().await;
            if sender.is_closed() {
                return;
            }
            let frame_index =
                usize::try_from(counter / 2 + self.phase_offset).unwrap() % prepared.corpus.len();
            let reference = prepared
                .corpus
                .reference_frame(&prepared.reference, frame_index)
                .expect("PCM corpus reference");
            // The reference is signed little-endian PCM, not one sample per byte.
            let reference = reference.as_chunks::<2>().0;
            let offset = usize::try_from(counter % 2).unwrap() * 480;
            let reference = &reference[offset..offset + 480];
            let active = matches!(
                prepared.corpus.frame(frame_index).unwrap().region,
                pulsebeam_testdata::QualityAudioRegion::Active
            );
            let rms = (reference
                .iter()
                .map(|bytes| f64::from(i16::from_le_bytes(*bytes)).powi(2))
                .sum::<f64>()
                / 480.0)
                .sqrt();
            let gain = if active && rms > 0.0 {
                target_rms / rms
            } else {
                0.0
            };
            let samples = reference
                .iter()
                .map(|bytes| {
                    let sample = i16::from_le_bytes(*bytes);
                    // Intentional integer PCM quantization after finite gain, rounding and clipping.
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "rounded and clipped to the i16 PCM range"
                    )]
                    let sample = (f64::from(sample) * gain)
                        .round()
                        .clamp(f64::from(i16::MIN), f64::from(i16::MAX))
                        as i16;
                    sample
                })
                .collect();
            let timestamp_us = sender
                .capture_time_us()
                .await
                .expect("native PCM capture clock");
            sender
                .submit(InputFrame::Pcm {
                    frame: pulsebeam_agent_native::PcmFrame {
                        sample_rate_hz: 48_000,
                        channels: 1,
                        samples,
                        timestamp_us,
                    },
                })
                .await
                .expect("native PCM fixture admission");
            counter += 1;
        }
    }
}

struct PreparedQualityAudio {
    corpus: pulsebeam_testdata::QualityCorpusAudio,
    reference: Vec<u8>,
    encoded_frames: BTreeMap<Vec<u8>, usize>,
}

fn quality_audio_reference() -> &'static PreparedQualityAudio {
    static REFERENCE: OnceLock<PreparedQualityAudio> = OnceLock::new();
    REFERENCE.get_or_init(|| {
        let corpus =
            pulsebeam_testdata::quality_corpus_audio(pulsebeam_testdata::QualityAudioSource::Zero);
        let reference = corpus
            .decode_reference()
            .unwrap_or_else(|error| panic!("quality Opus reference failed: {error}"));
        let mut encoded_frames = BTreeMap::new();
        for index in 0..corpus.len() {
            let frame = corpus.frame(index).expect("quality Opus corpus frame");
            encoded_frames
                .entry(frame.opus_packet.to_vec())
                .or_insert(index);
        }
        PreparedQualityAudio {
            corpus,
            reference,
            encoded_frames,
        }
    })
}

struct OpusReceiver {
    decoder: OpusReferenceDecoder,
    prepared: &'static PreparedQualityAudio,
}

fn matching_video_inputs(
    payload: &[u8],
    references: &BTreeMap<String, &'static QualityVideoReference>,
) -> Vec<(String, &'static QualityVideoReference, usize)> {
    let canonical = canonical_annex_b(payload);
    references
        .iter()
        .filter_map(|(publisher, entry)| {
            entry
                .encoded_frames
                .get(&canonical)
                .map(|index| (publisher.clone(), *entry, *index))
        })
        .collect()
}

#[cfg(test)]
fn record_video_frame(
    log: &Arc<Mutex<VideoReceiveLog>>,
    quality_references: &Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    decoder: &mut H264ReferenceDecoder,
    decoder_ready: &mut bool,
    retained_publisher: &mut Option<String>,
    frame: pulsebeam_agent_native::RemoteFrame,
    corrupt_payload: bool,
) {
    record_fixture_video_frame(
        log,
        FixtureOracle {
            references: quality_references,
            publishers: None,
        },
        decoder,
        decoder_ready,
        retained_publisher,
        frame,
        corrupt_payload,
    );
}

struct FixtureOracle<'a> {
    references: &'a Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    publishers: Option<&'a FixturePublishers>,
}

fn authenticated_fixture_origin(payload: &[u8], publishers: &FixturePublishers) -> Option<String> {
    let (origin, bare) = provenance::inspect(payload).ok()?;
    let origin = origin?;
    publishers
        .get(&origin)?
        .frames
        .contains(&bare)
        .then_some(origin)
}

fn record_native_fixture_sample(
    log: &Arc<Mutex<VideoReceiveLog>>,
    publishers: &FixturePublishers,
    frame: &pulsebeam_agent_native::RemoteFrame,
) {
    let pulsebeam_agent_native::OutputFrame::H264 { payload, .. } = &frame.frame else {
        panic!("native source sample requires an encoded access unit");
    };
    let origin = authenticated_fixture_origin(payload, publishers);
    let publisher = origin
        .as_ref()
        .and_then(|origin| publishers.get(origin))
        .map(|source| source.participant_id.as_str());
    let mut observed = log.lock().unwrap();
    observed.native_origin.encoded(frame, origin.as_deref());
    // Retain observed-slot/encoded diagnostics, but neither independently decode
    // nor turn polling loss into an application-generated keyframe request.
    observed.record(publisher, frame);
}

fn record_fixture_video_frame(
    log: &Arc<Mutex<VideoReceiveLog>>,
    oracle: FixtureOracle<'_>,
    decoder: &mut H264ReferenceDecoder,
    decoder_ready: &mut bool,
    retained_publisher: &mut Option<String>,
    frame: pulsebeam_agent_native::RemoteFrame,
    corrupt_payload: bool,
) {
    let pulsebeam_agent_native::OutputFrame::H264 {
        keyframe, payload, ..
    } = &frame.frame
    else {
        panic!("encoded video observer returned another media kind");
    };
    let inspected = provenance::inspect(payload);
    let marker = inspected
        .as_ref()
        .ok()
        .and_then(|(identity, _)| identity.as_deref());
    let admitted = marker.and_then(|source| oracle.publishers?.get(source));
    let authenticated = admitted.filter(|source| {
        inspected
            .as_ref()
            .is_ok_and(|(_, bare)| source.frames.contains(bare))
    });
    let authenticated_source = authenticated.map(|source| source.participant_id.as_str());
    let mut matches = matching_video_inputs(payload, &oracle.references.lock().unwrap());
    if oracle.publishers.is_some() || marker.is_some() || inspected.is_err() {
        matches.retain(|(source, _, _)| authenticated_source == Some(source.as_str()));
        // Every runtime fixture AU must prove its own origin. Even a familiar
        // marker cannot authenticate unknown picture bytes or an unmarked AU.
        *retained_publisher = authenticated_source.map(str::to_owned);
    } else if matches.len() == 1 {
        *retained_publisher = Some(matches[0].0.clone());
    }
    // Pure unmarked oracle regressions retain only a previously observed,
    // unique content identity. Runtime fixture attribution above is stricter:
    // every AU needs both its actual wire marker and known source bytes.
    // Neither path infers an origin from a catalog or an unmatched first frame.
    let publisher = matches
        .first()
        .filter(|_| matches.len() == 1)
        .map(|value| value.0.as_str())
        .or(retained_publisher.as_deref());
    let missing_dependency = {
        let mut observed = log.lock().unwrap();
        if observed.native_origin_enabled {
            observed
                .native_origin
                .encoded(&frame, marker.filter(|_| authenticated.is_some()));
        }
        observed.record(publisher, &frame)
    };
    if missing_dependency {
        *decoder_ready = false;
    }
    let is_keyframe = *keyframe || annex_b_has_nal_type(payload, 5);
    if !is_keyframe && !*decoder_ready {
        return;
    }
    let has_parameter_sets = annex_b_has_nal_type(payload, 7) && annex_b_has_nal_type(payload, 8);
    if is_keyframe && !*decoder_ready && !corrupt_payload && !has_parameter_sets {
        log.lock().unwrap().record_missing_parameter_set(publisher);
        return;
    }
    // Repeated SPS/PPS on an intact stream do not require rebuilding its decoder.
    if is_keyframe && has_parameter_sets && !*decoder_ready {
        decoder.reset();
    }
    let reference = matches.first().and_then(|(_, entry, index)| {
        let quality = entry.quality.as_ref()?;
        quality
            .corpus
            .reference_frame(&quality.decoded, *index)
            .map(|reference| (reference, quality.source, quality.layer))
    });
    let corrupt = [0, 0, 0, 1, 0x65, 0xff];
    let decode_payload = if corrupt_payload {
        corrupt.as_slice()
    } else {
        payload.as_slice()
    };
    let decode_result = decoder.try_decode_observation(
        decode_payload,
        reference.as_ref().map(|(reference, _, _)| *reference),
    );
    let decoded_ok = decode_result.is_ok();
    tracing::trace!(target: "pulsebeam_fixture_video", metadata = ?frame.metadata, publisher = ?publisher, wire_origin = ?marker, authenticated_source = ?authenticated_source, corpus_indices = ?matches.iter().map(|(publisher, _, index)| (publisher, index)).collect::<Vec<_>>(), is_keyframe, decoded_ok, "fixture receive");
    match decode_result {
        Ok(observation) => {
            let mut log = log.lock().unwrap();
            let identity = reference.map(|(_, source, layer)| (source, layer));
            log.by_receiver
                .entry(NativeVideoStream::from(&frame))
                .or_default()
                .decoded
                .record(
                    observation.width,
                    observation.height,
                    observation.reference_error,
                    identity,
                );
            log.record_decoded(
                publisher,
                observation.width,
                observation.height,
                observation.reference_error,
                is_keyframe,
                identity,
            );
            *log.decoded_by_track.entry(frame.track.track_id.clone()).or_default() += 1;
            if let Some(origin) = marker
                && authenticated.is_some()
            {
                log.decoded_by_incarnation
                    .entry(origin.to_owned())
                    .or_default()
                    .record(
                        observation.width,
                        observation.height,
                        observation.reference_error,
                        identity,
                    );
            }
        }
        Err(DecodeError::ReferenceMismatch(_)) => {
            let mut log = log.lock().unwrap();
            log.record_reference_mismatch(publisher);
            log.record_decoder_error(publisher, is_keyframe);
        }
        Err(DecodeError::Decoder(error)) => {
            let mut log = log.lock().unwrap();
            if log.decoder_errors < 8 {
                tracing::warn!(track = ?frame.track, metadata = ?frame.metadata, is_keyframe, ?error, "independent decoder rejected a native owned frame");
            }
            log.record_decoder_error(publisher, is_keyframe);
        }
    }
    if is_keyframe && decoded_ok {
        *decoder_ready = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic owned values for oracle checks only, never injected as RTC input.
    fn owned_video(
        receiver: u64,
        timestamp: u32,
        keyframe: bool,
        payload: Vec<u8>,
    ) -> pulsebeam_agent_native::RemoteFrame {
        pulsebeam_agent_native::RemoteFrame {
            track: pulsebeam_agent_native::RemoteTrack {
                generation: 1,
                receiver,
                track_id: "unit-native-track".into(),
                mid: "v0".into(),
                media_index: 0,
            },
            metadata: pulsebeam_agent_native::FrameMetadata::default(),
            frame: pulsebeam_agent_native::OutputFrame::H264 {
                timestamp,
                keyframe,
                payload,
            },
        }
    }

    #[test]
    fn distinct_180p_inputs_prove_origin_and_duplicate_inputs_stay_ambiguous() {
        use pulsebeam_testdata::{QualityVideoLayer::P180, QualityVideoSource::*};
        let quarter = generic_video_reference(&[Some("q".into())]);
        let references = BTreeMap::from([
            ("ann".into(), quality_video_reference(Zero, P180)),
            ("ben".into(), quality_video_reference(One, P180)),
            ("cal".into(), quarter),
        ]);
        assert!(quarter.quality.is_none());
        assert_eq!(create_video_source(Some("q")).dimensions(), (320, 180));
        for (publisher, reference) in &references {
            for payload in reference.frames.iter() {
                let matches = matching_video_inputs(payload, &references);
                assert_eq!(matches.len(), 1, "ambiguous {publisher} input");
                assert_eq!(&matches[0].0, publisher);
            }
        }
        let duplicates = BTreeMap::from([("first".into(), quarter), ("second".into(), quarter)]);
        assert_eq!(
            matching_video_inputs(&quarter.frames[0], &duplicates).len(),
            2
        );
        assert!(matching_video_inputs(&[0, 0, 0, 1, 0x65, 0xff], &references).is_empty());

        // The real observer path must count only unique content, even if the
        // native track happens to carry the name of a different publisher.
        let references = Arc::new(Mutex::new(references));
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        for (index, publisher) in ["ann", "ben", "cal"].into_iter().enumerate() {
            let payload = references.lock().unwrap()[publisher].frames[0].to_vec();
            let mut frame = owned_video(u64::try_from(index).unwrap(), 90_000, true, payload);
            frame.track.track_id = "foreign-catalog-name".into();
            let mut decoder = H264ReferenceDecoder::new(publisher, "unit input");
            record_video_frame(
                &log,
                &references,
                &mut decoder,
                &mut false,
                &mut None,
                frame,
                false,
            );
            assert_eq!(log.lock().unwrap().frames_from(publisher), 1);
        }
        let duplicates = Arc::new(Mutex::new(duplicates));
        let mut decoder = H264ReferenceDecoder::new("ambiguous", "unit input");
        record_video_frame(
            &log,
            &duplicates,
            &mut decoder,
            &mut false,
            &mut None,
            owned_video(10, 90_000, true, quarter.frames[0].to_vec()),
            false,
        );
        let observed = log.lock().unwrap();
        assert_eq!(observed.frames_from("first"), 0);
        assert_eq!(observed.frames_from("second"), 0);
        assert_eq!(observed.unattributed_frames, 1);
    }

    #[test]
    fn decoder_errors_retain_only_payload_authenticated_stream_identity() {
        use pulsebeam_testdata::{QualityVideoLayer::P180, QualityVideoSource::Zero};
        let reference = quality_video_reference(Zero, P180);
        let references = Arc::new(Mutex::new(BTreeMap::from([("source".into(), reference)])));
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let mut decoder = H264ReferenceDecoder::new("source", "unit input");
        let mut decoder_ready = false;
        let mut retained_publisher = None;
        record_video_frame(
            &log,
            &references,
            &mut decoder,
            &mut decoder_ready,
            &mut retained_publisher,
            owned_video(1, 90_000, true, reference.frames[0].to_vec()),
            false,
        );
        assert_eq!(retained_publisher.as_deref(), Some("source"));

        record_video_frame(
            &log,
            &references,
            &mut decoder,
            &mut decoder_ready,
            &mut retained_publisher,
            owned_video(1, 93_000, true, vec![0, 0, 0, 1, 0x65, 0xff]),
            true,
        );
        let observed = log.lock().unwrap();
        assert_eq!(observed.frames_from("source"), 2);
        assert!(observed.decoded_from("source").unwrap().decoder_errors > 0);
    }

    #[test]
    fn distinct_quality_ladders_register_real_inputs_without_catalog_attribution() {
        use pulsebeam_testdata::{QualityVideoLayer::*, QualityVideoSource::*};
        let references = BTreeMap::from([
            ("camera".into(), quality_ladder_reference(Zero)),
            ("protected".into(), quality_ladder_reference(One)),
        ]);
        for (publisher, source) in [("camera", Zero), ("protected", One)] {
            assert!(references[publisher].quality.is_none());
            for (layer, dimensions) in [(P180, (320, 180)), (P360, (640, 360)), (P720, (1280, 720))]
            {
                assert_eq!(
                    create_quality_video_source(source, layer).dimensions(),
                    dimensions
                );
                for payload in quality_encoded_frames(source, layer).iter() {
                    let matches = matching_video_inputs(payload, &references);
                    assert_eq!(matches.len(), 1, "ambiguous {publisher}/{layer:?} input");
                    assert_eq!(matches[0].0, publisher);
                }
            }
        }
        let duplicates = BTreeMap::from([
            ("first".into(), quality_ladder_reference(Zero)),
            ("second".into(), quality_ladder_reference(Zero)),
        ]);
        let payload = quality_encoded_frames(Zero, P720)[0].to_vec();
        assert_eq!(matching_video_inputs(&payload, &duplicates).len(), 2);
        assert!(matching_video_inputs(&[0, 0, 0, 1, 0x65, 0xff], &references).is_empty());
        let references = Arc::new(Mutex::new(references));
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let mut frame = owned_video(1, 90_000, true, payload);
        frame.track.track_id = "protected".into();
        record_video_frame(
            &log,
            &references,
            &mut H264ReferenceDecoder::new("camera", "unit input"),
            &mut false,
            &mut None,
            frame,
            false,
        );
        let observed = log.lock().unwrap();
        assert_eq!(observed.frames_from("camera"), 1);
        assert_eq!(observed.frames_from("protected"), 0);
    }

    #[test]
    fn native_playback_progress_counts_actual_native_callbacks() {
        let mut log = VideoReceiveLog {
            native_playback_only: true,
            ..VideoReceiveLog::default()
        };
        let mut frame = owned_video(1, 90_000, true, vec![]);
        frame.metadata.rtp_timestamp = Some(90_000);
        frame.frame = pulsebeam_agent_native::OutputFrame::I420 {
            frame: pulsebeam_agent_native::I420Frame {
                width: 4,
                height: 4,
                timestamp_us: 0,
                data: [16, 200].repeat(12),
                rotation_degrees: 0,
            },
        };
        log.record_dual_native_decoded(&frame, Instant::now());
        assert_eq!(log.frames, 1);
        assert!(log.first_frame_at.is_some());
        assert!(log.decoded_by_publisher.is_empty());
    }

    #[test]
    fn wire_markers_disambiguate_identical_sources_without_receiver_or_catalog_hints() {
        let input = generic_video_reference(&[None]);
        let references = Arc::new(Mutex::new(BTreeMap::from([
            ("left".into(), input),
            ("right".into(), input),
        ])));
        let codebook: BTreeSet<_> = input
            .frames
            .iter()
            .map(|frame| canonical_annex_b(frame))
            .collect();
        let publishers = BTreeMap::from([
            (
                "left".into(),
                FixturePublisher {
                    participant_id: "left".into(),
                    frames: codebook.clone(),
                },
            ),
            (
                "right".into(),
                FixturePublisher {
                    participant_id: "right".into(),
                    frames: codebook,
                },
            ),
        ]);
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let mut decoder = H264ReferenceDecoder::new("wire", "fixture");
        let mut ready = false;
        let mut retained = None;
        // Keep the exact same native receiver and SSRC while changing sources.
        // A familiar marker with unknown bytes, and an unmarked AU after a
        // valid source, must both lose attribution rather than inherit it.
        let unmatched = generic_video_reference(&[Some("f".into())]);
        let cases = [
            (Some("left"), &input.frames[0], Some("left")),
            (Some("left"), &unmatched.frames[0], None),
            (Some("right"), &input.frames[0], Some("right")),
            (None, &input.frames[0], None),
            (Some("right"), &input.frames[0], Some("right")),
            (Some("unregistered"), &input.frames[0], None),
        ];
        let native_log = Arc::new(Mutex::new(VideoReceiveLog {
            native_playback_only: true,
            native_origin_enabled: true,
            ..VideoReceiveLog::default()
        }));
        for (index, (publisher, payload, expected)) in cases.into_iter().enumerate() {
            let payload = publisher.map_or_else(
                || payload.to_vec(),
                |source| provenance::stamp(payload, source),
            );
            assert_eq!(
                authenticated_fixture_origin(&payload, &publishers).as_deref(),
                expected
            );
            let frame = owned_video(1, u32::try_from(index).unwrap() * 90_000, true, payload);
            record_native_fixture_sample(&native_log, &publishers, &frame);
            record_fixture_video_frame(
                &log,
                FixtureOracle {
                    references: &references,
                    publishers: Some(&publishers),
                },
                &mut decoder,
                &mut ready,
                &mut retained,
                frame,
                false,
            );
            assert_eq!(retained.as_deref(), expected);
        }
        let native_log = native_log.lock().unwrap();
        assert_eq!(
            native_log.frames, 0,
            "encoded samples are not decoded output"
        );
        assert_eq!(native_log.frames_from("left"), 1);
        assert_eq!(native_log.frames_from("right"), 2);
        assert_eq!(native_log.unattributed_frames, 3);
        assert!(
            native_log.decoded_by_publisher.is_empty(),
            "source sampling does not independently decode"
        );
        assert!(
            native_log.native_decoded_only(),
            "encoded quality assertions cannot use source-only samples"
        );
        let log = log.lock().unwrap();
        assert_eq!(log.unattributed_frames, 3);
        assert_eq!(log.decoded_from("left").unwrap().frames, 1);
        assert_eq!(log.decoded_from("right").unwrap().frames, 2);
        assert!(log.decoded_from("unregistered").is_none());
        assert!(
            retained.is_none(),
            "unknown markers must clear retained attribution"
        );
    }

    #[test]
    fn old_admission_decodes_cannot_satisfy_readmitted_source_progress() {
        let input = generic_video_reference(&[None]);
        let references = Arc::new(Mutex::new(BTreeMap::from([("source".into(), input)])));
        let old = provenance::source_key("old-admission", "v0");
        let new = provenance::source_key("new-admission", "v0");
        let frames: BTreeSet<_> = input
            .frames
            .iter()
            .map(|frame| canonical_annex_b(frame))
            .collect();
        let publishers = BTreeMap::from([
            (
                old.clone(),
                FixturePublisher {
                    participant_id: "source".into(),
                    frames: frames.clone(),
                },
            ),
            (
                new.clone(),
                FixturePublisher {
                    participant_id: "source".into(),
                    frames,
                },
            ),
        ]);
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let mut decoder = H264ReferenceDecoder::new("wire", "admission fixture");
        let mut ready = false;
        let mut retained = None;
        let mut feed = |origin: &str, timestamp| {
            record_fixture_video_frame(
                &log,
                FixtureOracle {
                    references: &references,
                    publishers: Some(&publishers),
                },
                &mut decoder,
                &mut ready,
                &mut retained,
                owned_video(
                    1,
                    timestamp,
                    true,
                    provenance::stamp(&input.frames[0], origin),
                ),
                false,
            );
        };
        feed(&old, 90_000);
        let baseline = log.lock().unwrap().decoded_from("source").unwrap().frames;
        feed(&old, 180_000);
        assert!(
            log.lock()
                .unwrap()
                .require_decoded_from("source", baseline, 1)
                .is_ok(),
            "the old logical-publisher-only witness would accept buffered old media"
        );
        assert!(
            log.lock()
                .unwrap()
                .require_decoded_from_incarnation(&new, 0, 1)
                .is_err()
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .decoded_from_incarnation(&old)
                .unwrap()
                .frames,
            2
        );
        feed(&new, 270_000);
        assert!(
            log.lock()
                .unwrap()
                .require_decoded_from_incarnation(&new, 0, 1)
                .is_ok()
        );
        assert!(
            log.lock()
                .unwrap()
                .require_decoded_from_incarnation(&new, 1, 1)
                .is_err(),
            "a stale sample of the correct incarnation is not fresh progress"
        );
    }

    #[test]
    fn wire_identity_requires_a_registered_codebook_and_still_requires_real_decode() {
        let input = generic_video_reference(&[None]);
        let references = Arc::new(Mutex::new(BTreeMap::from([("source".into(), input)])));
        let publishers = BTreeMap::from([(
            "source".into(),
            FixturePublisher {
                participant_id: "source".into(),
                frames: input
                    .frames
                    .iter()
                    .map(|frame| canonical_annex_b(frame))
                    .collect(),
            },
        )]);
        for (payload, corrupt) in [
            (
                generic_video_reference(&[Some("f".into())]).frames[0].clone(),
                false,
            ),
            (input.frames[0].clone(), true),
        ] {
            let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
            record_fixture_video_frame(
                &log,
                FixtureOracle {
                    references: &references,
                    publishers: Some(&publishers),
                },
                &mut H264ReferenceDecoder::new("wire", "fixture"),
                &mut false,
                &mut None,
                owned_video(1, 90_000, true, provenance::stamp(&payload, "source")),
                corrupt,
            );
            let log = log.lock().unwrap();
            assert_eq!(
                log.decoded_from("source")
                    .map_or(0, |decoded| decoded.frames),
                0,
                "neither an unmatched codebook nor an undecodable payload is successful media"
            );
            if corrupt {
                assert_eq!(log.decoder_errors, 1);
            } else {
                assert_eq!(log.frames_from("source"), 0);
            }
        }
    }

    #[test]
    fn fairness_fixture_offered_payload_exceeds_the_300kbps_link() {
        use pulsebeam_testdata::{QualityVideoLayer::P180, QualityVideoSource::*};
        let inputs = [
            quality_video_reference(Zero, P180),
            quality_video_reference(One, P180),
            generic_video_reference(&[Some("q".into())]),
        ];
        let offered_bps: u64 = inputs
            .iter()
            .map(|input| {
                let bytes: u64 = input
                    .frames
                    .iter()
                    .map(|frame| u64::try_from(frame.len()).unwrap())
                    .sum();
                bytes * 8 * 30 / u64::try_from(input.frames.len()).unwrap()
            })
            .sum();
        assert!(
            offered_bps > 300_000,
            "not a scarce link: offered={offered_bps}bps"
        );
    }

    #[test]
    fn native_decoded_progress_is_reported_without_publisher_or_codec_error_claims() {
        let mut frame = owned_video(1, 1, false, vec![]);
        frame.metadata.rtp_timestamp = Some(1);
        frame.frame = pulsebeam_agent_native::OutputFrame::I420 {
            frame: pulsebeam_agent_native::I420Frame {
                width: 2,
                height: 2,
                timestamp_us: 1000,
                data: vec![16, 200, 16, 200, 128, 128],
                rotation_degrees: 0,
            },
        };
        let mut log = VideoReceiveLog::default();
        log.record_native_decoded(&frame, Instant::now());
        assert_eq!(log.stats().frames, 1);
        assert!(log.stats().first_frame_at.is_some());
        assert!(log.native_decoded.has_observations());
        assert!(log.by_publisher.is_empty());
        assert!(log.require_not_received_from("foreign").is_err());
        let mut dual = VideoReceiveLog {
            native_origin_enabled: true,
            ..VideoReceiveLog::default()
        };
        dual.record_dual_native_decoded(&frame, Instant::now());
        assert_eq!(
            dual.stats().frames,
            0,
            "I420 must not inflate encoded metrics"
        );
        assert_eq!(
            dual.unattributed_frames, 0,
            "raw observations stay separate"
        );
        assert!(
            dual.require_not_received_from("foreign").is_err(),
            "positive native receipts are not an absence oracle"
        );
    }

    #[test]
    fn native_timestamp_wrap_and_independent_receivers_do_not_regress() {
        let mut log = VideoReceiveLog::default();
        for frame in [
            owned_video(1, u32::MAX - 2, true, vec![]),
            owned_video(2, 50, true, vec![]),
            owned_video(1, 10, false, vec![]),
        ] {
            assert!(!log.record(None, &frame));
        }
        assert_eq!(log.ts_regression_count, 0);
        assert_eq!(log.max_ts_regression, 0);
        assert_eq!(log.duplicate_ts_frames, 0);
        log.record(None, &owned_video(1, 9, false, vec![]));
        assert_eq!(log.ts_regression_count, 1);
        assert_eq!(log.max_ts_regression, 1);
        assert!(log.by_publisher.is_empty());
    }

    #[test]
    fn late_feedback_ack_does_not_restore_a_recovered_decoder_throttle() {
        let now = tokio::time::Instant::now();
        for decoder_ready in [false, true] {
            let mut stream = VideoStreamReceiver {
                decoder: H264ReferenceDecoder::new("feedback", "state-only unit"),
                decoder_ready,
                retained_publisher: None,
                last_keyframe_request: None,
            };
            stream.acknowledge_keyframe(now);
            assert_eq!(
                stream.last_keyframe_request,
                (!decoder_ready).then_some(now)
            );
        }
    }

    #[test]
    fn source_absence_requires_no_unattributed_native_video() {
        let mut log = VideoReceiveLog::default();
        assert!(log.require_not_received_from("foreign").is_ok());
        log.record(None, &owned_video(1, 90_000, true, vec![]));
        assert_eq!(log.unattributed_frames, 1);
        assert!(log.by_publisher.is_empty());
        assert!(log.require_not_received_from("foreign").is_err());
    }

    #[test]
    fn native_dd_reference_gaps_and_metadata_absence_are_distinct() {
        let mut log = VideoReceiveLog::default();
        let mut frame = owned_video(1, 0, true, vec![]);
        frame.metadata.frame_id = Some(10);
        frame.metadata.temporal_index = Some(0);
        frame.metadata.decode_target_indications =
            vec![pulsebeam_agent_native::DecodeTargetIndication::Required];
        assert!(!log.record(None, &frame));
        frame.frame = pulsebeam_agent_native::OutputFrame::H264 {
            timestamp: 3_000,
            keyframe: false,
            payload: vec![],
        };
        frame.metadata.frame_id = Some(12);
        frame.metadata.dependencies = vec![10];
        frame.metadata.temporal_index = Some(2);
        assert!(!log.record(None, &frame));
        frame.metadata.frame_id = Some(13);
        frame.metadata.dependencies = vec![11];
        assert!(log.record(None, &frame));
        let mut undescribed = owned_video(2, 0, true, vec![]);
        undescribed.metadata.temporal_index = Some(0);
        assert!(!log.record(None, &undescribed));
        let unknown = &log.by_receiver[&NativeVideoStream::from(&undescribed)];
        assert_eq!(unknown.dd_frames, 0);
        assert!(unknown.temporal_indices.is_empty());
        assert_eq!(log.non_contiguous, 1);
        assert!(log.by_publisher.is_empty());
    }

    #[test]
    fn temporal_witness_requires_real_dd_decode_indices_and_generation() {
        let mut log = VideoReceiveLog::default();
        let frame = owned_video(1, 0, true, vec![]);
        let identity = NativeVideoStream::from(&frame);
        let generation = identity.generation;
        let timeline = log.by_receiver.entry(identity.clone()).or_default();
        timeline.dd_frames = 10;
        timeline.decoded.frames = 10;
        timeline.temporal_indices.insert(0);
        assert!(
            log.require_native_temporal_indices(generation, 10, &[0, 1, 2])
                .is_err()
        );
        log.by_receiver
            .get_mut(&identity)
            .unwrap()
            .temporal_indices
            .extend([1, 2]);
        assert!(
            log.require_native_temporal_indices(generation, 10, &[0, 1, 2])
                .is_ok()
        );
        assert!(
            log.require_native_temporal_indices(generation + 1, 10, &[0, 1, 2])
                .is_err()
        );
        log.by_receiver.get_mut(&identity).unwrap().dd_frames = 0;
        assert!(
            log.require_native_temporal_indices(generation, 10, &[0, 1, 2])
                .is_err()
        );
        log.by_receiver.get_mut(&identity).unwrap().dd_frames = 10;
        log.by_receiver.get_mut(&identity).unwrap().decoded.frames = 0;
        assert!(
            log.require_native_temporal_indices(generation, 10, &[0, 1, 2])
                .is_err()
        );
    }

    #[test]
    fn native_traffic_projection_keeps_absent_fields_unavailable() {
        let report = pulsebeam_agent_native::NativeStatistics {
            generation: 7,
            timestamp_us: 123,
            report_json: br#"[{"type":"transport","bytesSent":0,"bytesReceived":42},
                {"type":"outbound-rtp","kind":"video","pliCount":3}]"#
                .to_vec(),
        };
        let stats = NativeTrafficStatistics::from_report(&report).unwrap();
        assert_eq!((stats.generation, stats.timestamp_us), (7, 123));
        assert_eq!(stats.bytes_sent, Some(0));
        assert_eq!(stats.bytes_received, Some(42));
        assert_eq!(stats.keyframe_requests, Some(3));
        let empty =
            NativeTrafficStatistics::from_report(&pulsebeam_agent_native::NativeStatistics {
                report_json: b"[]".to_vec(),
                ..report
            })
            .unwrap();
        assert_eq!(empty.bytes_sent, None);
        assert_eq!(empty.bytes_received, None);
        assert_eq!(empty.keyframe_requests, None);
    }

    #[test]
    fn native_video_witness_needs_old_source_frames_not_transport_padding_or_new_ssrc() {
        // Statistics projection only, never synthesized as RTC input.
        let baseline = NativeTrafficStatistics::from_report(&pulsebeam_agent_native::NativeStatistics {
            generation: 7, timestamp_us: 10,
            report_json: br#"[{"type":"media-source","id":"SV1","kind":"video","trackIdentifier":"v0"},
                {"type":"outbound-rtp","kind":"video","ssrc":10,"mediaSourceId":"SV1","bytesSent":100,"packetsSent":5,"framesSent":1},
                {"type":"transport","bytesSent":1000}]"#.to_vec(),
        }).unwrap();
        assert!(baseline.has_video_sender("v0"));
        assert!(!baseline.has_video_sender("other"));
        let mut current = baseline.clone();
        current.timestamp_us = 20;
        current.bytes_sent = Some(50_000);
        assert_eq!(current.video_progress_since(&baseline, "v0"), Ok(false));
        current.outbound_video.get_mut(&10).unwrap().bytes_sent = Some(200);
        current.outbound_video.get_mut(&10).unwrap().packets_sent = Some(10);
        assert_eq!(current.video_progress_since(&baseline, "v0"), Ok(false));
        current.outbound_video.get_mut(&10).unwrap().frames_sent = Some(2);
        assert_eq!(current.video_progress_since(&baseline, "v0"), Ok(true));
        let mut wrong = current.clone();
        wrong.timestamp_us = 10;
        assert!(wrong.video_progress_since(&baseline, "v0").is_err());
        wrong = current.clone();
        wrong.generation = 8;
        assert!(wrong.video_progress_since(&baseline, "v0").is_err());
        wrong = current.clone();
        wrong.outbound_video.get_mut(&10).unwrap().slot = Some("other".into());
        assert!(wrong.video_progress_since(&baseline, "v0").is_err());
        wrong = current.clone();
        wrong.outbound_video.get_mut(&10).unwrap().bytes_sent = None;
        assert!(wrong.video_progress_since(&baseline, "v0").is_err());
        wrong = current.clone();
        let replacement = wrong.outbound_video.remove(&10).unwrap();
        wrong.outbound_video.insert(11, replacement);
        assert!(wrong.video_progress_since(&baseline, "v0").is_err());
    }

    #[test]
    fn initial_native_baseline_uses_only_a_real_in_window_same_generation_counter() {
        let now = Instant::now();
        let sample = NativeTrafficStatistics {
            generation: 1,
            timestamp_us: 10,
            bytes_sent: Some(100),
            bytes_received: Some(200),
            keyframe_requests: None,
            outbound_video: BTreeMap::new(),
            report_json: vec![],
        };
        let mut baseline = NativeTrafficBaseline::default();
        baseline.begin(None, Some(1), now + Duration::from_millis(10));
        baseline.observe(&sample, now);
        assert!(
            baseline.sample().is_none(),
            "pre-window report is inadmissible"
        );
        let at = now + Duration::from_millis(20);
        baseline.observe(
            &NativeTrafficStatistics {
                generation: 2,
                ..sample.clone()
            },
            at,
        );
        baseline.observe(
            &NativeTrafficStatistics {
                bytes_sent: None,
                ..sample.clone()
            },
            at,
        );
        assert!(
            baseline.sample().is_none(),
            "epoch or missing field cannot become zero"
        );
        baseline.observe(&sample, at);
        let ending = NativeTrafficStatistics {
            timestamp_us: 20,
            bytes_sent: Some(150),
            ..sample.clone()
        };
        assert_eq!(
            ending.interval_delta(baseline.sample(), |stats| stats.bytes_sent),
            Ok(50)
        );
        baseline.observe(&ending, at + Duration::from_millis(20));
        assert_eq!(
            baseline.sample().unwrap().bytes_sent,
            Some(100),
            "do not move a valid start"
        );
        baseline.begin(Some(ending.clone()), Some(1), at);
        assert_eq!(baseline.sample().unwrap().timestamp_us, ending.timestamp_us);
    }

    #[test]
    fn native_interval_rejects_missing_stale_reset_and_regressing_samples() {
        let baseline = NativeTrafficStatistics {
            generation: 1,
            timestamp_us: 10,
            bytes_sent: Some(100),
            bytes_received: Some(200),
            keyframe_requests: None,
            outbound_video: BTreeMap::new(),
            report_json: vec![],
        };
        let mut current = NativeTrafficStatistics {
            timestamp_us: 20,
            bytes_received: Some(250),
            ..baseline.clone()
        };
        let received: fn(&NativeTrafficStatistics) -> Option<u64> = |stats| stats.bytes_received;
        assert_eq!(current.interval_delta(Some(&baseline), received), Ok(50));
        assert!(current.interval_delta(None, received).is_err());
        current.timestamp_us = 10;
        assert!(current.interval_delta(Some(&baseline), received).is_err());
        current.timestamp_us = 20;
        current.generation = 2;
        assert!(current.interval_delta(Some(&baseline), received).is_err());
        current.generation = 1;
        current.bytes_received = None;
        assert!(current.interval_delta(Some(&baseline), received).is_err());
        current.bytes_received = Some(199);
        assert!(current.interval_delta(Some(&baseline), received).is_err());
    }

    #[test]
    fn decoded_origin_gate_rejects_stale_missing_and_substituted_delivery() {
        let mut receiver = VideoReceiveLog::default();
        receiver.record_decoded(Some("intended"), 320, 180, None, true, None);
        let baseline = receiver.decoded_from("intended").unwrap().frames;
        assert!(receiver.require_decoded_from("intended", 0, 1).is_ok());
        assert!(
            receiver
                .require_decoded_from("intended", baseline, 1)
                .is_err()
        );

        receiver.record_decoded(Some("foreign"), 320, 180, None, true, None);
        assert!(
            receiver
                .require_decoded_from("intended", baseline, 1)
                .is_err()
        );
        assert!(receiver.require_not_received_from("foreign").is_err());

        let missing_receiver = VideoReceiveLog::default();
        assert!(
            missing_receiver
                .require_decoded_from("intended", 0, 1)
                .is_err()
        );
        assert!(
            missing_receiver
                .require_not_received_from("foreign")
                .is_ok()
        );

        receiver.record_decoded(Some("intended"), 320, 180, None, true, None);
        assert!(
            receiver
                .require_decoded_from("intended", baseline, 1)
                .is_ok()
        );
        assert!(
            receiver
                .require_decoded_from("intended", baseline, 2)
                .is_err()
        );
        assert!(receiver.require_not_received_from("foreign").is_err());

        let mut packet_only = VideoReceiveLog::default();
        packet_only.by_publisher.insert("foreign".to_owned(), 1);
        assert!(packet_only.require_not_received_from("foreign").is_err());
        assert!(packet_only.require_decoded_from("foreign", 0, 1).is_err());
    }

    #[test]
    fn a_keyframe_without_parameter_sets_is_recorded_as_undecodable() {
        let log = Arc::new(Mutex::new(VideoReceiveLog::default()));
        let references = Arc::new(Mutex::new(BTreeMap::new()));
        let mut decoder = H264ReferenceDecoder::new("source", "video");
        let mut decoder_ready = false;
        let frame = owned_video(1, 0, true, vec![0, 0, 0, 1, 0x65, 0]);

        record_video_frame(
            &log,
            &references,
            &mut decoder,
            &mut decoder_ready,
            &mut None,
            frame,
            false,
        );

        let log = log.lock().unwrap();
        assert_eq!(log.frames, 0);
        assert_eq!(log.decoder_errors, 1);
        assert_eq!(log.decoder_error_keyframes, 1);
        assert!(
            log.decoded_from("source").is_none(),
            "malformed content has no proven publisher"
        );
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
        Self {
            decoder: OpusReferenceDecoder::new(publisher, "audio-mono"),
            prepared: quality_audio_reference(),
        }
    }

    fn decode_packet(
        &mut self,
        payload: &[u8],
        log: &Arc<Mutex<AudioReceiveLog>>,
        publisher: &NativeAudioStream,
    ) {
        let reference = self.prepared.encoded_frames.get(payload).and_then(|index| {
            self.prepared
                .corpus
                .reference_frame(&self.prepared.reference, *index)
        });
        // Native RTP timestamp origins are not the application's input origin.
        // Match authentic bytes, not a guessed random sender timestamp offset.
        if reference.is_none() {
            log.lock().unwrap().record_reference_unavailable(publisher);
        }
        match self.decoder.try_decode_observation(payload, reference) {
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
    pub publication: Option<String>,
    pub height: u32,
    pub min_height: u32,
    pub priority: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOutboundVideo {
    /// Resolved through the report's actual mediaSourceId -> trackIdentifier.
    pub slot: Option<String>,
    pub bytes_sent: Option<u64>,
    pub packets_sent: Option<u64>,
    pub frames_sent: Option<u64>,
}

/// A real starting sample, never an invented zero. If a window begins before
/// native statistics exist, retain the first usable report INSIDE that window.
/// Its shorter measured interval makes the original byte threshold stricter;
/// neither the window nor its deadline is extended.
#[derive(Default, Debug)]
pub struct NativeTrafficBaseline {
    sample: Option<NativeTrafficStatistics>,
    expected_generation: Option<u64>,
    opened_at: Option<Instant>,
}
impl NativeTrafficBaseline {
    pub fn begin(
        &mut self,
        sample: Option<NativeTrafficStatistics>,
        generation: Option<u64>,
        at: Instant,
    ) {
        self.expected_generation =
            generation.or_else(|| sample.as_ref().map(|stats| stats.generation));
        self.sample = sample;
        self.opened_at = Some(at);
    }

    pub fn observe(&mut self, sample: &NativeTrafficStatistics, at: Instant) {
        if self.sample.is_none()
            && self.opened_at.is_some_and(|opened| at >= opened)
            && self
                .expected_generation
                .is_none_or(|generation| sample.generation == generation)
            && sample.bytes_sent.is_some()
            && sample.bytes_received.is_some()
        {
            self.expected_generation = Some(sample.generation);
            self.sample = Some(sample.clone());
        }
    }

    pub fn sample(&self) -> Option<&NativeTrafficStatistics> {
        self.sample.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct NativeTrafficStatistics {
    pub generation: u64,
    pub timestamp_us: i64,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
    /// Actual outbound-video RTCStats PLI count, not producer feedback polls.
    pub keyframe_requests: Option<u64>,
    pub outbound_video: BTreeMap<u32, NativeOutboundVideo>,
    /// Retain the actual report for failure diagnostics, not reconstructed stats.
    pub report_json: Vec<u8>,
}
impl NativeTrafficStatistics {
    pub fn has_video_sender(&self, slot: &str) -> bool {
        self.outbound_video.values().any(|video| {
            video.slot.as_deref() == Some(slot)
                && video.bytes_sent.is_some()
                && video.packets_sent.is_some()
                && video.frames_sent.is_some()
        })
    }

    /// Progress on a previously observed native sender, not just padding,
    /// retransmissions, control bytes, or a replacement generation's counters.
    pub fn video_progress_since(&self, baseline: &Self, slot: &str) -> Result<bool, String> {
        if self.generation != baseline.generation {
            return Err("native video sample crossed generations".into());
        }
        if self.timestamp_us <= baseline.timestamp_us {
            return Err("native video ending report is not fresh".into());
        }
        let mut observed = false;
        let mut progress = false;
        for (ssrc, before) in baseline
            .outbound_video
            .iter()
            .filter(|(_, video)| video.slot.as_deref() == Some(slot))
        {
            observed = true;
            let after = self
                .outbound_video
                .get(ssrc)
                .ok_or("old native video SSRC disappeared")?;
            if after.slot != before.slot {
                return Err("native video SSRC changed source slot".into());
            }
            let delta = |after: Option<u64>, before: Option<u64>| {
                after
                    .ok_or("native video counter unavailable")?
                    .checked_sub(before.ok_or("native video baseline counter unavailable")?)
                    .ok_or("native video counter regressed")
            };
            let bytes = delta(after.bytes_sent, before.bytes_sent)?;
            let packets = delta(after.packets_sent, before.packets_sent)?;
            let frames = delta(after.frames_sent, before.frames_sent)?;
            progress |= bytes > 0 && packets > 0 && frames > 0;
        }
        if !observed {
            return Err("native video baseline has no identified source sender".into());
        }
        Ok(progress)
    }

    pub fn interval_delta(
        &self,
        baseline: Option<&Self>,
        field: fn(&Self) -> Option<u64>,
    ) -> Result<u64, String> {
        let baseline = baseline.ok_or("native interval starting sample unavailable")?;
        if baseline.generation != self.generation {
            return Err("native interval crossed generations".into());
        }
        if self.timestamp_us <= baseline.timestamp_us {
            return Err("native interval ending report is not fresh".into());
        }
        let start = field(baseline).ok_or("native interval starting field unavailable")?;
        let end = field(self).ok_or("native interval ending field unavailable")?;
        end.checked_sub(start)
            .ok_or_else(|| "native cumulative counter regressed".into())
    }

    fn from_report(report: &pulsebeam_agent_native::NativeStatistics) -> anyhow::Result<Self> {
        let json: serde_json::Value = serde_json::from_slice(&report.report_json)?;
        let records: Vec<_> = match &json {
            serde_json::Value::Array(records) => records.iter().collect(),
            serde_json::Value::Object(records) => records.values().collect(),
            _ => anyhow::bail!("native RTC statistics are not a collection"),
        };
        fn sum<'a>(
            records: impl Iterator<Item = &'a serde_json::Value>,
            field: &str,
        ) -> Option<u64> {
            let mut total = None::<u64>;
            for record in records {
                total = Some(
                    total
                        .unwrap_or(0)
                        .checked_add(record.get(field)?.as_u64()?)?,
                );
            }
            total
        }
        let transports = || {
            records.iter().copied().filter(|record| {
                record.get("type").and_then(|value| value.as_str()) == Some("transport")
            })
        };
        let outbound_video = || {
            records.iter().copied().filter(|record| {
                record.get("type").and_then(|value| value.as_str()) == Some("outbound-rtp")
                    && record
                        .get("kind")
                        .or_else(|| record.get("mediaType"))
                        .and_then(|value| value.as_str())
                        == Some("video")
            })
        };
        Ok(Self {
            generation: report.generation,
            timestamp_us: report.timestamp_us,
            bytes_sent: sum(transports(), "bytesSent"),
            bytes_received: sum(transports(), "bytesReceived"),
            keyframe_requests: sum(outbound_video(), "pliCount"),
            outbound_video: {
                let mut video = BTreeMap::new();
                for record in outbound_video() {
                    let Some(ssrc) = record
                        .get("ssrc")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                    else {
                        continue;
                    };
                    let source_id = record.get("mediaSourceId").and_then(|value| value.as_str());
                    let slot = records
                        .iter()
                        .find(|source| {
                            source.get("type").and_then(|value| value.as_str())
                                == Some("media-source")
                                && source.get("id").and_then(|value| value.as_str()) == source_id
                                && source_id.is_some()
                                && source.get("kind").and_then(|value| value.as_str())
                                    == Some("video")
                        })
                        .and_then(|source| source.get("trackIdentifier"))
                        .and_then(|value| value.as_str())
                        .map(str::to_owned);
                    let sender = NativeOutboundVideo {
                        slot,
                        bytes_sent: record.get("bytesSent").and_then(serde_json::Value::as_u64),
                        packets_sent: record
                            .get("packetsSent")
                            .and_then(serde_json::Value::as_u64),
                        frames_sent: record.get("framesSent").and_then(serde_json::Value::as_u64),
                    };
                    anyhow::ensure!(
                        video.insert(ssrc, sender).is_none(),
                        "duplicate native outbound video SSRC"
                    );
                }
                video
            },
            report_json: report.report_json.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ClientObservation {
    pub ordered_cursors: Vec<(TopicSubscriber, String, u64)>,
    pub admission: Option<pulsebeam::entity::ConnectionId>,
    pub video_receivers: BTreeSet<u32>,
    pub audio_receivers: BTreeSet<u32>,
    pub snapshot: pulsebeam_agent_native::agent_core::Snapshot,
    pub sent_intent_revision: Option<u64>,
    pub last_successfully_sent_wire_revision: Option<u64>,
    pub video_operation: u64,
    pub accepted: Option<DesiredState>,
    pub submitted: DesiredState,
    pub video_capacity: usize,
    pub audio_capacity: usize,
}

pub struct ClientContext {
    pub agent: Agent,
    admission: Arc<Mutex<Option<pulsebeam::entity::ConnectionId>>>,
    desired: DesiredState,
    video_capacity: usize,
    audio_capacity: usize,
    auto_subscribe: bool,
    requested_video: Option<Vec<RequestedVideo>>,
    video_operation: u64,
    pub video_rx: Arc<Mutex<VideoReceiveLog>>,
    pub audio_rx: Arc<Mutex<AudioReceiveLog>>,
    quality_references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<FixturePublishers>>,
    corrupt_video_payload: bool,
    keyframe_recovery: Arc<Mutex<KeyframeRecoveryProbe>>,
    pub discovered_tracks: HashSet<String>,
    pub remote_tracks: HashMap<String, String>,
    pub topic_events: Vec<TopicNotification>,
    pub media_kinds: HashMap<String, (bool, bool)>,
    snapshots: tokio::sync::watch::Receiver<pulsebeam_agent_native::agent_core::Snapshot>,
    events: pulsebeam_agent_native::ObservationStream<AgentEvent>,
    owned_video: Option<u64>,
}

impl ClientContext {
    pub async fn observation(&self) -> anyhow::Result<ClientObservation> {
        let observed = self.agent.simulation_observation().await?;
        let snapshot = observed.snapshot;
        // Wire revisions can advance when catalog removal changes the encoded intent without
        // a new application revision. Correlate both namespaces at the native owner boundary.
        let accepted = (observed.sent_intent_revision == Some(snapshot.accepted_intent_revision)
            && snapshot.desired_revision == self.desired.revision)
            .then(|| self.desired.clone());
        Ok(ClientObservation {
            ordered_cursors: observed.ordered_cursors,
            admission: *self.admission.lock().unwrap(),
            video_receivers: observed
                .media_indices
                .iter()
                .filter_map(|(slot, index)| {
                    matches!(
                        slot,
                        pulsebeam_agent_native::agent_core::MediaSlot::RemoteVideo(_)
                    )
                    .then_some(*index)
                })
                .collect(),
            audio_receivers: observed
                .media_indices
                .iter()
                .filter_map(|(slot, index)| {
                    matches!(
                        slot,
                        pulsebeam_agent_native::agent_core::MediaSlot::RemoteAudio(_)
                    )
                    .then_some(*index)
                })
                .collect(),
            snapshot,
            sent_intent_revision: observed.sent_intent_revision,
            last_successfully_sent_wire_revision: observed.last_successfully_sent_wire_revision,
            accepted,
            submitted: self.desired.clone(),
            video_operation: self.video_operation,
            video_capacity: self.video_capacity,
            audio_capacity: self.audio_capacity,
        })
    }

    pub fn participant_id(&self) -> Option<String> {
        self.agent.snapshot().participant_id
    }

    pub fn statistics(&self) -> anyhow::Result<Option<NativeTrafficStatistics>> {
        self.agent
            .statistics()
            .borrow()
            .as_ref()
            .map(NativeTrafficStatistics::from_report)
            .transpose()
    }

    pub fn connected(&self) -> bool {
        self.agent.snapshot().connection == ConnectionState::Connected
    }

    pub fn events_closed(&self) -> bool {
        self.events.is_closed()
    }

    fn record_event(
        &mut self,
        update: pulsebeam_agent_native::StreamUpdate<AgentEvent>,
    ) -> anyhow::Result<()> {
        let event = match update {
            pulsebeam_agent_native::StreamUpdate::Value(event) => event,
            pulsebeam_agent_native::StreamUpdate::Lagged { skipped } => {
                anyhow::bail!("native fixture events lagged by {skipped}")
            }
            pulsebeam_agent_native::StreamUpdate::Closed => return Ok(()),
        };
        match event {
            AgentEvent::Core(pulsebeam_agent_native::agent_core::Notification::Topic(
                notification @ (TopicNotification::Message(_)
                | TopicNotification::RecoveryGap { .. }),
            )) => self.topic_events.push(notification),
            AgentEvent::RuntimeFailed(message) => anyhow::bail!(message),
            _ => {}
        }
        Ok(())
    }

    pub async fn receive_event(&mut self) -> anyhow::Result<()> {
        let update = self.events.next().await;
        self.record_event(update)
    }

    pub fn drain_events(&mut self) -> anyhow::Result<()> {
        while let Some(update) = self.events.try_next() {
            let closed = matches!(update, pulsebeam_agent_native::StreamUpdate::Closed);
            self.record_event(update)?;
            if closed {
                break;
            }
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> anyhow::Result<()> {
        self.drain_events()?;

        if !self.snapshots.has_changed()? {
            return Ok(());
        }
        let snapshot = self.snapshots.borrow_and_update().clone();
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
            .video_mapping
            .values()
            .filter_map(|track_id| {
                snapshot
                    .publications
                    .get(track_id)
                    .map(|publication| (publication.participant_id.clone(), track_id.clone()))
            })
            .collect();
        self.sync_video_desired(&snapshot).await.map(|_| ())
    }

    pub async fn relabel_owned_video(&mut self, label: String) -> anyhow::Result<bool> {
        let owner = self
            .owned_video
            .take()
            .ok_or_else(|| anyhow::anyhow!("participant has no owned video"))?;
        self.agent
            .resource(ResourceCommand::Release { owner })
            .await?;
        let ResourceResult::Publication { owner, slot, .. } = self
            .agent
            .resource(ResourceCommand::ClaimPublication {
                spec: TrackSpec::Video { label },
            })
            .await?
        else {
            anyhow::bail!("replacement video claim returned the wrong resource")
        };
        anyhow::ensure!(slot == "v0", "replacement video did not reuse v0");
        self.agent
            .resource(ResourceCommand::UpdatePublication {
                owner,
                present: true,
            })
            .await?;
        self.owned_video = Some(owner);
        Ok(true)
    }

    pub async fn set_video_subscriptions(
        &mut self,
        subscriptions: Vec<RequestedVideo>,
        operation: u64,
    ) -> anyhow::Result<bool> {
        self.video_operation = operation;
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
                publication: None,
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
                        && request
                            .publication
                            .as_ref()
                            .is_none_or(|id| *id == publication.id)
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
                selector: None,
                height: request.height,
                min_height: request.min_height,
                min_fps: 0,
                priority: request.priority,
                playout_delay: Default::default(),
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
        let audio = AudioSubscription {
            pinned,
            automatic,
            ..Default::default()
        };
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
            .any(|status| {
                status.registration == registration
                    && status.channel.is_some()
                    && status.data_track_id.is_some()
            }))
    }

    pub async fn register_subscriber(
        &mut self,
        topic: String,
        mode: TopicMode,
    ) -> anyhow::Result<bool> {
        let registration = TopicSubscriber { topic, mode };
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
            .any(|status| {
                status.registration == publisher
                    && status.channel.is_some()
                    && status.data_track_id.is_some()
            });
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

// Keep application recipes across reconnect, never generation-scoped native
// sources, clocks or observer leases. Each replacement gets fresh public handles.
struct ClientMediaPlan {
    video_slots: usize,
    audio_slots: usize,
    video_receive_mode: pulsebeam_agent_native::ReceiveMode,
    audio_receive_mode: pulsebeam_agent_native::ReceiveMode,
    publishes_video: bool,
    opaque_payload: bool,
    video_sources: Vec<(Option<String>, VideoSource)>,
    vbr_source: Option<VbrSource>,
    audio_level_dbov: Option<i8>,
    audio_phase_offset: u64,
    audio_input: AudioInput,
    corrupt_audio_payload: bool,
}

pub struct SimClient {
    pub ctx: ClientContext,
    join_set: JoinSet<()>,
    actor: Option<tokio::task::JoinHandle<()>>,
    media_generation: Option<u64>,
    media: ClientMediaPlan,
}

impl Drop for SimClient {
    fn drop(&mut self) {
        if let Some(actor) = &self.actor {
            actor.abort();
        }
    }
}

impl SimClient {
    async fn rearm_media(&mut self) -> anyhow::Result<()> {
        let generation = {
            let snapshot = self.ctx.snapshots.borrow();
            if snapshot.connection != ConnectionState::Connected {
                return Ok(());
            }
            snapshot
                .generation
                .map(pulsebeam_agent_native::agent_core::Generation::get)
                .ok_or_else(|| anyhow::anyhow!("connected native graph has no generation"))?
        };
        if self.media_generation == Some(generation) {
            return Ok(());
        }
        // The actor owns retirement. Cancel only application feeds/observers;
        // never carry an old handle or capture clock into its replacement.
        self.join_set.abort_all();
        while let Some(result) = self.join_set.join_next().await {
            match result {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(error.into()),
            }
        }
        let ctx = &self.ctx;
        let agent = ctx.agent.clone();
        let plan = &self.media;
        for slot in 0..plan.video_slots {
            let remote = agent
                .remote(
                    pulsebeam_agent_native::agent_core::MediaSlot::RemoteVideo(u8::try_from(slot)?),
                    plan.video_receive_mode,
                )
                .await?;
            spawn_video_receiver(
                &mut self.join_set,
                remote,
                ctx,
                agent.clone(),
                plan.video_receive_mode,
            );
        }
        for slot in 0..plan.audio_slots {
            let remote = agent
                .remote(
                    pulsebeam_agent_native::agent_core::MediaSlot::RemoteAudio(u8::try_from(slot)?),
                    plan.audio_receive_mode,
                )
                .await?;
            let log = ctx.audio_rx.clone();
            if plan.audio_receive_mode == pulsebeam_agent_native::ReceiveMode::Encoded {
                log.lock().unwrap().encoded_observers += 1;
            }
            let snapshots = agent.snapshots();
            let corrupt = plan.corrupt_audio_payload;
            self.join_set.spawn(async move {
                let mut decoders = HashMap::<NativeAudioStream, OpusReceiver>::new();
                loop {
                    let frame = match remote.next().await {
                        pulsebeam_agent_native::MediaStreamUpdate::Frame { frame } => frame,
                        pulsebeam_agent_native::MediaStreamUpdate::Track { .. } => continue,
                        pulsebeam_agent_native::MediaStreamUpdate::Closed => break,
                        other => panic!("native audio observer lost media: {other:?}"),
                    };
                    if snapshots
                        .borrow()
                        .generation
                        .map(pulsebeam_agent_native::agent_core::Generation::get)
                        != Some(frame.track.generation)
                    {
                        continue;
                    }
                    if matches!(
                        &frame.frame,
                        pulsebeam_agent_native::OutputFrame::Pcm { .. }
                    ) {
                        assert!(
                            !corrupt,
                            "encoded corruption oracle requires encoded output"
                        );
                        log.lock()
                            .unwrap()
                            .native_pcm
                            .observe(&frame, Instant::now());
                        continue;
                    }
                    // Catalog mapping is not the provenance of buffered native
                    // media. Keep actual receiver/SSRC identity in this oracle.
                    let stream = NativeAudioStream {
                        generation: frame.track.generation,
                        receiver: frame.track.receiver,
                        ssrc: frame.metadata.ssrc.expect("native encoded Opus SSRC"),
                    };
                    log.lock()
                        .unwrap()
                        .tracks
                        .insert(stream.clone(), frame.track.clone());
                    let pulsebeam_agent_native::OutputFrame::Opus { mut payload, .. } = frame.frame
                    else {
                        panic!("encoded audio observer returned another media kind");
                    };
                    if corrupt {
                        payload.fill(0xff);
                    }
                    log.lock().unwrap().record(
                        &stream,
                        stream.ssrc,
                        frame
                            .metadata
                            .sequence_number
                            .expect("native encoded Opus sequence"),
                        payload.len(),
                        Instant::now(),
                    );
                    decoders
                        .entry(stream.clone())
                        .or_insert_with(|| OpusReceiver::new(&format!("{stream:?}")))
                        .decode_packet(&payload, &log, &stream);
                }
            });
        }
        if plan.publishes_video {
            let mut sources = plan
                .video_sources
                .iter()
                .map(|(encoding, source)| (encoding.clone(), source.fresh()))
                .collect::<Vec<_>>();
            let mut vbr = plan.vbr_source.as_ref().map(VbrSource::fresh);
            if !plan.opaque_payload {
                let participant_id =
                    ctx.snapshots
                        .borrow()
                        .participant_id
                        .clone()
                        .ok_or_else(|| {
                            anyhow::anyhow!("connected source has no participant identity")
                        })?;
                let admission = (*ctx.admission.lock().unwrap()).ok_or_else(|| {
                    anyhow::anyhow!("connected source has no HTTP admission identity")
                })?;
                let origin = provenance::source_key(&admission.to_string(), "v0");
                let mut frames = BTreeSet::new();
                for (_, source) in &mut sources {
                    frames.extend(source.reference_frames().map(|frame| {
                        provenance::inspect(frame)
                            .expect("valid source access unit")
                            .1
                    }));
                    source.identify_as(&origin);
                }
                if let Some(source) = &mut vbr {
                    frames.extend(
                        source.reference_frames().map(|frame| {
                            provenance::inspect(frame).expect("valid VBR access unit").1
                        }),
                    );
                    source.identify_as(&origin);
                }
                // Register actual input bytes for THIS source admission. Rearm
                // may reuse the logical participant, but never its old marker.
                ctx.h264_publishers.lock().unwrap().insert(
                    origin,
                    FixturePublisher {
                        participant_id,
                        frames,
                    },
                );
            }
            let media = agent
                .source(pulsebeam_agent_native::agent_core::MediaSlot::LocalVideo(
                    "v0".into(),
                ))
                .await?;
            if let Some(source) = vbr {
                self.join_set.spawn(source.run(media));
            } else {
                self.join_set.spawn(VideoSource::run_batch(sources, media));
            }
        }
        if let Some(level) = plan.audio_level_dbov {
            self.join_set.spawn(
                QualityAudioLooper {
                    level_dbov: level,
                    phase_offset: plan.audio_phase_offset,
                }
                .run(
                    plan.audio_input,
                    agent.snapshots(),
                    agent
                        .source(pulsebeam_agent_native::agent_core::MediaSlot::LocalAudio(
                            "a0".into(),
                        ))
                        .await?,
                ),
            );
        }
        tracing::debug!(target: "pulsebeam_sim_recovery", generation, "application media attached to connected native generation");
        self.media_generation = Some(generation);
        Ok(())
    }

    pub async fn tick(&mut self) -> anyhow::Result<()> {
        while let Some(result) = self.join_set.try_join_next() {
            result?;
        }
        if self
            .actor
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(actor) = self.actor.take() {
                actor.await?;
            }
            anyhow::bail!("native fixture actor stopped before close");
        }
        self.ctx.refresh().await?;
        self.rearm_media().await
    }

    pub async fn close(mut self) -> anyhow::Result<()> {
        self.join_set.abort_all();
        // Do not abort the actor alongside application feeds before it can
        // acknowledge the core's graceful close and retire its native graph.
        self.ctx.agent.close().await?;
        self.ctx.agent.abort().await?;
        if let Some(actor) = self.actor.take() {
            actor.await?;
        }
        Ok(())
    }

    pub async fn abort(mut self) -> anyhow::Result<()> {
        self.join_set.abort_all();
        self.ctx.agent.abort().await?;
        if let Some(actor) = self.actor.take() {
            actor.await?;
        }
        Ok(())
    }
}

fn spawn_video_receiver(
    join_set: &mut JoinSet<()>,
    remote: pulsebeam_agent_native::RemoteMedia,
    ctx: &ClientContext,
    agent: Agent,
    mode: pulsebeam_agent_native::ReceiveMode,
) {
    let log = ctx.video_rx.clone();
    if mode == pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded {
        log.lock().unwrap().native_origin_enabled = true;
    }
    let references = ctx.quality_references.clone();
    let publishers = ctx.h264_publishers.clone();
    let corrupt = ctx.corrupt_video_payload;
    let native_playback_only = log.lock().unwrap().native_playback_only;
    let recovery_probe = ctx.keyframe_recovery.clone();
    let snapshots = agent.snapshots();
    join_set.spawn(async move {
        let mut one_shot = false;
        let mut streams = HashMap::<NativeVideoStream, VideoStreamReceiver>::new();
        type FeedbackRequest<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
        let mut feedback: Option<FeedbackRequest<'_>> = None;
        let mut feedback_context: Option<(NativeVideoStream, tokio::time::Instant, bool)> = None;
        let mut queued_one_shot = None;
        loop {
            // A control-command acknowledgement must not stop collecting a
            // best-effort media stream. Keep exactly one request in flight;
            // retain the existing retry interval and one-shot probe semantics.
            let update = tokio::select! {
                biased;
                succeeded = async {
                    match feedback.as_mut() {
                        Some(request) => request.await,
                        None => std::future::pending().await,
                    }
                } => {
                    feedback = None;
                    let (identity, issued, once) = feedback_context.take().expect("in-flight feedback context");
                    if succeeded {
                        if let Some(stream) = streams.get_mut(&identity) {
                            stream.acknowledge_keyframe(issued);
                        }
                        if once { recovery_probe.lock().unwrap().one_shot_requests += 1; }
                    }
                    if let Some(identity) = queued_one_shot.take() {
                        feedback_context = Some((identity, tokio::time::Instant::now(), true));
                        feedback = Some(Box::pin(async { remote.request_keyframe().await.is_ok() }));
                    }
                    continue;
                },
                update = remote.next() => update,
            };
            let frame = match update {
                pulsebeam_agent_native::MediaStreamUpdate::Frame { frame } => frame,
                pulsebeam_agent_native::MediaStreamUpdate::Track { .. } => continue,
                pulsebeam_agent_native::MediaStreamUpdate::Closed => break,
                pulsebeam_agent_native::MediaStreamUpdate::NativeDropped { skipped } => {
                    if mode == pulsebeam_agent_native::ReceiveMode::Decoded
                        && log.lock().unwrap().native_callback_enabled
                    {
                        log.lock().unwrap().native_decoded.polling_dropped(skipped);
                        continue;
                    }
                    record_video_drop(
                        &log,
                        &mut streams,
                        skipped,
                        "native SDK sink",
                        mode != pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded,
                    );
                    continue;
                }
                pulsebeam_agent_native::MediaStreamUpdate::NativeDecodedDropped { skipped } => {
                    tracing::warn!(skipped, cause = "native decoded SDK sink", "native video observation loss");
                    let mut observed = log.lock().unwrap();
                    if observed.native_callback_enabled {
                        observed.native_decoded.polling_dropped(skipped);
                    } else {
                        observed.native_decoded.dropped(skipped);
                    }
                    continue;
                }
                pulsebeam_agent_native::MediaStreamUpdate::Lagged { skipped } => {
                    record_video_drop(
                        &log,
                        &mut streams,
                        skipped,
                        "application observation buffer",
                        true,
                    );
                    continue;
                }
            };
            if snapshots
                .borrow()
                .generation
                .map(pulsebeam_agent_native::agent_core::Generation::get)
                != Some(frame.track.generation)
            {
                continue;
            }
            if matches!(
                &frame.frame,
                pulsebeam_agent_native::OutputFrame::I420 { .. }
            ) {
                let mut observed = log.lock().unwrap();
                // Polling remains available to application consumers, but is
                // not a second native callback and cannot alter its cadence.
                if !observed.native_callback_enabled {
                    if mode == pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded {
                        observed.record_dual_native_decoded(&frame, Instant::now());
                    } else {
                        observed.record_native_decoded(&frame, Instant::now());
                    }
                }
                continue;
            }
            if native_playback_only {
                record_native_fixture_sample(&log, &publishers.lock().unwrap(), &frame);
                continue;
            }
            let identity = NativeVideoStream::from(&frame);
            let stream = streams
                .entry(identity.clone())
                .or_insert_with(|| VideoStreamReceiver {
                    decoder: H264ReferenceDecoder::new(format!("{identity:?}"), "native video"),
                    decoder_ready: false,
                    retained_publisher: None,
                    last_keyframe_request: None,
                });
            let request_once = std::mem::take(&mut recovery_probe.lock().unwrap().request_once);
            if request_once {
                one_shot = true;
                stream.decoder_ready = false;
                queued_one_shot = Some(identity.clone());
            }
            record_fixture_video_frame(
                &log,
                FixtureOracle {
                    references: &references,
                    publishers: Some(&publishers.lock().unwrap()),
                },
                &mut stream.decoder,
                &mut stream.decoder_ready,
                &mut stream.retained_publisher,
                frame,
                corrupt,
            );
            let now = tokio::time::Instant::now();
            let should_request = !stream.decoder_ready
                && stream
                    .last_keyframe_request
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500));
            // A one-shot probe must not be rescued by this caller retry loop.
            if feedback.is_none() {
                let request = queued_one_shot.take().map(|identity| (identity, true))
                    .or_else(|| (!one_shot && should_request).then_some((identity, false)));
                if let Some((identity, once)) = request {
                    feedback_context = Some((identity, now, once));
                    feedback = Some(Box::pin(async { remote.request_keyframe().await.is_ok() }));
                }
            }
            if stream.decoder_ready {
                stream.last_keyframe_request = None;
            }
        }
    });
}

fn record_video_drop(
    log: &Arc<Mutex<VideoReceiveLog>>,
    streams: &mut HashMap<NativeVideoStream, VideoStreamReceiver>,
    skipped: u64,
    cause: &str,
    affects_native_decoded: bool,
) {
    tracing::warn!(skipped, cause, "native video observation loss");
    let mut log = log.lock().unwrap();
    log.non_contiguous += skipped;
    log.native_queue_drops += skipped;
    log.native_origin.encoded_observation_lost();
    if affects_native_decoded && !log.native_callback_enabled {
        log.native_decoded.dropped(skipped);
    }
    for stream in streams.values_mut() {
        stream.decoder_ready = false;
    }
}

struct VideoStreamReceiver {
    decoder: H264ReferenceDecoder,
    decoder_ready: bool,
    retained_publisher: Option<String>,
    last_keyframe_request: Option<tokio::time::Instant>,
}

impl VideoStreamReceiver {
    fn acknowledge_keyframe(&mut self, issued: tokio::time::Instant) {
        // Collection may already have recovered while this command awaited its
        // acknowledgement. Do not resurrect a throttle cleared by that frame.
        self.last_keyframe_request = (!self.decoder_ready).then_some(issued);
    }
}

fn generic_video_reference(encodings: &[Option<String>]) -> &'static QualityVideoReference {
    // Seven nonempty subsets of the three actual CBR inputs. Share immutable
    // payloads only; never infer a publisher from a catalog or native receiver.
    static REFERENCES: [OnceLock<QualityVideoReference>; 7] = [const { OnceLock::new() }; 7];
    let mask = encodings.iter().fold(0usize, |mask, rid| {
        mask | match rid.as_deref() {
            Some("f") => 1,
            Some("h") => 2,
            _ => 4,
        }
    });
    REFERENCES[mask.checked_sub(1).expect("nonempty video encodings")].get_or_init(|| {
        let mut frames = Vec::new();
        for (bit, rid) in [(1, Some("f")), (2, Some("h")), (4, None)] {
            if mask & bit != 0 {
                frames.extend(generic_video_frames(rid).iter().cloned());
            }
        }
        let mut encoded_frames = BTreeMap::new();
        for (index, frame) in frames.iter().enumerate() {
            encoded_frames
                .entry(canonical_annex_b(frame))
                .or_insert(index);
        }
        QualityVideoReference {
            quality: None,
            encoded_frames,
            frames: frames.into(),
        }
    })
}

fn create_video_source(encoding: Option<&str>) -> VideoSource {
    VideoSource::from_frames(generic_video_frames(encoding), 30)
}

fn generic_video_frames(encoding: Option<&str>) -> Arc<[Arc<[u8]>]> {
    type EncodedFrames = Arc<[Arc<[u8]>]>;
    static FRAMES: [OnceLock<EncodedFrames>; 3] = [const { OnceLock::new() }; 3];
    let (index, data) = match encoding {
        Some("f") => (0, pulsebeam_testdata::RAW_H264_FULL_CBR),
        Some("h") => (1, pulsebeam_testdata::RAW_H264_HALF_CBR),
        _ => (2, pulsebeam_testdata::RAW_H264_QUARTER_CBR),
    };
    FRAMES
        .get(index)
        .expect("video fixture index")
        .get_or_init(|| video_frames(data))
        .clone()
}

fn create_quality_video_source(
    source: pulsebeam_testdata::QualityVideoSource,
    layer: pulsebeam_testdata::QualityVideoLayer,
) -> VideoSource {
    VideoSource::from_frames(
        quality_encoded_frames(source, layer),
        pulsebeam_testdata::QUALITY_VIDEO_FPS,
    )
}

fn create_vbr_source(encoding: Option<&str>, profile: VbrProfile) -> VbrSource {
    debug_assert_eq!(encoding, Some("f"));
    static FRAMES: OnceLock<Arc<[Arc<[u8]>]>> = OnceLock::new();
    static TIMES: OnceLock<Arc<[Duration]>> = OnceLock::new();
    VbrSource::scheduled(
        FRAMES
            .get_or_init(|| video_frames(pulsebeam_testdata::RAW_H264_SCREEN_FULL_VBR))
            .clone(),
        TIMES
            .get_or_init(|| {
                pulsebeam_testdata::RAW_H264_SCREEN_FULL_TIMING
                    .lines()
                    .map(|line| {
                        Duration::from_micros(line.parse().expect("screen frame timestamp"))
                    })
                    .collect()
            })
            .clone(),
        profile,
    )
}

struct AdmissionClient {
    inner: Box<dyn AsyncHttpClient>,
    admission: Arc<Mutex<Option<pulsebeam::entity::ConnectionId>>>,
}

impl AsyncHttpClient for AdmissionClient {
    fn execute(&self, request: HttpRequest) -> HttpResult<'_> {
        Box::pin(async move {
            let response = self.inner.execute(request).await?;
            if response.status() == http::StatusCode::CREATED {
                let location = response
                    .headers()
                    .get(http::header::LOCATION)
                    .expect("admission has Location")
                    .to_str()
                    .expect("Location is text");
                let connection = location
                    .rsplit('/')
                    .next()
                    .expect("connection resource")
                    .parse()
                    .expect("admission names a canonical connection");
                *self.admission.lock().unwrap() = Some(connection);
            }
            Ok(response)
        })
    }
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
