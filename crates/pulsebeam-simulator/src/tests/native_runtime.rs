mod provisioned;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use pulsebeam_agent_core::{DataEvent, ffi as core_ffi};
use pulsebeam_agent_native::ffi::{
    Agent, AudioSubscription, EventUpdate, NativeEvent, SnapshotUpdate, VideoSubscription,
};
use pulsebeam_agent_native::{
    Agent as RuntimeAgent, AsyncHttpClient, AudioInput, Config, H264Frame, Host, InputFrame,
    MediaStreamUpdate, OpusFrame, OutputFrame, ReceiveMode, VideoInput,
};
use pulsebeam_core::identity::{ParticipantId, RoomId};

use super::common::client::create_http_client;
use super::common::native::NativeHost;
use super::common::native_audio::NativePcmLog;
use super::common::native_video::NativeDecodedVideoLog;
use super::common::{
    DEFAULT_SIM_SHARDS, reserve_subnet, run_sim_or_timeout, sfu_node_builder, subnet_ip,
};

const NATIVE_RUNTIME_SEED: u64 = 1;

#[derive(Default)]
struct CreationEvidence {
    requests: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    posts: Vec<pulsebeam_core::net::HttpRequest>,
    locations: Vec<Vec<u8>>,
    statuses: Vec<http::StatusCode>,
    dropped: bool,
}

struct LostCreationResponse {
    inner: Box<dyn AsyncHttpClient>,
    evidence: Arc<Mutex<CreationEvidence>>,
    successor_receiver: Option<IpAddr>,
}

impl AsyncHttpClient for LostCreationResponse {
    fn execute(
        &self,
        mut request: pulsebeam_core::net::HttpRequest,
    ) -> pulsebeam_core::net::HttpResult<'_> {
        Box::pin(async move {
            let path = request.uri().path();
            if let Some(receiver) = self.successor_receiver
                && request.method() == http::Method::POST
                && matches!(path, "/api/v1/native" | "/api/v1/native/admissions")
                && self.evidence.lock().unwrap().dropped
            {
                *request.uri_mut() = format!("http://{receiver}:7070{path}").parse().unwrap();
            }
            let can_drop = self.successor_receiver.is_none()
                || serde_json::from_slice::<serde_json::Value>(request.body())
                    .is_ok_and(|body| body.get("successor_receipt").is_some());
            let creation = request.headers().get("PulseBeam-Owner").map(|owner| {
                (
                    request.body().clone(),
                    owner.as_bytes().to_vec(),
                    request
                        .headers()
                        .get("PulseBeam-Attempt")
                        .expect("creation sequence")
                        .as_bytes()
                        .to_vec(),
                )
            });
            if let Some(captured) = creation {
                let mut copy = http::Request::builder()
                    .method(request.method())
                    .uri(request.uri())
                    .body(request.body().clone())
                    .expect("copy native admission");
                *copy.headers_mut() = request.headers().clone();
                {
                    let mut evidence = self.evidence.lock().unwrap();
                    evidence.requests.push(captured);
                    evidence.posts.push(copy);
                }
                let response = self.inner.execute(request).await?;
                self.evidence
                    .lock()
                    .unwrap()
                    .statuses
                    .push(response.status());
                if response.status() == http::StatusCode::CREATED {
                    let mut evidence = self.evidence.lock().unwrap();
                    evidence.locations.push(
                        response
                            .headers()
                            .get(http::header::LOCATION)
                            .expect("created connection location")
                            .as_bytes()
                            .to_vec(),
                    );
                    if can_drop && !evidence.dropped {
                        evidence.dropped = true;
                        return Err(
                            std::io::Error::other("simulated lost creation response").into()
                        );
                    }
                }
                Ok(response)
            } else {
                self.inner.execute(request).await
            }
        })
    }
}

struct HeldCreationResponse {
    inner: Box<dyn AsyncHttpClient>,
    admitted: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl AsyncHttpClient for HeldCreationResponse {
    fn execute(
        &self,
        request: pulsebeam_core::net::HttpRequest,
    ) -> pulsebeam_core::net::HttpResult<'_> {
        Box::pin(async move {
            let creation = request.headers().contains_key("PulseBeam-Owner");
            let response = self.inner.execute(request).await?;
            if creation && response.status() == http::StatusCode::CREATED {
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    if let Some(admitted) = self.admitted.lock().unwrap().take() {
                        let _ = admitted.send(());
                    }
                    release
                        .await
                        .map_err(|_| std::io::Error::other("creation gate closed"))?;
                }
            }
            Ok(response)
        })
    }
}

struct RuntimeRetirement(Rc<Cell<usize>>);

impl Drop for RuntimeRetirement {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_add(1));
    }
}

#[derive(Default)]
struct ExchangeReport {
    video_frames: u64,
    audio_frames: u64,
    // Original owning values, held across later callbacks, reconnect and close.
    retained: Vec<OutputFrame>,
}

/// One bounded second of genuine native stereo Opus. Never rewrite packet bits
/// to reconcile a mono fixture with the SFU's negotiated stereo carrier.
fn stock_opus_fixture() -> &'static [OpusFrame] {
    static FIXTURE: OnceLock<Vec<OpusFrame>> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        use pulsebeam_rtc_simulation::{
            AudioEncoderFactory, AudioEncoderOptions, Environment, SdpAudioFormat,
        };
        use pulsebeam_testdata::{QualityAudioSource, quality_corpus_audio};

        let reference = [QualityAudioSource::Zero, QualityAudioSource::One].map(|source| {
            quality_corpus_audio(source)
                .decode_reference()
                .expect("signed little-endian PCM reference")
        });
        let environment = Environment::builder().build().expect("encoder environment");
        let factory = AudioEncoderFactory::builtin().expect("stock audio encoder factory");
        let mut format = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
        format.parameters.insert(b"stereo".to_vec(), b"1".to_vec());
        let encoder = factory
            .create(
                &environment,
                &format,
                &AudioEncoderOptions {
                    payload_type: 111,
                    codec_pair_id: None,
                },
            )
            .expect("native Opus encoder creation")
            .expect("stock Opus support");
        assert_eq!(encoder.channels(), 2);
        assert_eq!(encoder.sample_rate_hz(), 48_000);
        assert_eq!(encoder.frames_in_next_packet(), 2);
        encoder.set_target_bitrate(128_000).expect("stereo bitrate");
        assert!(encoder.set_dtx(false));
        let mut packets = Vec::new();
        for block in 0..100_u32 {
            let offset = usize::try_from(block).unwrap() * 480 * 2;
            let left = reference[0][offset..offset + 960].as_chunks::<2>().0;
            let right = reference[1][offset..offset + 960].as_chunks::<2>().0;
            let pcm: Vec<_> = left
                .iter()
                .zip(right)
                .flat_map(|(left, right)| [i16::from_le_bytes(*left), i16::from_le_bytes(*right)])
                .collect();
            let output = encoder
                .encode(block * 480, &pcm)
                .expect("stock Opus encode");
            if !output.bytes.is_empty() {
                assert_eq!(output.info.primary.encoded_bytes, output.bytes.len());
                packets.push(OpusFrame {
                    timestamp: output.info.primary.encoded_timestamp,
                    samples_per_channel: 960,
                    payload: output.bytes,
                    // Explicit producer declaration for this active reference interval,
                    // not inferred from native encoder speech/CNG metadata.
                    audio_level: Some(30),
                    voice_activity: true,
                });
            }
        }
        assert_eq!(packets.len(), 50);
        packets
    })
}

#[derive(Default)]
struct MediaObservation {
    pcm: NativePcmLog,
    video: NativeDecodedVideoLog,
    video_baseline: NativeDecodedVideoLog,
    active: bool,
    closed: bool,
    blocks: u64,
    retained: Option<OutputFrame>,
    failure: Option<String>,
}

/// Consume bounded native streams during control-plane waits too. Validate the
/// real native output and retain one owning value, never another frame queue.
struct MediaProbe {
    state: Arc<Mutex<MediaObservation>>,
    reader: tokio::task::JoinHandle<()>,
    kind: core_ffi::MediaKind,
    generation: u64,
}

impl Drop for MediaProbe {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl MediaProbe {
    fn start(
        receiver: Arc<pulsebeam_agent_native::ffi::RemoteMediaReceiver>,
        generation: u64,
        kind: core_ffi::MediaKind,
    ) -> Self {
        let state = Arc::new(Mutex::new(MediaObservation::default()));
        let observed = state.clone();
        let expected_kind = kind;
        let reader = tokio::task::spawn_local(async move {
            loop {
                let update = receiver.next().await;
                let mut state = observed.lock().unwrap();
                match update {
                    MediaStreamUpdate::Frame { frame } => {
                        if frame.track.generation != generation {
                            state.failure = Some("cross-generation native media".into());
                            return;
                        }
                        let retain = match (&frame.frame, expected_kind) {
                            (OutputFrame::Pcm { samples, .. }, core_ffi::MediaKind::Audio) => {
                                let nontrivial =
                                    samples.iter().any(|sample| sample.unsigned_abs() > 32);
                                state.pcm.observe(&frame, Instant::now());
                                nontrivial
                            }
                            (OutputFrame::I420 { frame: pixels }, core_ffi::MediaKind::Video)
                                if pixels.width == 320 && pixels.height == 180 =>
                            {
                                if state.active && state.blocks == 0 {
                                    tracing::debug!(generation, now = ?Instant::now(), previous = ?state.video, "first native video frame in media window");
                                }
                                state.video.observe(&frame, Instant::now());
                                true
                            }
                            _ => {
                                state.failure = Some("invalid native decoded media".into());
                                return;
                            }
                        };
                        state.blocks = state.blocks.saturating_add(1);
                        if retain && state.retained.is_none() {
                            state.retained = Some(frame.frame);
                        }
                    }
                    MediaStreamUpdate::Track { .. } => {}
                    MediaStreamUpdate::Lagged { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} observer lagged {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::NativeDecodedDropped { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} decoded observer dropped {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::NativeDropped { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} sink dropped {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::Closed => {
                        state.closed = true;
                        return;
                    }
                }
            }
        });
        Self {
            state,
            reader,
            kind,
            generation,
        }
    }

    fn begin_window(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        anyhow::ensure!(!state.closed, "native media observer closed before window");
        tracing::debug!(kind = ?self.kind, generation = self.generation, now = ?Instant::now(), previous = ?state.video, "native media window begins");
        state.active = true;
        state.pcm.begin_window();
        state.video_baseline = state.video.checkpoint_after_pause();
        state.blocks = 0;
        state.retained = None;
        Ok(())
    }

    fn ready(&self, generation: u64) -> anyhow::Result<bool> {
        anyhow::ensure!(
            generation == self.generation,
            "wrong media observer generation"
        );
        let state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        anyhow::ensure!(
            !state.active || !state.closed,
            "native media observer closed during window"
        );
        if self.kind == core_ffi::MediaKind::Video {
            return Ok(state
                .video
                .require(
                    &state.video_baseline,
                    generation,
                    (320, 180),
                    2,
                    Instant::now(),
                )
                .is_ok());
        }
        // Reuse the system PCM oracle: complete blocks, nontrivial signal,
        // same native generation and fresh, continuous 10 ms playout.
        Ok(state
            .pcm
            .require(generation, 2880, 1, None, Instant::now())
            .is_ok())
    }

    fn finish_window(&self, generation: u64) -> anyhow::Result<(u64, OutputFrame)> {
        if !self.ready(generation)? {
            let state = self.state.lock().unwrap();
            anyhow::bail!(
                "no qualified native {:?} media at window finish: generation={generation}, now={:?}, video={:?}, pcm={:?}",
                self.kind,
                Instant::now(),
                state.video,
                state.pcm,
            );
        }
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        let retained = state
            .retained
            .take()
            .ok_or_else(|| anyhow::anyhow!("no owning native frame"))?;
        state.active = false;
        Ok((state.blocks, retained))
    }
}

struct LatestData {
    sender: Arc<pulsebeam_agent_native::ffi::DataPublication>,
    receiver: Arc<pulsebeam_agent_native::ffi::DataSubscription>,
}

impl LatestData {
    fn new(agent: &Agent, room: &RoomId, remote: &str) -> anyhow::Result<Self> {
        Ok(Self {
            sender: agent.data("latest-state".into())?.publish(Some(
                core_ffi::DataPublishOptions {
                    delivery: Some(core_ffi::DataDelivery::Latest),
                    ..Default::default()
                },
            ))?,
            receiver: agent
                .room(room.as_str().into())?
                .participant(remote.into())?
                .data("latest-state".into())?
                .subscribe(Some(core_ffi::DataSubscribeOptions {
                    delivery: Some(core_ffi::DataDelivery::Latest),
                }))?,
        })
    }

    async fn exchange(
        &self,
        remote: &str,
        barrier: &tokio::sync::Barrier,
        marker: u8,
    ) -> anyhow::Result<String> {
        barrier.wait().await;
        let expected = [marker];
        let ready = async {
            loop {
                let update = self.receiver.next().await?;
                match update {
                    Some(pulsebeam_agent_core::DataEvent::Message {
                        participant_id,
                        publication_id,
                        data,
                        ..
                    }) => {
                        anyhow::ensure!(participant_id == remote);
                        if data == expected {
                            barrier.wait().await;
                            return Ok(publication_id);
                        }
                    }
                    None => {
                        anyhow::bail!("Latest cursor closed")
                    }
                    _ => {}
                }
            }
        };
        tokio::pin!(ready);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    result = &mut ready => break result,
                    _ = tick.tick() => self.sender.send(expected.to_vec(), None).await?,
                }
            }
        })
        .await?
    }

