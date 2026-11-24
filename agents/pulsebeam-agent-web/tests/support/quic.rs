use std::{
    cell::Cell,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::net::{TcpListener, TcpStream};

use agent_core::{
    AgentCommand, AgentConfig, AudioSubscription, DataEvent, DesiredState, MediaKind,
    PublicationIntent, ResourceCommand, ResourceResult, TopicMode, TrackSelector, TrackSpec,
    VideoSubscription,
};
use noq_proto::rustls::RootCertStore;
use pulsebeam_agent_quic::{executor::linux::LinuxAgent, transport::DatagramAdmission};
use pulsebeam_core::identity::{ParticipantId, RoomId};
use pulsebeam_testdata::{QualityAudioSource, QualityVideoLayer, QualityVideoSource};
use thirtyfour::bidi::{BiDi, BrowsingContextId};

use super::{TestResult, evaluate_json, mint_development_token};
use crate::destination::DestinationServer;

const ROOM: &str = "quic-browser-smoke";
const BROWSER: &str = "shipped-browser";
const NATIVE: &str = "linux-quic";

pub struct QuicEndpoint {
    pub https: String,
    pub certificate: Vec<u8>,
}

pub async fn start_destination() -> TestResult<(DestinationServer, QuicEndpoint)> {
    let (endpoint, listener, acceptor, quic) = quic_endpoint()?;
    let destination = DestinationServer::start_observed(move |node, address| {
        let listener = TcpListener::from_std(listener)?;
        tokio::task::spawn_local(serve_placement_tls(listener, acceptor, address));
        Ok(node.quic_listeners(vec![quic]))
    })
    .await?;
    Ok((destination, endpoint))
}

#[allow(
    clippy::disallowed_types,
    reason = "rustls and Noq own shared immutable TLS state"
)]
fn quic_endpoint() -> TestResult<(
    QuicEndpoint,
    std::net::TcpListener,
    tokio_rustls::TlsAcceptor,
    pulsebeam::node::QuicListener,
)> {
    use noq_proto::rustls::{
        self, pki_types::PrivatePkcs8KeyDer, time_provider::DefaultTimeProvider,
    };
    use std::{num::NonZeroUsize, sync::Arc};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let certificate = identity.cert.der().clone();
    let key = || PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key())?;
    let quic = pulsebeam_agent_quic::transport::server_config(
        vec![certificate.clone()],
        key(),
        Arc::new(DefaultTimeProvider),
        Arc::new(noq_proto::StdSystemTime),
        NonZeroUsize::MIN,
    )
    .map_err(|error| format!("browser fixture QUIC TLS: {error:?}"))?;
    Ok((
        QuicEndpoint {
            https: format!("https://localhost:{}", listener.local_addr()?.port()),
            certificate: certificate.to_vec(),
        },
        listener,
        tokio_rustls::TlsAcceptor::from(Arc::new(tls)),
        pulsebeam::node::QuicListener {
            bind: "127.0.0.1:0".parse()?,
            host: "localhost".into(),
            port: 0,
            tls: quic,
        },
    ))
}

async fn serve_placement_tls(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    destination: SocketAddr,
) {
    while let Ok((stream, _)) = listener.accept().await {
        let acceptor = acceptor.clone();
        tokio::task::spawn_local(async move {
            let _ = tokio::time::timeout(Duration::from_secs(15), async {
                let mut client = acceptor.accept(stream).await?;
                let mut upstream = TcpStream::connect(destination).await?;
                tokio::io::copy_bidirectional(&mut client, &mut upstream).await
            })
            .await;
        });
    }
}

