//! Non-simulation encoded worker on the production shared UDP listener.

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use noq_proto::rustls::{
    self, RootCertStore,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    time_provider::DefaultTimeProvider,
};
use pulsebeam_agent_native::agent_core::{
    AgentCommand, AudioSubscription, ConnectionState, DesiredState, MediaKind, PublicationIntent,
    RtpSource, VideoSubscription,
};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        recorder::{Limits, RecordError, Recorder},
        rtp::{Binding, Publication, SendPacketError},
        timed::{Policy, Receive},
    },
    media::{
        Opus, Packet, PacketBudget,
        units::{Access, Codec, Event},
    },
    transport::{self, DatagramAdmission},
};

use pulsebeam_core::fabric::Profile;
use pulsebeam_runtime::{
    net::tcp::{TcpListener, TcpStream},
    sync::CancellationToken,
};
use pulsebeam_testdata::wire_capture::{Cursor, PacketKind};
use tokio_rustls::TlsAcceptor;

use super::{Family, ROOM, config};

pub(super) const AUTHORITY: &str = "localhost";
const DATA: &[u8] = &[0xf8, 0xff, 0xfe];

struct VideoFixture {
    name: &'static str,
    profile: Profile,
    codec: Codec,
    wire: &'static [u8],
    index: &'static str,
}

macro_rules! video_fixture {
    ($name:literal, $codec:ident) => {
        VideoFixture {
            name: $name,
            profile: Profile::$codec,
            codec: Codec::$codec,
            wire: include_bytes!(concat!(
                "../../../../pulsebeam-testdata/fixtures/",
                $name,
                "-v1/L1T1/wire.bin"
            )),
            index: include_str!(concat!(
                "../../../../pulsebeam-testdata/fixtures/",
                $name,
                "-v1/L1T1/wire.tsv"
            )),
        }
    };
}

const VIDEO: [VideoFixture; 3] = [
    video_fixture!("vp8", Vp8),
    video_fixture!("vp9", Vp9),
    video_fixture!("av1", Av1),
];

impl VideoFixture {
    fn keyframe(&self) -> Result<Vec<Packet<&'static [u8]>>> {
        let mut cursor =
            Cursor::new(self.wire, self.index).map_err(|_| anyhow!("invalid video fixture"))?;
        let mut packets = Vec::new();
        while let Some(packet) = cursor.next_due(Duration::ZERO) {
            if packet.kind != PacketKind::Rtp {
                continue;
            }
            let [
                0x90,
                pt,
                seq_hi,
                seq_lo,
                a,
                b,
                c,
                d,
                _,
                _,
                _,
                _,
                _,
                _,
                ext_hi,
                ext_lo,
                rest @ ..,
            ] = packet.bytes
            else {
                anyhow::bail!("fixture RTP header");
            };
            let extension_bytes = usize::from(u16::from_be_bytes([*ext_hi, *ext_lo])) << 2;
            let marker = pt & 0x80 != 0;
            packets.push(Packet {
                sequence: u32::from(u16::from_be_bytes([*seq_hi, *seq_lo])),
                timestamp: u32::from_be_bytes([*a, *b, *c, *d]),
                marker,
                first: packets.is_empty(),
                last: marker,
                payload: rest.get(extension_bytes..).context("fixture RTP payload")?,
            });
        }
        ensure!(
            packets.len() > 1 && packets.last().is_some_and(|packet| packet.marker),
            "fixture must contain a fragmented keyframe"
        );
        Ok(packets)
    }
}

pub(super) struct Tls {
    pub(super) quic: transport::ServerConfig,
    pub(super) https: TlsAcceptor,
    pub(super) certificate: CertificateDer<'static>,
}

#[allow(
    clippy::disallowed_types,
    reason = "TLS and Noq require shared immutable clock providers"
)]
pub(super) fn tls() -> Result<Tls> {
    use std::sync::Arc;

    let identity = rcgen::generate_simple_self_signed(vec![AUTHORITY.into(), "127.0.0.1".into()])?;
    let certificate = identity.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der());
    let quic = transport::server_config(
        vec![certificate.clone()],
        key.clone_key().into(),
        Arc::new(DefaultTimeProvider),
        Arc::new(noq_proto::StdSystemTime),
        NonZeroUsize::MIN,
    )
    .map_err(|error| anyhow!("native TLS identity: {error:?}"))?;
    let https = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key.into())?;
    Ok(Tls {
        quic,
        https: TlsAcceptor::from(Arc::new(https)),
        certificate,
    })
}