    async fn close(&self) -> anyhow::Result<()> {
        self.sender.close().await?;
        self.receiver.close().await?;
        Ok(())
    }
}

struct PeerReport {
    participant: String,
    reconnected: bool,
    first_generation: u64,
    final_generation: u64,
    video_frames: u64,
    audio_frames: u64,
}

#[derive(Clone)]
struct PeerBarriers {
    before_media: Arc<tokio::sync::Barrier>,
    before_reconnect: Arc<tokio::sync::Barrier>,
    after_reconnect: Arc<tokio::sync::Barrier>,
    before_close: Arc<tokio::sync::Barrier>,
}

struct OwnedData {
    sender: Arc<pulsebeam_agent_native::ffi::DataPublication>,
    room: Arc<pulsebeam_agent_native::ffi::DataSubscription>,
    participant: Arc<pulsebeam_agent_native::ffi::DataSubscription>,
    remote: String,
}

impl OwnedData {
    fn new(agent: &Agent, room: &RoomId, remote: String) -> anyhow::Result<Self> {
        let sender =
            agent
                .data("owned-bytes".into())?
                .publish(Some(core_ffi::DataPublishOptions {
                    delivery: Some(core_ffi::DataDelivery::Unreliable),
                    ..Default::default()
                }))?;
        Self::with_sender(agent, room, remote, sender)
    }

    fn with_sender(
        agent: &Agent,
        room: &RoomId,
        remote: String,
        sender: Arc<pulsebeam_agent_native::ffi::DataPublication>,
    ) -> anyhow::Result<Self> {
        let source = agent
            .room(room.as_str().into())?
            .participant(remote.clone())?
            .data("owned-bytes".into())?;
        let options = Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Unreliable),
        });
        let room = source.subscribe(options.clone())?;
        let participant = source.subscribe(options)?;
        Ok(Self {
            sender,
            room,
            participant,
            remote,
        })
    }

    async fn message(
        &self,
        cursor: &pulsebeam_agent_native::ffi::DataSubscription,
        expected: &[u8],
    ) -> anyhow::Result<pulsebeam_agent_core::DataEvent> {
        loop {
            let Some(event) = cursor.next().await? else {
                anyhow::bail!("owned data cursor closed before its message");
            };
            let pulsebeam_agent_core::DataEvent::Message {
                participant_id,
                publication_id,
                data,
                ..
            } = &event
            else {
                anyhow::bail!("owned data cursor lagged");
            };
            anyhow::ensure!(
                participant_id == &self.remote && !publication_id.is_empty(),
                "data cursor lost publisher identity"
            );
            if data == expected {
                return Ok(event);
            }
            anyhow::ensure!(data == &[0, 255, 128, 0], "unexpected binary payload");
        }
    }

    async fn exchange(
        &self,
        barrier: &tokio::sync::Barrier,
        round: u8,
    ) -> anyhow::Result<DataEvent> {
        barrier.wait().await;
        let payload = vec![0, 255, 128, round];
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            // Both cursors must have a live DATA v1 branch before the marker.
            let setup = vec![0, 255, 128, 0];
            let ready = async {
                let room = self.message(&self.room, &setup).await?;
                let participant = self.message(&self.participant, &setup).await?;
                anyhow::ensure!(room == participant, "independent setup cursors diverged");
                barrier.wait().await;
                anyhow::Ok(())
            };
            tokio::pin!(ready);
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    result = &mut ready => { result?; break; },
                    _ = tick.tick() => self.sender.send(setup.clone(), None).await?,
                }
            }
            self.sender.send(payload.clone(), None).await?;
            let room = self.message(&self.room, &payload).await?;
            let participant = self.message(&self.participant, &payload).await?;
            anyhow::ensure!(room == participant, "independent data cursors diverged");
            anyhow::Ok(room)
        })
        .await??;
        // Neither peer may replace its graph until the other has consumed this round.
        barrier.wait().await;
        Ok(received)
    }
}

#[test]
fn native_agents_prove_media_topics_reconnect_and_close() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse().expect("simulation seed must be a u64"))
        .unwrap_or(NATIVE_RUNTIME_SEED);
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let _ = stock_opus_fixture();
    let cluster_dir = tempfile::tempdir().expect("single-node cluster state directory");

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(90))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server_ip = subnet_ip(subnet, 1);
    let alice_ip = subnet_ip(subnet, 2);
    let bob_ip = subnet_ip(subnet, 3);

    let state = cluster_dir.path().to_owned();
    sim.host(server_ip, move || {
        let state = state.clone();
        async move {
            let gossip = "127.0.0.1:7946".parse().unwrap();
            sfu_node_builder(
                server_ip,
                pulsebeam_runtime::rand::seeded_rng(seed),
                DEFAULT_SIM_SHARDS,
            )
            .with_cluster(pulsebeam::cluster::Options {
                ownership_peers: Vec::new(),
                cluster_id: "native-reconnect".into(),
                topology: pulsebeam::cluster::Topology::SingleNode,
                listen: gossip,
                advertise: gossip,
                seeds: Vec::new(),
                data_dir: state,
            })
            .run(tokio_util::sync::CancellationToken::new())
            .await
            .map_err(Into::into)
        }
    });

    let (reports_tx, mut reports_rx) = tokio::sync::mpsc::channel(2);
    let barriers = PeerBarriers {
        before_media: Arc::new(tokio::sync::Barrier::new(2)),
        before_reconnect: Arc::new(tokio::sync::Barrier::new(2)),
        after_reconnect: Arc::new(tokio::sync::Barrier::new(2)),
        before_close: Arc::new(tokio::sync::Barrier::new(2)),
    };
    let alice_reports = reports_tx.clone();
    let alice_barriers = barriers.clone();
    sim.client(alice_ip, async move {
        let report = run_peer("alice", "bob", alice_ip, server_ip, false, alice_barriers).await?;
        alice_reports.send(report).await?;
        Ok(())
    });
    sim.client(bob_ip, async move {
        let report = run_peer("bob", "alice", bob_ip, server_ip, true, barriers).await?;
        reports_tx.send(report).await?;
        Ok(())
    });

    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = Arc::clone(&reports);
    sim.client(subnet_ip(subnet, 254), async move {
        for _ in 0..2 {
            let report = reports_rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("native peer report channel closed"))?;
            collected.lock().unwrap().push(report);
        }
        Ok(())
    });

    run_sim_or_timeout(&mut sim, Duration::from_secs(180)).expect("native vertical slice failed");
    let mut reports = reports.lock().unwrap();
    reports.sort_by(|left, right| left.participant.cmp(&right.participant));
    assert_eq!(reports.len(), 2);
    for report in reports.iter() {
        assert!(!report.participant.is_empty());
        if report.reconnected {
            assert_ne!(report.first_generation, report.final_generation);
        } else {
            assert_eq!(report.first_generation, report.final_generation);
        }
        assert!(report.video_frames > 0);
        assert!(report.audio_frames > 0);
    }
    // Stable semantic replay evidence, not ciphertext, timing-adaptive codec
    // bytes or an assertion of unavailable per-frame publication provenance.
    let semantics: Vec<_> = reports
        .iter()
        .map(|report| {
            serde_json::json!({
                "participant": report.participant,
                "reconnected": report.reconnected,
                "generation_changed": report.first_generation != report.final_generation,
                "same_peer_media_readded": true,
                "video_progress": report.video_frames > 0,
                "pcm_progress": report.audio_frames > 0,
                "owned_frames_and_close": true,
            })
        })
        .collect();
    println!("[native-lifecycle] {}", serde_json::Value::Array(semantics));
}