pub async fn witness(
    bidi: &BiDi,
    context: &BrowsingContextId,
    destination: &DestinationServer,
    endpoint: &QuicEndpoint,
    expiry: u64,
    script: &str,
) -> TestResult<()> {
    let room = RoomId::new(ROOM)?;
    let browser_token = mint_development_token(&room, &ParticipantId::new(BROWSER)?, expiry)?;
    let native_token = mint_development_token(&room, &ParticipantId::new(NATIVE)?, expiry)?;
    let mut roots = RootCertStore::empty();
    roots.add(endpoint.certificate.clone().into())?;
    let mut agent = LinuxAgent::new(
        AgentConfig {
            endpoint: endpoint.https.clone(),
            token: native_token,
            retry: Default::default(),
            log_level: Default::default(),
        },
        Ipv4Addr::LOCALHOST.into(),
        roots,
    )
    .and_then(|agent| agent.with_placement_root(&endpoint.certificate))
    .map_err(|error| format!("Linux QUIC browser fixture: {error:?}"))?;
    let script = script
        .replace("__ENDPOINT__", &destination.endpoint())
        .replace("__TOKEN__", &browser_token);
    let browser_done = Cell::new(false);
    tokio::time::timeout(Duration::from_secs(18), async {
        tokio::try_join!(
            async {
                let passed: bool = evaluate_json(bidi, context, &script).await?;
                if !passed {
                    return Err("shipped browser did not decode QUIC media".into());
                }
                browser_done.set(true);
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            },
            drive(&mut agent, &browser_done),
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .map_err(|_| "browser/QUIC interoperability deadline")??;
    Ok(())
}

async fn drive(agent: &mut LinuxAgent, browser_done: &Cell<bool>) -> TestResult<()> {
    use pulsebeam_agent_quic::executor::linux::{H264SendError, OpusSendError};
    let spec = TrackSpec::Data {
        track_id: "bytes".into(),
        delivery: TopicMode::Ordered,
    };
    let publications = [
        PublicationIntent {
            slot: "voice".into(),
            kind: MediaKind::Audio,
            track_id: "voice".into(),
            active: true,
        },
        PublicationIntent {
            slot: "camera".into(),
            kind: MediaKind::Video,
            track_id: "camera".into(),
            active: true,
        },
    ];
    agent.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications: publications.to_vec(),
        ..Default::default()
    }))?;
    agent.resource(ResourceCommand::DeclareDataPublication {
        spec: spec.clone(),
        capture_clock: None,
    })?;
    let ResourceResult::Owned { owner } = agent.resource(ResourceCommand::ReceiveData {
        spec: spec.clone(),
        source: TrackSelector {
            room: ROOM.into(),
            participant_id: BROWSER.into(),
            track_id: "bytes".into(),
        },
    })?
    else {
        return Err("missing QUIC DATA receiver owner".into());
    };
    agent.command(AgentCommand::Join(ROOM.into()))?;
    let watch = agent
        .catalog_watch(ROOM.into(), Default::default())
        .map_err(|error| format!("QUIC browser catalog: {error:?}"))?;
    let video =
        pulsebeam_testdata::quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P180);
    let stock = video
        .frame(0)
        .ok_or("missing stock H.264 keyframe")?
        .encoded;
    let mut keyframe = Vec::new();
    for nal in pulsebeam_core::h264::Packetizer::new(pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE)
        .packetize(stock)
    {
        if nal
            .payload
            .first()
            .is_some_and(|header| matches!(header & 0x1f, 5 | 7 | 8))
        {
            keyframe.extend_from_slice(&[0, 0, 0, 1]);
            keyframe.extend_from_slice(&nal.payload);
        }
    }
    let audio = pulsebeam_testdata::quality_corpus_audio(QualityAudioSource::Zero);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame = 0_usize;
    let mut pins = None;
    let mut got = (false, false, false);
    let (mut opus_admitted, mut video_admitted) = (0_u32, 0_u32);
    loop {
        if browser_done.get() && got == (true, true, true) {
            return Ok(());
        }
        let snapshot = agent.snapshot();
        if let Some(failure) = &snapshot.terminal_failure {
            return Err(format!("Linux QUIC interop: {failure:?}").into());
        }
        if pins.is_none() {
            let catalog = agent.catalog_watch_state(watch);
            if let Some(error) = catalog.as_ref().and_then(|state| state.error.as_ref()) {
                return Err(format!("browser catalog watch: {error:?}").into());
            }
            let browser = catalog.and_then(|state| state.page).and_then(|page| {
                page.participants()
                    .into_iter()
                    .find(|participant| participant.id == BROWSER)
            });
            let remote = |track: &str| {
                browser
                    .as_ref()?
                    .publications
                    .iter()
                    .find(|entry| entry.track_id == track)
                    .map(|entry| entry.id.clone())
            };
            if let (Some(voice), Some(camera)) = (remote("voice"), remote("camera")) {
                let revision = snapshot
                    .desired_revision
                    .checked_add(1)
                    .ok_or("browser smoke revision")?;
                agent.command(AgentCommand::ReplaceDesired(DesiredState {
                    revision,
                    connected: true,
                    publications: publications.to_vec(),
                    audio: AudioSubscription {
                        pinned: vec![voice.clone()],
                        ..Default::default()
                    },
                    video: vec![VideoSubscription {
                        slot: 0,
                        publication_id: camera.clone(),
                        selector: None,
                        height: 0,
                        min_height: 0,
                        min_fps: 0,
                        priority: 1,
                    }],
                    ..Default::default()
                }))?;
                let _ = agent.close_catalog_watch(watch);
                pins = Some((voice, camera));
            }
        }
        if let Some((voice, camera)) = &pins {
            got.0 |= agent
                .next_opus()
                .is_some_and(|unit| unit.publication_id == *voice && !unit.payload.is_empty());
            got.1 |= agent
                .next_h264()
                .is_some_and(|unit| unit.publication_id == *camera && !unit.access_unit.is_empty());
        }
        if let ResourceResult::Event {
            event:
                Some(DataEvent::Message {
                    participant_id,
                    data,
                    ..
                }),
        } = agent.resource(ResourceCommand::ReadData { owner })?
        {
            got.2 |= participant_id == BROWSER && data == [255, 0, 7];
        }
        tokio::select! {
            _ = tick.tick() => {
                if browser_done.get() { continue; }
                if frame == 200 {
                    let ready = pins.as_ref().map(|(voice, camera)| (agent.opus_receiver_ready(voice), agent.h264_receiver_ready(camera)));
                    eprintln!("QUIC smoke: state={:?} admitted_audio={opus_admitted} admitted_video={video_admitted} receive_ready={ready:?} received={got:?}", agent.snapshot().connection);
                }
                let timestamp: u32 = frame.try_into()?;
                let sample = audio.frame(frame % audio.len()).ok_or("missing stock Opus packet")?;
                match agent.send_opus("voice", timestamp.wrapping_mul(960), opus_admitted == 0, sample.opus_packet) {
                    Ok(()) => opus_admitted = opus_admitted.saturating_add(1),
                    Err(OpusSendError::NotReady | OpusSendError::Datagram(DatagramAdmission::ControlTurn | DatagramAdmission::Full)) => {},
                    Err(error) => return Err(format!("QUIC Opus browser send: {error:?}").into()),
                }
                if frame.is_multiple_of(10) || agent.take_h264_keyframe_request("camera") {
                    match agent.send_h264("camera", timestamp.wrapping_mul(1800), keyframe.clone()) {
                        Ok(()) => video_admitted = video_admitted.saturating_add(1),
                        Err(H264SendError::Busy | H264SendError::NotReady) => {},
                        Err(error) => return Err(format!("QUIC H.264 browser send: {error:?}").into()),
                    }
                }
                if got == (true, true, true) && frame.is_multiple_of(10) && agent.snapshot().topics.publishers.iter().any(|item| item.publication_id.is_some()) {
                    match agent.send_data(spec.clone(), vec![0, 255, 9]) {
                        Ok(_) | Err(agent_core::AgentError::InvalidTopic(agent_core::TopicError::SendQueueFull(_))) => {},
                        Err(error) => return Err(error.into()),
                    }
                }
                frame = frame.saturating_add(1);
            }
            result = agent.turn() => result.map_err(|error| format!("QUIC browser turn: {error:?}"))?,
        }
    }
}