/// Mirror TLS termination in front of the node's real placement HTTP API.
pub(super) async fn placement_https(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    backend: SocketAddr,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        let (socket, _) = pulsebeam_runtime::select! {
            connection = listener.accept() => connection?,
            () = shutdown.cancelled() => return Ok(()),
        };
        let acceptor = acceptor.clone();
        let shutdown = shutdown.clone();
        drop(pulsebeam_runtime::task::spawn_local(async move {
            let copy = async {
                let mut incoming = acceptor.accept(socket).await?;
                let mut outgoing = TcpStream::connect(backend).await?;
                pulsebeam_runtime::io::copy_bidirectional(&mut incoming, &mut outgoing).await
            };
            pulsebeam_runtime::select! {
                result = copy => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "conformance HTTPS placement connection ended");
                    }
                }
                () = shutdown.cancelled() => {}
            }
        }));
    }
}

fn check(agent: &Agent) -> Result<()> {
    ensure!(
        agent.snapshot().terminal_failure.is_none(),
        "native worker admission: {:?}",
        agent.snapshot().terminal_failure
    );
    Ok(())
}

async fn drive_pair(publisher: &mut Agent, subscriber: &mut Agent) -> Result<()> {
    pulsebeam_runtime::select! {
        result = publisher.turn() => result.map_err(|error| anyhow!("native publisher: {error:?}")),
        result = subscriber.turn() => result.map_err(|error| anyhow!("native subscriber: {error:?}")),
    }
}

async fn ready_source(publisher: &mut Agent, kind: MediaKind, track: &str) -> Result<Publication> {
    let result = pulsebeam_runtime::time::timeout(Duration::from_secs(5), async {
        loop {
            check(publisher)?;
            if let Some(source) = publisher.rtp_publication(kind, track) {
                return Ok(source);
            }
            publisher
                .turn()
                .await
                .map_err(|error| anyhow!("native publish install: {error:?}"))?;
        }
    })
    .await;
    result.with_context(|| {
        format!(
            "{track} publication deadline; state: {:?}",
            publisher.snapshot()
        )
    })?
}

async fn ready_reader(
    publisher: &mut Agent,
    subscriber: &mut Agent,
    kind: MediaKind,
    publication: &str,
    label: &str,
) -> Result<Binding> {
    let result = pulsebeam_runtime::time::timeout(Duration::from_secs(5), async {
        loop {
            check(subscriber)?;
            if let Some(binding) = subscriber.rtp_subscription(kind, publication) {
                subscriber
                    .attach_encoded(
                        binding,
                        Policy::Recording {
                            reorder: Duration::from_millis(25),
                        },
                    )
                    .map_err(|error| anyhow!("worker attach: {error:?}"))?;
                return Ok(binding);
            }
            drive_pair(publisher, subscriber).await?;
        }
    })
    .await;
    result.with_context(|| {
        format!(
            "{label} subscription deadline; state: {:?}",
            subscriber.snapshot()
        )
    })?
}

async fn send_read(
    publisher: &mut Agent,
    subscriber: &mut Agent,
    source: &Publication,
    binding: Binding,
    sequence: u32,
    mut observe: impl FnMut(Binding, &Receive) -> Result<()>,
) -> Result<()> {
    let opus = Opus::new(PacketBudget::new(1200, 12).context("RTP budget")?);
    let packet = opus
        .packetize(
            sequence,
            sequence.saturating_add(1).saturating_mul(960),
            true,
            DATA,
        )
        .map_err(|error| anyhow!("Opus packetizer: {error:?}"))?;
    send_encoded(
        publisher,
        subscriber,
        source,
        binding,
        &[packet],
        |binding, event| {
            observe(binding, event)?;
            if let Receive::Media(Event::Unit(unit)) = event {
                ensure!(unit.codec == Codec::Opus && unit.data == DATA && unit.rtp_time.is_some());
            }
            Ok(())
        },
    )
    .await
    .with_context(|| format!("worker Opus sequence {sequence}"))
}