#[test]
fn native_joined_recovery_moves_to_a_distinct_controller_attempt() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse().expect("simulation seed must be a u64"))
        .unwrap_or(NATIVE_RUNTIME_SEED);
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(40))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let issuer_ip = subnet_ip(subnet, 1);
    let receiver_ip = subnet_ip(subnet, 2);
    let client_ip = subnet_ip(subnet, 3);
    let (issuer, receiver) = pulsebeam::node::SimOwnershipPeer::pair();
    let (issuer_tx, issuer_link) = pulsebeam_runtime::sync::oneshot::channel();
    let (receiver_tx, receiver_link) = pulsebeam_runtime::sync::oneshot::channel();
    for (ip, peer, node_seed, link) in [
        (issuer_ip, issuer.clone(), seed, issuer_tx),
        (receiver_ip, receiver.clone(), !seed, receiver_tx),
    ] {
        let link = Arc::new(Mutex::new(Some(link)));
        sim.host(ip, move || {
            let peer = peer.clone();
            let link = link.clone();
            async move {
                let link = link.lock().unwrap().take().unwrap();
                sfu_node_builder(ip, pulsebeam_runtime::rand::seeded_rng(node_seed), 1)
                    .with_sim_ownership_peer(peer)
                    .with_local_data_v1()
                    .with_link_control((ip, 0).into(), link)
                    .run(tokio_util::sync::CancellationToken::new())
                    .await
                    .map_err(Into::into)
            }
        });
    }
    let witness = Arc::new(Mutex::new(None));
    let client_witness = witness.clone();
    sim.client(client_ip, async move {
        *client_witness.lock().unwrap() = Some(
            prove_joined_recovery(
                client_ip,
                issuer_ip,
                receiver_ip,
                issuer,
                receiver,
                [issuer_link.await?, receiver_link.await?],
            )
            .await?,
        );
        Ok(())
    });
    use std::io::{Read, Seek};
    use tracing_subscriber::{Layer, layer::SubscriberExt};
    let mut logs = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(logs.try_clone().unwrap())
            .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                meta.target() == "pulsebeam::incident"
            })),
    );
    tracing::subscriber::with_default(subscriber, || {
        run_sim_or_timeout(&mut sim, Duration::from_secs(20))
    })
    .unwrap();
    logs.rewind().unwrap();
    let mut export = String::new();
    logs.read_to_string(&mut export).unwrap();
    let keys = witness.lock().unwrap().take().unwrap();
    assert!(
        !export.contains("native-failover")
            && !export.contains("alice")
            && !export.contains("viewer")
    );
    let records = export
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let matching = |name: &str, field: &str, value: &serde_json::Value| {
        records
            .iter()
            .any(|record| record["fields"]["event"] == name && &record["fields"][field] == value)
    };
    assert!(matching(
        "connection_ended",
        "winner",
        &keys["publisher_conn"]
    ));
    assert!(matching("publication_admitted", "pub", &keys["pub"]));
    assert!(matching("relay_installed", "pub", &keys["pub"]));
    assert!(matching("relay_removed", "pub", &keys["pub"]));
    assert!(matching("subscription_waiting", "conn", &keys["conn"]));
    assert!(matching("subscription_bound", "pub", &keys["pub"]));
    assert!(matching("credential_renewed", "jti", &keys["jti"]));
    println!(
        "[incident-witness] {}",
        serde_json::json!({"keys": keys, "records": records})
    );
}

async fn prove_joined_recovery(
    client_ip: IpAddr,
    issuer_ip: IpAddr,
    receiver_ip: IpAddr,
    issuer: pulsebeam::node::SimOwnershipPeer,
    receiver: pulsebeam::node::SimOwnershipPeer,
    [issuer_link, receiver_link]: [pulsebeam::link::Control; 2],
) -> anyhow::Result<serde_json::Value> {
    let room = "native-failover";
    let same_project = pulsebeam::entity::ProjectRoomKey {
        project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
        room: RoomId::new(room)?,
    };
    let unix_now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let token = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key().unwrap(),
        &serde_json::from_value(serde_json::json!([{"rooms":[room],"allow":["join","publish"]}]))?,
        "alice",
        unix_now() + 3600,
    )?
    .token;
    let config = Config::new(pulsebeam_agent_core::AgentConfig {
        endpoint: format!("http://{issuer_ip}:7070"),
        token,
        retry: Default::default(),
        log_level: Default::default(),
    });
    let evidence = Arc::new(Mutex::new(CreationEvidence::default()));
    let host = NativeHost::bind(client_ip)?;
    let subscriber_host = host.clone();
    let subscribe_token = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key().unwrap(),
        &serde_json::from_value(serde_json::json!([{"rooms":[room],"allow":["subscribe"]}]))?,
        "viewer",
        unix_now() + 3600,
    )?
    .token;
    let (subscriber, task) = RuntimeAgent::managed_with_services(
        Config::new(pulsebeam_agent_core::AgentConfig {
            endpoint: format!("http://{issuer_ip}:7070"),
            token: subscribe_token,
            retry: Default::default(),
            log_level: Default::default(),
        }),
        Host::new(create_http_client()),
        move || subscriber_host.runtime(),
    )?;
    tokio::task::spawn_local(task);
    let viewer = Agent::from_runtime(subscriber.clone());
    let feed = viewer
        .room(room.into())?
        .participant("alice".into())?
        .data("feed".into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
        }))?;
    let feed_updates = feed.updates()?;
    subscriber.connect().await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = feed_updates
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("feed closed before source joined"))?;
            if state.admission == core_ffi::PublicationAdmissionStatus::Admitted
                && !state.admission_pending
            {
                anyhow::ensure!(
                    state.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
                );
                anyhow::ensure!(state.source_track_id.is_none());
                return anyhow::Ok(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("roomless feed was not admitted while its source was absent"))??;
    let waiting = viewer.snapshot();
    anyhow::ensure!(waiting.room_id.is_none() && waiting.generation.is_some());
    let viewer_generation = waiting.generation;
    let viewer_support = viewer.support_report();
    let viewer_connection: pulsebeam_core::identity::ConnectionId = viewer_support
        .connection_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("admitted subscriber has no support key"))?
        .parse()?;
    let (runtime, actor) = RuntimeAgent::managed_with_services(
        config,
        Host::new(Box::new(LostCreationResponse {
            inner: create_http_client(),
            evidence: evidence.clone(),
            successor_receiver: Some(receiver_ip),
        })),
        move || host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime.clone());
    let publisher = agent
        .data("feed".into())?
        .publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
            ..Default::default()
        }))?;
    agent.join(room.into())?;
    let first = wait_connected_when(&agent, None, |_| true).await?;
    let first_support = agent.support_report();
    let first_connection: pulsebeam_core::identity::ConnectionId = first_support
        .connection_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("admitted publisher has no support key"))?
        .parse()?;
    anyhow::ensure!(viewer_connection != first_connection);
    use anyhow::Context as _;
    let initial_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = feed_updates
                .next()
                .await?
                .context("feed closed before initial installation")?;
            if state.execution == core_ffi::PublicationExecution::Installed {
                return state.source_track_id.context("initial DATA source missing");
            }
        }
    })
    .await
    .context("initial DATA receiver not installed")??;
    let receive_payload = |expected, expected_id| {
        let publisher = &publisher;
        let feed = &feed;
        async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut ticks = tokio::time::interval(Duration::from_millis(25));
                loop {
                    tokio::select! {
                        _ = ticks.tick() => publisher.send(Vec::from(expected), None).await?,
                        update = feed.next() => {
                            use pulsebeam_agent_core::DataEvent;
                            match update? {
                                Some(DataEvent::Message { publication_id, participant_id, data, .. })
                                    if data == expected => {
                                        anyhow::ensure!(publication_id == expected_id && participant_id == "alice");
                                        return anyhow::Ok(());
                                    }
                                None => anyhow::bail!("feed closed during ownership change"),
                                _ => {}
                            }
                        }
                    }
                }
            }).await.map_err(|_| anyhow::anyhow!(
                "did not receive payload for {expected_id}; subscription: {:?}", feed.state()
            ))?
        }
    };
    receive_payload(b"before".as_slice(), initial_id.as_str()).await?;
    issuer.deliver_receipt_facts(unix_now() + 20);
    runtime.reconnect().await?;
    let recovered = wait_connected_when(&agent, first.generation, |_| true).await?;
    let recovered_support = agent.support_report();
    let recovered_connection: pulsebeam_core::identity::ConnectionId = recovered_support
        .connection_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("replacement publisher has no support key"))?
        .parse()?;
    anyhow::ensure!(
        recovered_connection != first_connection && recovered_connection != viewer_connection
    );
    anyhow::ensure!(
        recovered.participant_id == first.participant_id && recovered.room_id == first.room_id
    );
    {
        let evidence = evidence.lock().unwrap();
        let created = http::StatusCode::CREATED;
        anyhow::ensure!(
            evidence.statuses == [created, created, http::StatusCode::GONE, created],
            "unexpected admission sequence: {:?}",
            evidence.statuses
        );
        anyhow::ensure!(
            evidence.requests[1] == evidence.requests[2],
            "wrong-target replay changed the pinned request"
        );
        anyhow::ensure!(
            evidence.requests[2].1 != evidence.requests[3].1,
            "receiving controller reused the issuer attempt"
        );
        let source = issuer_ip.to_string();
        let destination = receiver_ip.to_string();
        anyhow::ensure!(
            evidence.posts[1].uri().host() == Some(source.as_str())
                && evidence.posts[2].uri().host() == Some(destination.as_str())
        );
        let receipt = |body: &[u8]| {
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["successor_receipt"].clone()
        };
        anyhow::ensure!(!receipt(&evidence.requests[1].0).is_null());
        anyhow::ensure!(receipt(&evidence.requests[1].0) == receipt(&evidence.requests[3].0));
    }
    anyhow::ensure!(
        issuer.local_claim_count().await? == 1 && receiver.local_claim_count().await? == 1
    );
    let [ingress, egress] = link_recovery_peers(&issuer_link, &receiver_link).await?;
    let until = pulsebeam_runtime::time::Instant::now() + Duration::from_secs(4);
    let (replacement, first_label) = receiver
        .relay_fabric_data(egress, ingress, recovered_connection, until)
        .await?;
    let recovered_id = replacement.as_str();
    anyhow::ensure!(recovered_id != initial_id);
    receive_payload(b"after".as_slice(), recovered_id.as_str()).await?;
    let bound = feed.state();
    anyhow::ensure!(bound.source_track_id.as_deref() == Some(recovered_id.as_str()));

    tokio::time::timeout_at(until + Duration::from_secs(2), async {
        loop {
            let state = feed_updates
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("feed closed when peer evidence expired"))?;
            if state.observation_revision > bound.observation_revision
                && state.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
            {
                anyhow::ensure!(
                    state.source_track_id.is_none()
                        && state.accepted_revision == bound.accepted_revision
                        && state.admission == core_ffi::PublicationAdmissionStatus::Admitted
                );
                return anyhow::Ok(());
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "expired peer source was not withdrawn; subscription: {:?}",
            feed.state()
        )
    })??;
    let withdrawn = issuer_link.destinations(same_project, replacement).await;
    anyhow::ensure!(
        matches!(withdrawn, Err(pulsebeam_link::BindError::Closed)),
        "expired publication is still visible to Link: {withdrawn:?}"
    );
    let (refreshed, new_label) = receiver
        .relay_fabric_data(
            egress,
            ingress,
            recovered_connection,
            pulsebeam_runtime::time::Instant::now() + Duration::from_secs(10),
        )
        .await?;
    anyhow::ensure!(refreshed == replacement && first_label != new_label);
    receive_payload(b"refreshed".as_slice(), recovered_id.as_str()).await?;
    anyhow::ensure!(feed.state().accepted_revision == bound.accepted_revision);
    anyhow::ensure!(viewer.snapshot().generation == viewer_generation && !feed.is_closed());
    anyhow::ensure!(viewer.support_report() == viewer_support);
    anyhow::ensure!(
        issuer.local_claim_count().await? == 0 && receiver.local_claim_count().await? == 1,
        "sibling claims did not converge"
    );
    anyhow::ensure!(
        agent.snapshot().generation == recovered.generation
            && agent.snapshot().connection == core_ffi::ConnectionState::Connected
    );
    let renewal = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key().unwrap(),
        &serde_json::from_value(serde_json::json!([{"rooms":[room],"allow":["subscribe"]}]))?,
        "viewer",
        unix_now() + 7200,
    )?;
    let previous_credential = viewer.snapshot().accepted_credential_revision;
    let snapshots = viewer.snapshots();
    viewer.renew(renewal.token)?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("subscriber snapshot stream closed during renewal");
            };
            if snapshot.accepted_credential_revision > previous_credential {
                return anyhow::Ok(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("subscriber credential renewal was not accepted"))??;
    anyhow::ensure!(viewer.support_report() == viewer_support);
    viewer.close().await?;
    agent.close().await?;
    Ok(serde_json::json!({
        "project": same_project.project.to_string(),
        "conn": viewer_connection.to_string(),
        "publisher_conn": recovered_connection.to_string(),
        "pub": recovered_id,
        "jti": renewal.jti.to_string(),
    }))
}

fn relay_error(error: impl std::fmt::Debug) -> anyhow::Error {
    anyhow::anyhow!("Link relay provisioning: {error:?}")
}

async fn link_recovery_peers(
    issuer: &pulsebeam::link::Control,
    receiver: &pulsebeam::link::Control,
) -> anyhow::Result<[pulsebeam_link::NetworkId; 2]> {
    use pulsebeam_link::{FabricPath, NetworkClass, NetworkPeer, PeerRole, SecretCredential};
    let path = FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 0,
    };
    let left = SecretCredential::generate().map_err(relay_error)?;
    let right = SecretCredential::generate().map_err(relay_error)?;
    let left_fingerprint = left.fingerprint();
    let right_fingerprint = right.fingerprint();
    let local = issuer.health().await.map_err(relay_error)?.local_addr;
    let remote = receiver.health().await.map_err(relay_error)?.local_addr;
    let egress = receiver
        .open_peer(
            NetworkPeer {
                endpoint: local,
                credential: right,
                expected_peer: left_fingerprint,
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
            }
            .with_fabric_path(path),
        )
        .await
        .map_err(relay_error)?;
    let installed = receiver.health().await.map_err(relay_error)?;
    anyhow::ensure!(
        installed
            .peers
            .iter()
            .any(|peer| peer.peer == egress && peer.failure.is_none()),
        "relay responder was not installed"
    );
    let ingress = issuer
        .open_peer(
            NetworkPeer {
                endpoint: remote,
                credential: left,
                expected_peer: right_fingerprint,
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            }
            .with_fabric_path(path),
        )
        .await
        .map_err(relay_error)?;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let mut ready = true;
            for (node, id) in [(issuer, ingress), (receiver, egress)] {
                let health = node.health().await.map_err(relay_error)?;
                if let Some(peer) = health.peers.iter().find(|peer| peer.peer == id) {
                    anyhow::ensure!(
                        peer.failure.is_none(),
                        "relay peer failed: {:?}",
                        peer.failure
                    );
                    ready &= peer.transport.ready;
                } else {
                    ready = false;
                }
            }
            if ready {
                return anyhow::Ok([ingress, egress]);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("authenticated Link peers did not become ready"))?
}

#[test]
fn native_roomless_reception_survives_join_and_leave() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse().expect("simulation seed must be a u64"))
        .unwrap_or(NATIVE_RUNTIME_SEED);
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server_ip = subnet_ip(subnet, 1);
    let client_ip = subnet_ip(subnet, 2);
    sim.host(server_ip, move || async move {
        sfu_node_builder(
            server_ip,
            pulsebeam_runtime::rand::seeded_rng(seed),
            DEFAULT_SIM_SHARDS,
        )
        .run(tokio_util::sync::CancellationToken::new())
        .await
        .map_err(Into::into)
    });
    sim.client(client_ip, async move {
        roomless_presence_scenario(client_ip, server_ip)
            .await
            .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(30))
        .expect("roomless presence simulation failed");
}

async fn roomless_presence_scenario(client_ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<()> {
    use core_ffi::{ConnectionState, PublicationAdmissionStatus as Admission};
    let host = NativeHost::bind(client_ip)?;
    let make_agent = |participant: &str, grants: serde_json::Value| -> anyhow::Result<_> {
        let key = pulsebeam_server::insecure_dev_api_key().unwrap();
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 60;
        let token = pulsebeam_server::sign_participant_token(
            &key,
            &serde_json::from_value(grants)?,
            participant,
            expiry,
        )?
        .token;
        let config = Config::new(pulsebeam_agent_core::AgentConfig {
            endpoint: format!("http://{server_ip}:7070"),
            token,
            retry: Default::default(),
            log_level: Default::default(),
        });
        let host = host.clone();
        let (runtime, actor) = RuntimeAgent::managed_with_services(
            config,
            Host::new(create_http_client()),
            move || host.runtime(),
        )?;
        tokio::task::spawn_local(actor);
        Ok((Agent::from_runtime(runtime.clone()), runtime))
    };
    let (publisher, _) = make_agent(
        "present-publisher",
        serde_json::json!([{"rooms":["stage"],"allow":["join","publish"]}]),
    )?;
    let (viewer, viewer_transport) = make_agent(
        "presence-viewer",
        serde_json::json!([
            {"rooms":["stage"],"allow":["discover","subscribe"]},
            {"rooms":["lobby"],"allow":["join","publish"]},
            {"rooms":["listen"],"allow":["join"]}
        ]),
    )?;
    let (replacement_viewer, replacement_transport) = make_agent(
        "known-address-viewer",
        serde_json::json!([{"rooms":["stage"],"allow":["subscribe"]}]),
    )?;
    let replacement_receiver = replacement_viewer
        .room("stage".into())?
        .participant("present-publisher".into())?
        .data("replacement".into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Unreliable),
        }))?;
    replacement_transport.connect().await?;
    let updates = &replacement_receiver.updates()?;
    let await_source = |present| async move {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let state = updates
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("logical data subscription closed"))?;
                anyhow::ensure!(state.terminal_error.is_none());
                if state.admission == Admission::Admitted
                    && !state.admission_pending
                    && if present {
                        state.execution == core_ffi::PublicationExecution::Installed
                    } else {
                        state.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
                    }
                {
                    return anyhow::Ok(state.source_track_id);
                }
            }
        })
        .await?
    };
    anyhow::ensure!(await_source(false).await?.is_none());
    let replacement_generation = replacement_viewer.snapshot().generation;
    anyhow::ensure!(replacement_generation.is_some());

    let sender = publisher
        .data("events".into())?
        .publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
            ..Default::default()
        }))?;
    publisher.join("stage".into())?;
    let sender_updates = sender.updates()?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = sender_updates
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("DATA v1 publisher observation closed"))?;
            anyhow::ensure!(
                state.error.is_none() && state.execution_error.is_none(),
                "DATA v1 publisher failed: {state:?}"
            );
            if state.admission == Admission::Admitted
                && state.execution == core_ffi::PublicationExecution::Installed
            {
                return anyhow::Ok(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("DATA v1 publisher installation timed out"))??;

    let replacement_topic = publisher.data("replacement".into())?;
    let mut first_source = None;
    for value in [b"original", b"replaced"] {
        let source = replacement_topic.publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(core_ffi::DataDelivery::Unreliable),
            ..Default::default()
        }))?;
        let id = await_source(true)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing installed publication ID"))?;
        source.send(value.to_vec(), None).await?;
        let mut update =
            tokio::time::timeout(Duration::from_secs(3), replacement_receiver.next()).await??;
        if let Some(previous) = &first_source {
            anyhow::ensure!(
                matches!(update, Some(pulsebeam_agent_core::DataEvent::Discontinuity {
                previous_publication_id, publication_id, participant_id,
            }) if &previous_publication_id == previous && publication_id == id && participant_id == "present-publisher")
            );
            update =
                tokio::time::timeout(Duration::from_secs(3), replacement_receiver.next()).await??;
        }
        anyhow::ensure!(
            matches!(update, Some(pulsebeam_agent_core::DataEvent::Message { publication_id, participant_id, data, .. })
            if publication_id == id && participant_id == "present-publisher" && data == value)
        );
        anyhow::ensure!(first_source.as_ref() != Some(&id));
        first_source = Some(id);
        source.close().await?;
        anyhow::ensure!(await_source(false).await?.is_none());
        anyhow::ensure!(!replacement_receiver.is_closed());
        anyhow::ensure!(replacement_viewer.snapshot().generation == replacement_generation);
    }
    replacement_viewer.close().await?;

    let watch = viewer.room("stage".into())?.watch(None)?;
    let watch_updates = watch.updates()?;
    let receiver = viewer
        .room("stage".into())?
        .participant("present-publisher".into())?
        .data("events".into())?
        .subscribe(None)?;
    let local = viewer
        .audio("microphone".into())?
        .publish(Default::default())?;
    let local_updates = local.updates()?;
    viewer_transport.connect().await?;
    let mut previous = None;
    let mut source = None;
    let mut local_id = None;
    for (room, payload) in [
        (None, b"roomless".as_slice()),
        (Some("lobby"), b"joined"),
        (None, b"left"),
        (Some("listen"), b"restricted"),
        (Some("lobby"), b"rejoined"),
    ] {
        if previous.is_some() {
            if let Some(room) = room {
                viewer.join(room.into())?;
            } else {
                viewer_transport.leave_room()?;
            }
            anyhow::ensure!(
                viewer.snapshot().room_id.is_none(),
                "old presence remained visible"
            );
        }
        let active = wait_connected_when(&viewer, previous, |_| true).await?;
        anyhow::ensure!(active.room_id.as_deref() == room);
        previous = active.generation;
        match room {
            Some("lobby") => {
                let admitted = wait_publication_admitted(&local_updates).await?;
                let id = admitted
                    .admission
                    .publication_id
                    .ok_or_else(|| anyhow::anyhow!("admitted publication has no ID"))?;
                anyhow::ensure!(
                    local_id.as_ref() != Some(&id),
                    "rejoin reused the retired publication ID"
                );
                local_id = Some(id);
            }
            Some("listen") => {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let state = local_updates
                            .next()
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("publication observation closed"))?;
                        if state.admission.status == Admission::Rejected {
                            anyhow::ensure!(
                                state.admission.publication_id.is_none(),
                                "denied Publish produced an ID"
                            );
                            break anyhow::Ok(());
                        }
                    }
                })
                .await??;
            }
            _ => anyhow::ensure!(local.state().admission.status == Admission::Pending),
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let state = watch_updates
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("watch closed"))?;
                let Some(participants) = state.participants else {
                    continue;
                };
                if participants.iter().any(|entry| {
                    entry.id() == "present-publisher"
                        && entry
                            .tracks()
                            .iter()
                            .any(|track| track.track_id == "events")
                }) {
                    anyhow::ensure!(
                        participants.len() == 1,
                        "viewer leaked presence into the source room"
                    );
                    break anyhow::Ok(());
                }
            }
        })
        .await??;
        let publication = tokio::time::timeout(Duration::from_secs(3), async {
            let mut interval = tokio::time::interval(Duration::from_millis(25));
            loop {
                tokio::select! {
                    _ = interval.tick() => sender.send(payload.to_vec(), None).await?,
                    update = receiver.next() => match update? {
                        Some(pulsebeam_agent_core::DataEvent::Message {
                            participant_id, publication_id, data, ..
                        }) if data == payload => {
                            anyhow::ensure!(participant_id == "present-publisher");
                            return anyhow::Ok(publication_id);
                        }
                        None => anyhow::bail!("receiver closed across presence change"),
                        _ => {}
                    }
                }
            }
        })
        .await??;
        if let Some(expected) = &source {
            anyhow::ensure!(*expected == publication);
        }
        source = Some(publication);
    }
    viewer.join("stage".into())?;
    let snapshots = viewer.snapshots();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("viewer observation closed");
            };
            if snapshot.connection == ConnectionState::TerminalFailure {
                anyhow::ensure!(snapshot.room_id.is_none(), "denied Join exposed presence");
                anyhow::ensure!(
                    snapshot.failure.is_some_and(
                        |failure| failure.class == core_ffi::FailureClass::Authorization
                    )
                );
                break anyhow::Ok(());
            }
        }
    })
    .await??;
    viewer.close().await?;
    publisher.close().await?;
    Ok(())
}