async fn send_encoded(
    publisher: &mut Agent,
    subscriber: &mut Agent,
    source: &Publication,
    binding: Binding,
    packets: &[Packet<impl AsRef<[u8]>>],
    mut observe: impl FnMut(Binding, &Receive) -> Result<()>,
) -> Result<()> {
    let send = async {
        for packet in packets {
            loop {
                match publisher.send_rtp(source, packet.as_send_packet(&[])) {
                    Ok(()) => break,
                    Err(SendPacketError::Datagram(
                        DatagramAdmission::ControlTurn | DatagramAdmission::Full,
                    )) => {}
                    Err(error) => anyhow::bail!("real QUIC send: {error:?}"),
                }
                publisher
                    .turn()
                    .await
                    .map_err(|error| anyhow!("native send progress: {error:?}"))?;
            }
        }
        loop {
            publisher
                .turn()
                .await
                .map_err(|error| anyhow!("native publisher progress: {error:?}"))?;
        }
    };
    let receive = async {
        loop {
            while let Some(event) = subscriber.next_encoded(binding) {
                observe(binding, &event)?;
                if matches!(event, Receive::Media(Event::Unit(_))) {
                    return Ok(());
                }
            }
            check(subscriber)?;
            subscriber
                .turn()
                .await
                .map_err(|error| anyhow!("native worker progress: {error:?}"))?;
        }
    };
    let result = pulsebeam_runtime::select! {
        result = send => result,
        result = pulsebeam_runtime::time::timeout(Duration::from_secs(3), receive) => result.context("real encoded delivery deadline").and_then(|result| result),
    };
    result.with_context(|| format!("worker publication {}", source.publication_id()))
}

async fn close(agent: &mut Agent) -> Result<()> {
    agent.command(AgentCommand::Disconnect)?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(3), async {
        while agent.snapshot().connection != ConnectionState::Disconnected {
            agent
                .turn()
                .await
                .map_err(|error| anyhow!("native worker disconnect: {error:?}"))?;
        }
        ensure!(agent.quic_peer().is_none());
        anyhow::Ok(())
    })
    .await
    .context("real QUIC cleanup deadline")?
}