async fn run_peer(
    name: &str,
    remote: &str,
    ip: IpAddr,
    server_ip: IpAddr,
    reconnect: bool,
    barriers: PeerBarriers,
) -> anyhow::Result<PeerReport> {
    let endpoint = format!("http://{server_ip}:7070");
    let room = RoomId::new("native-vertical")?;
    let participant = ParticipantId::new(name)?;
    let session = pulsebeam_agent_core::AgentConfig {
        endpoint,
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &serde_json::from_value(serde_json::json!([{"rooms":[(room).as_str()],"allow":["join","publish","subscribe"]}])).unwrap(),
            (participant).as_str(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                .saturating_add(3600),
        )?.token,
        retry: pulsebeam_agent_core::RetryPolicy::default(),
        log_level: pulsebeam_agent_core::LogLevel::default(),
    };
    let mut config = Config::new(session);
    config.media.audio_input = AudioInput::EncodedOpus { channels: 2 };
    config.media.video_input = VideoInput::EncodedH264;
    let host = NativeHost::bind(ip)?;
    let created = Rc::new(Cell::new(0_usize));
    let retired = Rc::new(Cell::new(0_usize));
    let creation_evidence = Arc::new(Mutex::new(CreationEvidence::default()));
    let http: Box<dyn AsyncHttpClient> = if reconnect {
        Box::new(LostCreationResponse {
            inner: create_http_client(),
            evidence: creation_evidence.clone(),
            successor_receiver: None,
        })
    } else {
        create_http_client()
    };
    let successor_config = config.clone();
    let (runtime, actor) = RuntimeAgent::managed_with_services(config, Host::new(http), {
        let created = created.clone();
        let retired = retired.clone();
        let host = host.clone();
        move || {
            let (runtime, owner) = host.runtime()?;
            created.set(created.get().saturating_add(1));
            Ok((runtime, (owner, RuntimeRetirement(retired.clone()))))
        }
    })?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime.clone());
    let mut video = agent.create_video_input(Default::default()).await?;
    let mut audio = agent.create_audio_input().await?;
    let mut video_owner = agent.video("camera".into())?.publish(Default::default())?;
    let initial_video_pipe = video.pipe_to(&video_owner)?;
    let mut audio_owner = agent
        .audio("microphone".into())?
        .publish(Default::default())?;
    let _audio_pipe = audio.pipe_to(&audio_owner)?;
    initial_video_pipe.close().await?;
    let video_updates = video_owner.updates()?;
    let pending_video = video_updates
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("publication observation ended before join"))?;
    anyhow::ensure!(
        pending_video.admission.status == core_ffi::PublicationAdmissionStatus::Pending
            && !pending_video.input_attached
            && !pending_video.forwarding_requested,
        "detached publication claimed admission or forwarding before join"
    );
    let latest_data = LatestData::new(&agent, &room, remote)?;
    let owned_data = OwnedData::new(&agent, &room, remote.to_owned())?;
    runtime.join(room.as_str().to_owned())?;

    let connected = wait_connected(&agent, None).await?;
    if reconnect {
        let evidence = creation_evidence.lock().unwrap();
        anyhow::ensure!(
            evidence.dropped && evidence.requests.len() == 2,
            "lost successful creation did not produce exactly one retry"
        );
        anyhow::ensure!(
            evidence.requests[0] == evidence.requests[1],
            "creation retry changed the captured body, proof or sequence"
        );
        anyhow::ensure!(
            evidence.locations.len() == 2 && evidence.locations[0] == evidence.locations[1],
            "creation retry did not recover the same admitted connection"
        );
    }
    let generation = connected
        .generation
        .ok_or_else(|| anyhow::anyhow!("connected without generation"))?;
    audio_owner.ready().await?;
    let admitted_video = wait_publication_admitted(&video_updates).await?;
    anyhow::ensure!(
        admitted_video.admission.publication_id.is_some()
            && admitted_video.admission.demand == core_ffi::PublicationDemand::Idle
            && !admitted_video.input_attached
            && !admitted_video.forwarding_requested
            && admitted_video.configuration.enabled
            && !video.is_closed(),
        "input-free publication was not admitted idle before the first frame"
    );
    let _video_pipe = video.pipe_to(&video_owner)?;
    video_owner.ready().await?;
    anyhow::ensure!(
        video_owner.state().admission.publication_id == admitted_video.admission.publication_id,
        "attaching input replaced the admitted publication"
    );
    barriers.before_media.wait().await;
    let mut frame_cursor = 0;

    let video_subscription = agent
        .room(room.as_str().to_owned())?
        .participant(remote.to_owned())?
        .video("camera".into())?
        .subscribe(Some(core_ffi::VideoObservation {
            visible: true,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 1,
            playout_delay: core_ffi::PlayoutDelay::Adaptive,
        }))?;
    let audio_subscription = agent
        .room(room.as_str().to_owned())?
        .participant(remote.to_owned())?
        .audio("microphone".into())?
        .subscribe(Default::default())?;
    let (initial, remote_video_id, remote_audio_id) = wait_remote_media(
        &agent,
        &video_subscription,
        &audio_subscription,
        &video,
        &mut frame_cursor,
    )
    .await?;
    barriers.before_media.wait().await;
    let participant = initial
        .participant_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a participant ID"))?;
    let first_generation = initial
        .generation
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a generation"))?;
    wait_media_bindings(
        &agent,
        &video_subscription,
        &audio_subscription,
        &remote_video_id,
        &remote_audio_id,
    )
    .await?;
    let mut remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(remote_video_id.as_str()),
        "named video lost its publication association"
    );
    let mut remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_audio.source_track_id().as_deref() == Some(remote_audio_id.as_str()),
        "named audio lost its publication association"
    );
    let mut video_probe =
        MediaProbe::start(remote_video.clone(), generation, core_ffi::MediaKind::Video);
    let mut audio_probe =
        MediaProbe::start(remote_audio.clone(), generation, core_ffi::MediaKind::Audio);
    // The two Lossy cursors share one subscription stream.
    native_payload_channels(&runtime, first_generation, 4, None).await?;
    let old_latest = latest_data
        .exchange(remote, &barriers.before_media, 1)
        .await?;
    owned_data.exchange(&barriers.before_media, 1).await?;

    let accepted = agent.snapshot();
    let errors = agent.events();
    // Backend rejection is per resource; unrelated accepted video stays desired.
    let fixed = core_ffi::PlayoutDelay::Fixed {
        min_ms: 100,
        max_ms: 100,
    };
    audio_subscription
        .configure(core_ffi::AudioSubscriptionConfig {
            playout_delay: fixed,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match errors.next().await {
                EventUpdate::Event {
                    event:
                        NativeEvent::Core {
                            notification: core_ffi::Notification::ServerError { .. },
                        },
                } => return anyhow::Ok(()),
                EventUpdate::Event { .. } => {}
                EventUpdate::Lagged { skipped } => {
                    anyhow::bail!("rejection observation lost {skipped} events")
                }
                EventUpdate::Closed => anyhow::bail!("rejection observation closed"),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("unsupported fixed audio was not rejected"))??;
    let rejected = agent.snapshot();
    anyhow::ensure!(
        rejected.desired_revision == accepted.desired_revision && !audio_subscription.is_closed(),
        "owned audio configuration changed the declaration revision or ended its parent"
    );
    anyhow::ensure!(
        rejected.generation == accepted.generation
            && rejected.connection == core_ffi::ConnectionState::Connected,
        "unsupported audio reset the established session"
    );
    let unchanged_video = video_subscription.state();
    anyhow::ensure!(
        unchanged_video.source_status == core_ffi::SubscriptionSourceStatus::Available
            && unchanged_video.source_track_id.as_deref() == Some(remote_video_id.as_str())
            && unchanged_video.admission == core_ffi::PublicationAdmissionStatus::Admitted
            && unchanged_video.execution == core_ffi::PublicationExecution::Installed,
        "unsupported audio disturbed unchanged video binding"
    );
    // The existing exchange must decode fresh audio and video after rejection.
    let first = exchange(
        &barriers.before_reconnect,
        &agent,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        1,
        &mut frame_cursor,
    )
    .await?;
    let (original_payloads, closed_before) =
        native_payload_channels(&runtime, first_generation, 4, None).await?;
    barriers.before_reconnect.wait().await;
    latest_data.close().await?;
    let closed_after = closed_before
        .checked_add(2)
        .ok_or_else(|| anyhow::anyhow!("native closed-channel counter overflow"))?;
    let (continuing, _) =
        native_payload_channels(&runtime, first_generation, 2, Some(closed_after)).await?;
    for (stream, label) in &continuing {
        anyhow::ensure!(
            original_payloads.get(stream) == Some(label),
            "continuing payload was replaced"
        );
    }
    barriers.before_reconnect.wait().await;
    let latest_data = LatestData::new(&agent, &room, remote)?;
    let (reused, _) =
        native_payload_channels(&runtime, first_generation, 4, Some(closed_after)).await?;
    let new_latest = latest_data
        .exchange(remote, &barriers.before_reconnect, 2)
        .await?;
    anyhow::ensure!(
        new_latest != old_latest,
        "retired Latest source identity was revived"
    );
    anyhow::ensure!(
        original_payloads.keys().eq(reused.keys()),
        "payload stream IDs grew instead of being reused"
    );
    for (stream, label) in &reused {
        if continuing.contains_key(stream) {
            anyhow::ensure!(
                original_payloads.get(stream) == Some(label),
                "continuing payload identity changed"
            );
        } else {
            anyhow::ensure!(
                original_payloads.get(stream) != Some(label),
                "reused stream retained an old physical identity"
            );
        }
    }
    anyhow::ensure!(
        agent.snapshot().generation == Some(first_generation)
            && created.get() == 1
            && retired.get() == 0,
        "payload reuse replaced the native peer"
    );
    audio_subscription
        .configure(core_ffi::AudioSubscriptionConfig {
            playout_delay: core_ffi::PlayoutDelay::Adaptive,
        })
        .await?;

    let first_expected = first.retained.clone();
    let retired_video_input = video.clone();
    let retired_audio_input = audio.clone();
    let retired_video_observation = remote_video.clone();
    let retired_audio_observation = remote_audio.clone();
    video_owner.close().await?;
    let closed_video = video_updates
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("publication observation lost its final state"))?;
    anyhow::ensure!(
        closed_video.closed && video_updates.next().await?.is_none(),
        "publication close did not expose a final state and end its cursor"
    );
    audio_owner.close().await?;
    anyhow::ensure!(
        !video.is_closed() && !audio.is_closed(),
        "publication retirement stopped independent producers"
    );
    video.close().await?;
    audio.close().await?;
    anyhow::ensure!(
        video.is_closed() && audio.is_closed(),
        "explicit producer retirement retained an input"
    );
    barriers.before_reconnect.wait().await;
    wait_media_absent(&video_subscription, &audio_subscription).await?;
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed)
            && matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "retired source observation stayed open"
    );
    anyhow::ensure!(
        !video_subscription.is_closed() && !audio_subscription.is_closed(),
        "source absence ended a logical media interest"
    );
    let absent = agent.snapshot();
    anyhow::ensure!(
        absent.generation == Some(first_generation)
            && absent.participant_id.as_deref() == Some(participant.as_str())
            && absent.connection == core_ffi::ConnectionState::Connected,
        "media retirement replaced the established session"
    );
    barriers.before_reconnect.wait().await;
    video = agent.create_video_input(Default::default()).await?;
    audio = agent.create_audio_input().await?;
    video_owner = agent.video("camera".into())?.publish(Default::default())?;
    let video_pipe = video.pipe_to(&video_owner)?;
    audio_owner = agent
        .audio("microphone".into())?
        .publish(Default::default())?;
    let audio_pipe = audio.pipe_to(&audio_owner)?;
    video_owner.ready().await?;
    audio_owner.ready().await?;
    barriers.before_reconnect.wait().await;
    let (_, readded_video_id, readded_audio_id) = wait_remote_media(
        &agent,
        &video_subscription,
        &audio_subscription,
        &video,
        &mut frame_cursor,
    )
    .await?;
    anyhow::ensure!(
        readded_video_id != remote_video_id && readded_audio_id != remote_audio_id,
        "re-added media reused a retired source identity"
    );
    let readded = wait_media_bindings(
        &agent,
        &video_subscription,
        &audio_subscription,
        &readded_video_id,
        &readded_audio_id,
    )
    .await?;
    let replaced_video_input = video;
    let replaced_audio_input = audio;
    video = agent.create_video_input(Default::default()).await?;
    audio = agent.create_audio_input().await?;
    video_pipe.close().await?;
    audio_pipe.close().await?;
    let _replacement_video_pipe = video.pipe_to(&video_owner)?;
    let _replacement_audio_pipe = audio.pipe_to(&audio_owner)?;
    video_owner.ready().await?;
    audio_owner.ready().await?;
    anyhow::ensure!(
        !replaced_video_input.is_closed() && !replaced_audio_input.is_closed(),
        "source replacement stopped borrowed producers"
    );
    replaced_video_input.close().await?;
    replaced_audio_input.close().await?;
    anyhow::ensure!(
        replaced_video_input.is_closed() && replaced_audio_input.is_closed(),
        "explicit old-producer retirement failed"
    );
    anyhow::ensure!(
        readded.generation == Some(first_generation)
            && readded.participant_id.as_deref() == Some(participant.as_str())
            && readded.connection == core_ffi::ConnectionState::Connected,
        "media re-addition replaced the established session"
    );
    remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(readded_video_id.as_str())
            && remote_audio.source_track_id().as_deref() == Some(readded_audio_id.as_str()),
        "re-added observations lost their exact-source association"
    );
    video_probe = MediaProbe::start(
        remote_video.clone(),
        first_generation,
        core_ffi::MediaKind::Video,
    );
    audio_probe = MediaProbe::start(
        remote_audio.clone(),
        first_generation,
        core_ffi::MediaKind::Audio,
    );
    let same_peer = exchange(
        &barriers.before_reconnect,
        &agent,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        2,
        &mut frame_cursor,
    )
    .await?;
    let original_owned = owned_data.exchange(&barriers.before_reconnect, 2).await?;
    let same_peer_expected = same_peer.retained.clone();
    anyhow::ensure!(
        agent.snapshot().generation == Some(first_generation)
            && created.get() == 1
            && retired.get() == 0,
        "same-peer media exchange reconstructed its native graph"
    );
    anyhow::ensure!(
        matches!(
            retired_video_observation.next().await,
            MediaStreamUpdate::Closed
        ) && matches!(
            retired_audio_observation.next().await,
            MediaStreamUpdate::Closed
        ),
        "retired exact-source observations rebound to re-added media"
    );
    anyhow::ensure!(
        matches!(
            retired_video_input.submit(video_frame(0)).await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ) && matches!(
            retired_audio_input
                .submit(InputFrame::Opus {
                    frame: stock_opus_fixture()[0].clone()
                })
                .await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ),
        "a retired input submitted to re-added media"
    );
    anyhow::ensure!(
        matches!(
            replaced_video_input.submit(video_frame(0)).await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ) && matches!(
            replaced_audio_input
                .submit(InputFrame::Opus {
                    frame: stock_opus_fixture()[0].clone()
                })
                .await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ),
        "replaced input submitted after its same-generation replacement became current"
    );
    anyhow::ensure!(
        first.retained == first_expected,
        "returned owning frames changed across same-peer re-addition"
    );
    let (final_generation, replacement) = if reconnect {
        runtime.reconnect().await?;
        let connected = wait_connected(&agent, Some(first_generation)).await?;
        let generation = connected
            .generation
            .ok_or_else(|| anyhow::anyhow!("reconnected without generation"))?;
        anyhow::ensure!(
            !video.is_closed()
                && !audio.is_closed()
                && !video_subscription.is_closed()
                && !audio_subscription.is_closed(),
            "generation retirement ended logical inputs or media interests"
        );
        video_owner.ready().await?;
        audio_owner.ready().await?;
        let replacement = wait_remote_media(
            &agent,
            &video_subscription,
            &audio_subscription,
            &video,
            &mut frame_cursor,
        )
        .await?;
        if replacement.0.participant_id.as_deref() != Some(participant.as_str()) {
            anyhow::bail!("{name} participant identity changed across reconnect");
        }
        let replacement_generation = replacement
            .0
            .generation
            .ok_or_else(|| anyhow::anyhow!("{name} replacement has no generation"))?;
        anyhow::ensure!(
            replacement_generation == generation,
            "replacement media escaped the connected generation"
        );
        (replacement_generation, Some(replacement))
    } else {
        (first_generation, None)
    };
    barriers.after_reconnect.wait().await;
    if reconnect {
        let evidence = creation_evidence.lock().unwrap();
        anyhow::ensure!(
            evidence.requests.len() == 3,
            "reconnect did not submit exactly one new creation"
        );
        anyhow::ensure!(
            evidence.requests[2].1 == evidence.requests[0].1
                && evidence.requests[2].2 != evidence.requests[0].2,
            "reconnect did not retain its owner proof with a new sequence"
        );
    }
    anyhow::ensure!(
        created.get() == usize::from(reconnect).saturating_add(1)
            && retired.get() == usize::from(reconnect),
        "reconstruction did not retire exactly its previous runtime owner"
    );
    let (_, expected_video_id, expected_audio_id) = match replacement {
        Some(media) => media,
        None => {
            wait_remote_media(
                &agent,
                &video_subscription,
                &audio_subscription,
                &video,
                &mut frame_cursor,
            )
            .await?
        }
    };
    wait_media_bindings(
        &agent,
        &video_subscription,
        &audio_subscription,
        &expected_video_id,
        &expected_audio_id,
    )
    .await?;
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed),
        "retired video observer stayed open"
    );
    anyhow::ensure!(
        matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "retired audio observer stayed open"
    );
    video_probe.ready(first_generation)?;
    audio_probe.ready(first_generation)?;
    remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(expected_video_id.as_str()),
        "fresh named video observation has the wrong source"
    );
    remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_audio.source_track_id().as_deref() == Some(expected_audio_id.as_str()),
        "fresh named audio observation has the wrong source"
    );
    video_probe = MediaProbe::start(
        remote_video.clone(),
        final_generation,
        core_ffi::MediaKind::Video,
    );
    audio_probe = MediaProbe::start(
        remote_audio.clone(),
        final_generation,
        core_ffi::MediaKind::Audio,
    );
    barriers.after_reconnect.wait().await;
    native_payload_channels(&runtime, final_generation, 4, None)
        .await
        .with_context(|| {
            format!(
                "{name} replacement payload lanes; owned-bytes subscriber: {:?}; publisher: {:?}",
                owned_data.participant.state(),
                owned_data.sender.state(),
            )
        })?;
    latest_data
        .exchange(remote, &barriers.after_reconnect, 3)
        .await?;

    anyhow::ensure!(
        first.retained == first_expected,
        "returned frames changed across reconnect"
    );
    let second = exchange(
        &barriers.before_close,
        &agent,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        3,
        &mut frame_cursor,
    )
    .await?;
    if reconnect {
        owned_data.exchange(&barriers.before_close, 2).await?;
    } else {
        let replacement =
            OwnedData::with_sender(&agent, &room, remote.into(), owned_data.sender.clone())?;
        let fresh = replacement.exchange(&barriers.before_close, 2).await?;
        let (
            DataEvent::Message {
                publication_id: previous,
                ..
            },
            DataEvent::Message {
                publication_id: current,
                ..
            },
        ) = (&original_owned, &fresh)
        else {
            anyhow::bail!("expected message evidence for both DATA source incarnations");
        };
        anyhow::ensure!(
            previous != current,
            "reconnect reused the retired Lossy source"
        );
        let expected_change = DataEvent::Discontinuity {
            participant_id: remote.into(),
            previous_publication_id: previous.clone(),
            publication_id: current.clone(),
        };
        for retained in [&owned_data.room, &owned_data.participant] {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match retained.next().await? {
                        Some(event) if event == expected_change => break,
                        Some(DataEvent::Message {
                            publication_id,
                            data,
                            ..
                        }) if publication_id == *previous && data == [0, 255, 128, 0] => {}
                        other => anyhow::bail!(
                            "Lossy source changed without its discontinuity: {other:?}"
                        ),
                    }
                }
                let received = owned_data.message(retained, &[0, 255, 128, 2]).await?;
                anyhow::ensure!(
                    received == fresh,
                    "retained Lossy cursor missed the replacement message"
                );
                anyhow::Ok(())
            })
            .await??;
        }
        replacement.room.close().await?;
        replacement.participant.close().await?;
    }
    let second_expected = second.retained.clone();
    runtime.disconnect().await?;
    anyhow::ensure!(
        runtime.snapshot().connection == pulsebeam_agent_core::ConnectionState::Disconnected,
        "graceful close did not disconnect"
    );
    anyhow::ensure!(
        runtime.statistics().borrow().is_none()
            && created.get() == retired.get()
            && retired.get() == usize::from(reconnect).saturating_add(1),
        "native graph or runtime services not retired with retained media handles"
    );
    anyhow::ensure!(
        !owned_data.room.is_closed()
            && !owned_data.participant.is_closed()
            && !video.is_closed()
            && !audio.is_closed()
            && !video_subscription.is_closed()
            && !audio_subscription.is_closed(),
        "disconnect ended logical inputs or media interests"
    );
    anyhow::ensure!(
        video.submit(video_frame(0)).await.is_err(),
        "retired video target admitted input"
    );
    anyhow::ensure!(
        audio
            .submit(InputFrame::Opus {
                frame: stock_opus_fixture()[0].clone()
            })
            .await
            .is_err(),
        "retired audio target admitted input"
    );
    video_owner.close().await?;
    audio_owner.close().await?;
    anyhow::ensure!(
        !video.is_closed() && !audio.is_closed(),
        "publication close stopped independently owned producers"
    );
    video.close().await?;
    audio.close().await?;
    anyhow::ensure!(
        video.is_closed() && audio.is_closed(),
        "explicit producer close retained inputs"
    );
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed),
        "closed video observer stayed open"
    );
    anyhow::ensure!(
        matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "closed audio observer stayed open"
    );
    video_subscription.close().await?;
    audio_subscription.close().await?;
    anyhow::ensure!(
        video_subscription.is_closed() && audio_subscription.is_closed(),
        "released media interests remained open"
    );
    anyhow::ensure!(
        first.retained == first_expected
            && same_peer.retained == same_peer_expected
            && second.retained == second_expected,
        "returned owning frames changed after close"
    );

    let mut pending_data = Box::pin(owned_data.participant.next());
    anyhow::ensure!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            pending_data.as_mut(),
            cx
        )))
        .await
        .is_pending(),
        "disconnect ended logical data demand"
    );
    owned_data.room.close().await?;
    anyhow::ensure!(
        matches!(owned_data.room.next().await?, None) && !owned_data.participant.is_closed(),
        "cursor close ended independent demand"
    );
    agent.close().await?;
    anyhow::ensure!(
        matches!(pending_data.await?, None),
        "terminal Agent close did not finish a pending data read"
    );

    if reconnect {
        prove_takeover(successor_config, host, room.as_str()).await?;
    }

    Ok(PeerReport {
        participant,
        reconnected: reconnect,
        first_generation,
        final_generation,
        video_frames: first
            .video_frames
            .saturating_add(same_peer.video_frames)
            .saturating_add(second.video_frames),
        audio_frames: first
            .audio_frames
            .saturating_add(same_peer.audio_frames)
            .saturating_add(second.audio_frames),
    })
}

async fn prove_takeover(config: Config, host: NativeHost, room: &str) -> anyhow::Result<()> {
    let evidence = Arc::new(Mutex::new(CreationEvidence {
        dropped: true,
        ..Default::default()
    }));
    let predecessor_host = host.clone();
    let (predecessor, actor) = RuntimeAgent::managed_with_services(
        config.clone(),
        Host::new(Box::new(LostCreationResponse {
            inner: create_http_client(),
            evidence: evidence.clone(),
            successor_receiver: None,
        })),
        move || predecessor_host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let old = Agent::from_runtime(predecessor.clone());
    let video = old.create_video_input(Default::default()).await?;
    let publication = old
        .video("takeover-camera".into())?
        .publish(Default::default())?;
    let _pipe = video.pipe_to(&publication)?;
    predecessor.join(room.to_owned())?;
    let first = wait_connected(&old, None).await?;
    let updates = publication.updates()?;
    wait_publication_admitted(&updates).await?;
    predecessor.reconnect().await?;
    wait_connected(&old, first.generation).await?;
    publication.ready().await?;

    let (successor, actor) =
        RuntimeAgent::managed_with_services(config.clone(), Host::new(create_http_client()), {
            let host = host.clone();
            move || host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    let new = Agent::from_runtime(successor.clone());
    successor.join(room.to_owned())?;
    wait_connected(&new, None).await?;

    // Actual captured initial and continuation POSTs are replayed directly to
    // the server, independent of delivery of the replacement notification.
    let posts = std::mem::take(&mut evidence.lock().unwrap().posts);
    anyhow::ensure!(posts.len() == 2, "missing captured predecessor admissions");
    let client = create_http_client();
    for post in posts {
        let response = client
            .execute(post)
            .await
            .map_err(|error| anyhow::anyhow!("stale admission replay: {error}"))?;
        anyhow::ensure!(
            response.status() == http::StatusCode::CONFLICT,
            "superseded admission was not fenced: {}",
            response.status()
        );
    }
    anyhow::ensure!(
        new.snapshot().connection == core_ffi::ConnectionState::Connected,
        "stale replay displaced successor"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        let snapshots = old.snapshots();
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("predecessor snapshot stream closed");
            };
            if snapshot.failure.as_ref().is_some_and(|failure| {
                failure.class == core_ffi::FailureClass::ParticipantSuperseded
            }) {
                return anyhow::Ok(());
            }
        }
    })
    .await??;
    anyhow::ensure!(
        video.submit(video_frame(0)).await.is_err(),
        "replaced predecessor retained media target"
    );
    publication.close().await?;
    video.close().await?;
    predecessor.abort().await?;
    new.close().await?;
    prove_held_creation_takeover(config, host, room).await?;
    Ok(())
}