pub(super) async fn witness(
    endpoint: &str,
    port: u16,
    certificate: CertificateDer<'static>,
    rtc_probe: impl AsyncFn(&'static [u8]) -> Result<()>,
) -> Result<()> {
    let local = Ipv4Addr::LOCALHOST.into();
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone())?;
    let make = |name| -> Result<Agent> {
        Agent::new(config(endpoint, name)?.session, local, roots.clone())
            .and_then(|agent| agent.with_placement_root(certificate.as_ref()))
            .map_err(|error| anyhow!("production Agent setup: {error:?}"))
    };
    let mut publisher = make("quic-source")?;
    let mut subscriber = make("quic-worker")?;
    let mut publications = vec![PublicationIntent {
        slot: "voice".into(),
        track_id: "voice".into(),
        kind: MediaKind::Audio,
        active: true,
    }];
    for video in &VIDEO {
        publisher.configure_video_codec(video.name, video.profile)?;
        publications.push(PublicationIntent {
            slot: video.name.into(),
            track_id: video.name.into(),
            kind: MediaKind::Video,
            active: true,
        });
    }
    publisher.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications,
        ..Default::default()
    }))?;
    publisher.command(AgentCommand::Join(ROOM.into()))?;
    let source = ready_source(&mut publisher, MediaKind::Audio, "voice").await?;
    let publication = source.publication_id().to_owned();
    let mut sources = Vec::new();
    let source_facts = |id: &str, mime: &str| -> Result<RtpSource> {
        RtpSource::native_quic_baseline(id.into(), ROOM.into(), mime)
            .context("native source format")
    };
    let mut rtp_sources = vec![source_facts(&publication, "audio/opus")?];
    for video in &VIDEO {
        let source = ready_source(&mut publisher, MediaKind::Video, video.name).await?;
        rtp_sources.push(source_facts(
            source.publication_id(),
            &format!("video/{}", video.name),
        )?);
        sources.push(source);
    }
    subscriber.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        rtp_sources,
        audio: AudioSubscription {
            pinned: vec![publication.clone()],
            ..Default::default()
        },
        video: (0_u32..)
            .zip(&sources)
            .map(|(slot, source)| VideoSubscription {
                slot,
                publication_id: source.publication_id().into(),
                decode_target: None,
                selector: None,
                height: 720,
                min_height: 0,
                min_fps: 0,
                priority: 0,
            })
            .collect(),
        ..Default::default()
    }))?;
    subscriber.command(AgentCommand::Join(ROOM.into()))?;
    let first = ready_reader(
        &mut publisher,
        &mut subscriber,
        MediaKind::Audio,
        &publication,
        "Opus",
    )
    .await?;
    let peer = Some(Family::V4.address(port));
    ensure!(
        publisher.quic_peer() == peer && subscriber.quic_peer() == peer,
        "authenticated QUIC selected different UDP endpoint from WebRTC"
    );
    let generation = publisher
        .snapshot()
        .generation
        .context("native publisher generation")?;
    let directory =
        PathBuf::from(std::env::var_os("TEST_TMPDIR").context("Bazel test output isolation")?)
            .join("encoded-worker");
    let mut recorder = Recorder::create(
        &directory,
        publication.clone(),
        Codec::Opus,
        Limits {
            units: 8,
            bytes: 8192,
        },
    )
    .map_err(|error| anyhow!("create worker recording: {error:?}"))?;
    let mut store = |binding, event: &Receive| {
        recorder
            .record(binding, event)
            .map_err(|error| anyhow!("write worker event: {error:?}"))
    };
    pulsebeam_runtime::try_join!(
        send_read(
            &mut publisher,
            &mut subscriber,
            &source,
            first,
            0,
            &mut store
        ),
        rtc_probe(b"RTC DATA during native QUIC media delivery"),
    )?;
    for (video, source) in VIDEO.iter().zip(&sources) {
        let exercise = async {
            let binding = ready_reader(
                &mut publisher,
                &mut subscriber,
                MediaKind::Video,
                source.publication_id(),
                video.name,
            )
            .await?;
            let packets = video.keyframe()?;
            send_encoded(&mut publisher, &mut subscriber, source, binding, &packets, |_, event| {
                if let Receive::Media(Event::Unit(unit)) = event {
                    ensure!(unit.codec == video.codec && unit.access == Access::IndependentVideo
                        && unit.rtp_time.is_some() && unit.data.len() > 1200
                        && unit.last_sequence - unit.first_sequence + 1 == packets.len() as u64,
                        "expected the complete fragmented {} keyframe, got {:?} {:?}, {} bytes, sequences {}..={}",
                        video.name, unit.codec, unit.access, unit.data.len(), unit.first_sequence, unit.last_sequence);
                }
                Ok(())
            }).await
        };
        exercise
            .await
            .with_context(|| format!("{} worker witness", video.name))?;
    }
    publisher.rebind(local).await?;
    pulsebeam_runtime::try_join!(
        send_read(
            &mut publisher,
            &mut subscriber,
            &source,
            first,
            2,
            &mut store
        ),
        rtc_probe(b"RTC DATA after native QUIC address change"),
    )?;
    ensure!(publisher.snapshot().generation == Some(generation) && publisher.quic_peer() == peer);

    close(&mut subscriber).await?;
    ensure!(
        subscriber
            .rtp_subscription(MediaKind::Audio, &publication)
            .is_none()
    );
    ensure!(subscriber.next_encoded(first).is_none());
    subscriber.command(AgentCommand::Join(ROOM.into()))?;
    let next = ready_reader(
        &mut publisher,
        &mut subscriber,
        MediaKind::Audio,
        &publication,
        "replacement Opus",
    )
    .await?;
    ensure!(first != next && subscriber.quic_peer() == peer);
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        next,
        3,
        &mut store,
    )
    .await?;
    let index = fs::read_to_string(directory.join("index.tsv"))?;
    ensure!(
        index
            .lines()
            .filter(|line| line.starts_with("unit\t"))
            .count()
            == 3
    );
    ensure!(
        index
            .lines()
            .any(|line| line.starts_with("gap\t1\t") && line.contains("Gap(Deadline)")),
        "recording did not preserve the explicit hole"
    );
    ensure!(
        index.lines().any(|line| line.starts_with("reset\t2\t")),
        "recording did not record replacement binding"
    );
    ensure!(fs::read(directory.join("0000000002.opus"))? == DATA);
    fs::create_dir(directory.join("0000000003.opus"))?;
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        next,
        4,
        |binding, event| {
            ensure!(matches!(
                recorder.record(binding, event),
                Err(RecordError::Storage(_))
            ));
            Ok(())
        },
    )
    .await?;
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        next,
        5,
        |binding, event| {
            ensure!(matches!(
                recorder.record(binding, event),
                Err(RecordError::Stopped)
            ));
            Ok(())
        },
    )
    .await?;
    pulsebeam_runtime::try_join!(close(&mut publisher), close(&mut subscriber))?;
    tracing::info!(
        port,
        "EXERCISED production QUIC worker, shared RTC UDP endpoint, timed gap, socket rebind, binding reset and fenced storage failure"
    );
    Ok(())
}