async fn prove_held_creation_takeover(
    config: Config,
    host: NativeHost,
    room: &str,
) -> anyhow::Result<()> {
    let evidence = Arc::new(Mutex::new(CreationEvidence {
        dropped: true,
        ..Default::default()
    }));
    let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let http = HeldCreationResponse {
        inner: Box::new(LostCreationResponse {
            inner: create_http_client(),
            evidence: evidence.clone(),
            successor_receiver: None,
        }),
        admitted: Mutex::new(Some(admitted_tx)),
        release: Mutex::new(Some(release_rx)),
    };
    let old_host = host.clone();
    let (predecessor, actor) = RuntimeAgent::managed_with_services(
        config.clone(),
        Host::new(Box::new(http)),
        move || old_host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let old = Agent::from_runtime(predecessor.clone());
    let video = old.create_video_input(Default::default()).await?;
    let publication = old
        .video("held-camera".into())?
        .publish(Default::default())?;
    let _pipe = video.pipe_to(&publication)?;
    let snapshots = old.snapshots();
    let connected = Rc::new(Cell::new(false));
    let superseded = Rc::new(Cell::new(false));
    let admitted = Rc::new(Cell::new(false));
    let observed_connected = connected.clone();
    let observed_superseded = superseded.clone();
    let observed_admitted = admitted.clone();
    let publication_updates = publication.updates()?;
    let observed_publication = publication_updates.clone();
    let observer = tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                update = snapshots.next() => {
                    let SnapshotUpdate::Snapshot { snapshot } = update else { return; };
                    if snapshot.connection == core_ffi::ConnectionState::Connected {
                        observed_connected.set(true);
                    }
                    if snapshot.participant_id.is_some() {
                        observed_admitted.set(true);
                    }
                    if snapshot.failure.as_ref().is_some_and(|failure| {
                        failure.class == core_ffi::FailureClass::ParticipantSuperseded
                    }) {
                        observed_superseded.set(true);
                    }
                }
                update = observed_publication.next() => {
                    let Ok(Some(state)) = update else { return; };
                    if state.admission.status == core_ffi::PublicationAdmissionStatus::Admitted
                        || state.admission.publication_id.is_some()
                        || state.admission.execution == core_ffi::PublicationExecution::Installed
                    {
                        observed_admitted.set(true);
                    }
                }
            }
        }
    });
    predecessor.join(room.to_owned())?;
    tokio::time::timeout(Duration::from_secs(20), admitted_rx).await??;
    anyhow::ensure!(
        !connected.get(),
        "predecessor connected before held SDP answer"
    );
    let (successor, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), move || {
            host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    let new = Agent::from_runtime(successor.clone());
    successor.join(room.to_owned())?;
    let successor_connected = wait_connected(&new, None).await?;
    anyhow::ensure!(
        !connected.get(),
        "predecessor connected before answer release"
    );
    release_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("held answer receiver closed"))?;
    // Observe the whole bounded window, including transient connected snapshots.
    tokio::time::sleep(Duration::from_secs(20)).await;
    if !superseded.get() {
        predecessor.disconnect().await?;
        predecessor.join(room.to_owned())?;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let snapshots = old.snapshots();
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("held predecessor snapshot stream closed");
            };
            if snapshot.failure.as_ref().is_some_and(|failure| {
                failure.class == core_ffi::FailureClass::ParticipantSuperseded
            }) {
                return anyhow::Ok(());
            }
        }
    })
    .await??;
    anyhow::ensure!(
        evidence
            .lock()
            .unwrap()
            .statuses
            .contains(&http::StatusCode::CONFLICT),
        "predecessor continuation did not receive authoritative ownership rejection"
    );
    let requests = evidence.lock().unwrap().requests.clone();
    anyhow::ensure!(!requests.is_empty(), "missing held admission evidence");
    anyhow::ensure!(
        requests.iter().all(|request| request.1 == requests[0].1),
        "predecessor fell back to a fresh ownership family"
    );
    anyhow::ensure!(
        !connected.get(),
        "obsolete held admission reached Connected (superseded observed: {})",
        superseded.get()
    );
    anyhow::ensure!(
        !admitted.get(),
        "obsolete predecessor restored an admitted session or publication"
    );
    anyhow::ensure!(
        new.snapshot().connection == core_ffi::ConnectionState::Connected
            && new.snapshot().generation == successor_connected.generation,
        "held predecessor displaced successor"
    );
    println!(
        "[held-creation] attempts={} superseded_observed={} bounded_seconds=20",
        requests.len(),
        superseded.get()
    );
    anyhow::ensure!(
        !observer.is_finished(),
        "held admission observation ended early"
    );
    observer.abort();
    let _ = observer.await;
    publication_updates.close();
    publication.close().await?;
    video.close().await?;
    predecessor.abort().await?;
    new.close().await?;
    Ok(())
}

async fn native_payload_channels(
    runtime: &RuntimeAgent,
    generation: u64,
    count: usize,
    closed_at_least: Option<u64>,
) -> anyhow::Result<(BTreeMap<u16, String>, u64)> {
    let mut statistics = runtime.statistics();
    let mut last = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = statistics.borrow_and_update().clone();
            if let Some(current) = current.filter(|current| current.generation == generation) {
                let report: serde_json::Value = serde_json::from_slice(&current.report_json)?;
                let entries = report
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("native stats are not an array"))?;
                let closed = entries
                    .iter()
                    .find(|entry| entry["type"] == "peer-connection")
                    .and_then(|entry| entry["dataChannelsClosed"].as_u64())
                    .ok_or_else(|| anyhow::anyhow!("native stats omit closed-channel counter"))?;
                let mut channels = BTreeMap::new();
                for entry in entries {
                    if entry["type"] != "data-channel" || entry["state"] != "open" {
                        continue;
                    }
                    let Some(label) = entry["label"]
                        .as_str()
                        .filter(|label| label.starts_with("native/payload/"))
                    else {
                        continue;
                    };
                    let stream =
                        u16::try_from(entry["dataChannelIdentifier"].as_u64().ok_or_else(
                            || anyhow::anyhow!("native data channel has no stream ID"),
                        )?)?;
                    anyhow::ensure!(
                        stream != 0 && stream != u16::MAX,
                        "payload uses reserved SID"
                    );
                    anyhow::ensure!(
                        channels.insert(stream, label.to_owned()).is_none(),
                        "simultaneous payload SID alias"
                    );
                }
                if channels.len() == count
                    && closed_at_least.is_none_or(|minimum| closed >= minimum)
                {
                    return Ok((channels, closed));
                }
                last = Some((channels, closed));
            }
            statistics.changed().await?;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native payload channel count did not reach {count} in generation {generation}; last open SID/label map and closed count: {last:?}"))?
}

async fn wait_publication_admitted(
    updates: &pulsebeam_agent_native::ffi::PublicationUpdates,
) -> anyhow::Result<core_ffi::PublicationState> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = updates
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("publication observation ended before admission"))?;
            match state.admission.status {
                core_ffi::PublicationAdmissionStatus::Admitted => return Ok(state),
                core_ffi::PublicationAdmissionStatus::Pending => {}
                core_ffi::PublicationAdmissionStatus::Rejected => {
                    anyhow::bail!(
                        "publication admission rejected: {:?}",
                        state.admission.error
                    );
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("publication admission timed out"))?
}

async fn wait_connected(
    agent: &Agent,
    previous: Option<u64>,
) -> anyhow::Result<core_ffi::Snapshot> {
    wait_connected_when(agent, previous, |_| true).await
}

async fn wait_connected_when(
    agent: &Agent,
    previous: Option<u64>,
    ready: impl Fn(&core_ffi::Snapshot) -> bool,
) -> anyhow::Result<core_ffi::Snapshot> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            let generation_ready = snapshot
                .generation
                .is_some_and(|generation| Some(generation) != previous);
            if snapshot.connection == core_ffi::ConnectionState::Connected
                && generation_ready
                && ready(&snapshot)
            {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native agent did not connect"))?
}

async fn wait_media_absent(
    video: &VideoSubscription,
    audio: &AudioSubscription,
) -> anyhow::Result<()> {
    let video_updates = video.updates()?;
    let audio_updates = audio.updates()?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let video = video.state();
            let audio = audio.state();
            if video.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
                && video.source_track_id.is_none()
                && audio.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
                && audio.source_track_id.is_none()
            {
                return Ok(());
            }
            tokio::select! {
                state = video_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "video subscription observation ended");
                }
                state = audio_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "audio subscription observation ended");
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("retired media remained available to its subscriptions"));
    video_updates.close();
    audio_updates.close();
    result?
}

async fn wait_media_bindings(
    agent: &Agent,
    video: &VideoSubscription,
    audio: &AudioSubscription,
    video_id: &str,
    audio_id: &str,
) -> anyhow::Result<core_ffi::Snapshot> {
    let video_updates = video.updates()?;
    let audio_updates = audio.updates()?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let video = video.state();
            let audio = audio.state();
            if video.source_status == core_ffi::SubscriptionSourceStatus::Available
                && video.source_track_id.as_deref() == Some(video_id)
                && video.admission == core_ffi::PublicationAdmissionStatus::Admitted
                && video.execution == core_ffi::PublicationExecution::Installed
                && audio.source_status == core_ffi::SubscriptionSourceStatus::Available
                && audio.source_track_id.as_deref() == Some(audio_id)
                && audio.admission == core_ffi::PublicationAdmissionStatus::Admitted
                && audio.execution == core_ffi::PublicationExecution::Installed
            {
                return Ok(agent.snapshot());
            }
            tokio::select! {
                state = video_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "video subscription observation ended");
                }
                state = audio_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "audio subscription observation ended");
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("owned native media reception did not become installed"));
    video_updates.close();
    audio_updates.close();
    result?
}

// Raw transport churn is a controlled-host fixture, not a generated SDK policy path.
async fn wait_remote_media(
    agent: &Agent,
    video_subscription: &VideoSubscription,
    audio_subscription: &AudioSubscription,
    video: &pulsebeam_agent_native::ffi::VideoSource,
    frame_number: &mut u64,
) -> anyhow::Result<(core_ffi::Snapshot, String, String)> {
    let video_updates = video_subscription.updates()?;
    let audio_updates = audio_subscription.updates()?;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let video_state = video_subscription.state();
            let audio_state = audio_subscription.state();
            if video_state.source_status == core_ffi::SubscriptionSourceStatus::Available
                && audio_state.source_status == core_ffi::SubscriptionSourceStatus::Available
                && let (Some(video_id), Some(audio_id)) =
                    (video_state.source_track_id, audio_state.source_track_id)
            {
                return Ok((agent.snapshot(), video_id, audio_id));
            }
            tokio::select! {
                _ = tick.tick() => {
                    video.submit(video_frame(video.capture_time_us().await?)).await?;
                    *frame_number = frame_number.checked_add(1).unwrap();
                }
                state = video_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "video subscription observation ended");
                }
                state = audio_updates.next() => {
                    anyhow::ensure!(state?.is_some(), "audio subscription observation ended");
                }
            }
        }
    }).await.map_err(|_| anyhow::anyhow!(
        "native named subscriptions did not resolve media: video={:?}, audio={:?}, session={:?}",
        video_subscription.state(), audio_subscription.state(), agent.snapshot()
    ));
    video_updates.close();
    audio_updates.close();
    result?
}

fn video_frame(timestamp_us: i64) -> InputFrame {
    video_frame_for(timestamp_us, pulsebeam_testdata::QualityVideoSource::Zero)
}

fn video_frame_for(
    timestamp_us: i64,
    source: pulsebeam_testdata::QualityVideoSource,
) -> InputFrame {
    let corpus = pulsebeam_testdata::quality_corpus_video(
        source,
        pulsebeam_testdata::QualityVideoLayer::P180,
    );
    let frame = corpus.frame(0).expect("genuine corpus keyframe");
    InputFrame::H264 {
        frame: H264Frame {
            timestamp_us,
            payload: frame.encoded.to_vec(),
            width: 320,
            height: 180,
            keyframe: true,
            temporal_index: None,
            base_layer_sync: false,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    completion: &tokio::sync::Barrier,
    agent: &Agent,
    video: &pulsebeam_agent_native::ffi::VideoSource,
    audio: &pulsebeam_agent_native::ffi::AudioSource,
    video_probe: &MediaProbe,
    audio_probe: &MediaProbe,
    round: u64,
    frame_number: &mut u64,
) -> anyhow::Result<ExchangeReport> {
    let generation = agent
        .snapshot()
        .generation
        .ok_or_else(|| anyhow::anyhow!("exchange without generation"))?;
    video_probe.begin_window()?;
    audio_probe.begin_window()?;
    let mut video_ready = false;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut audio_ready = false;
    let started = Instant::now();
    let mut video_diagnosed = false;
    let native_rtp_report = || {
        agent
            .statistics()
            .and_then(|statistics| {
                serde_json::from_slice::<serde_json::Value>(&statistics.report_json).ok()
            })
            .map(|report| {
                if let Some(entries) = report.as_array() {
                    serde_json::Value::Array(
                        entries
                            .iter()
                            .filter(|entry| {
                                matches!(
                                    entry["type"].as_str(),
                                    Some("inbound-rtp" | "outbound-rtp" | "media-source")
                                )
                            })
                            .cloned()
                            .collect(),
                    )
                } else {
                    report
                }
            })
    };
    let mut proof = None;
    let completed = completion.wait();
    tokio::pin!(completed);

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if proof.is_none() {
                video_ready = video_probe.ready(generation)?;
                audio_ready = audio_probe.ready(generation)?;
                if video_ready && audio_ready {
                    // Finish at the actual fresh observation, not after waiting
                    // for the peer. Keep sourcing until both proofs are finished.
                    let (video_frames, retained_video) = video_probe.finish_window(generation)?;
                    let (audio_frames, retained_audio) = audio_probe.finish_window(generation)?;
                    proof = Some(ExchangeReport {
                        video_frames,
                        audio_frames,
                        retained: vec![retained_video, retained_audio],
                    });
                }
            }
            tokio::select! {
                _ = &mut completed, if proof.is_some() => break,
                _ = tick.tick() => {
                    if !video_diagnosed && started.elapsed() >= Duration::from_secs(1) {
                        let observed = video_probe.state.lock().unwrap();
                        let error = observed.video.require(&observed.video_baseline, generation, (320, 180), 2, Instant::now()).err();
                        let native_rtp = native_rtp_report();
                        tracing::warn!(round, participant = ?agent.snapshot().participant_id, ?error, frames = observed.blocks, submitted = *frame_number, ?native_rtp, native_video = ?observed.video, "native media boundary observation");
                        video_diagnosed = true;
                    }
                    // Genuine first-IDR corpus inputs, held for 200ms so native
                    // framerate adaptation cannot sample only one alternating scene.
                    let source = if (*frame_number / 10).is_multiple_of(2) {
                        pulsebeam_testdata::QualityVideoSource::Zero
                    } else {
                        pulsebeam_testdata::QualityVideoSource::One
                    };
                    video.submit(video_frame_for(video.capture_time_us().await?, source)).await?;
                    let fixtures = stock_opus_fixture();
                    let index = usize::try_from(*frame_number).unwrap() % fixtures.len();
                    let mut frame = fixtures[index].clone();
                    let cycle = *frame_number / u64::try_from(fixtures.len()).unwrap();
                    frame.timestamp = frame.timestamp.checked_add(u32::try_from(cycle.checked_mul(48_000).unwrap()).unwrap()).unwrap();
                    audio.submit(InputFrame::Opus { frame }).await.inspect_err(|error| {
                        let native_rtp = native_rtp_report();
                        tracing::error!(round, generation, submitted = *frame_number, ?error, ?native_rtp, "native Opus producer admission failed");
                    })?;
                    *frame_number = frame_number.checked_add(1).unwrap();
                }
            }
        }
        anyhow::Ok(())
    })
    .await
    .map_err(|_| {
            let observed = video_probe.state.lock().unwrap();
            let video_error = observed.video.require(&observed.video_baseline, generation, (320, 180), 2, Instant::now()).err();
            let native_rtp = native_rtp_report();
            tracing::warn!(round, generation, participant = ?agent.snapshot().participant_id,
                frames = observed.blocks, submitted = *frame_number, ?native_rtp,
                native_video = ?observed.video, "native media boundary at final timeout");
            anyhow::anyhow!(
                "native media exchange {round} timed out: participant={:?} video_ready={video_ready} audio_ready={audio_ready} video_oracle={video_error:?}",
                agent.snapshot().participant_id,
            )
    })??;

    proof.ok_or_else(|| anyhow::anyhow!("native exchange completed without a media proof"))
}
