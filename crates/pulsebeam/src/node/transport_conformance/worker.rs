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
};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        recorder::{Limits, RecordError, Recorder},
        rtp::{Binding, Publication, SendPacketError},
        timed::{Policy, Reader, Receive},
    },
    media::{
        Opus, PacketBudget,
        units::{Codec, Event},
    },
    transport::{self, DatagramAdmission},
};

use pulsebeam_runtime::{
    net::tcp::{TcpListener, TcpStream},
    sync::CancellationToken,
};
use tokio_rustls::TlsAcceptor;

use super::{Family, ROOM, config};

pub(super) const AUTHORITY: &str = "localhost";
const DATA: &[u8] = &[0xf8, 0xff, 0xfe];

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

async fn send_read(
    publisher: &mut Agent,
    subscriber: &mut Agent,
    source: &Publication,
    reader: &mut Reader,
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
    let send = async {
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
        loop {
            publisher
                .turn()
                .await
                .map_err(|error| anyhow!("native publisher progress: {error:?}"))?;
        }
    };
    let receive = async {
        loop {
            while let Some((binding, event)) = reader
                .next(subscriber)
                .map_err(|error| anyhow!("real encoded reader: {error:?}"))?
            {
                observe(binding, &event)?;
                if let Receive::Media(Event::Unit(unit)) = event {
                    ensure!(
                        unit.codec == Codec::Opus && unit.data == DATA && unit.rtp_time.is_some()
                    );
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
    result.with_context(|| {
        let stats = subscriber
            .rtp_subscription(MediaKind::Audio, source.publication_id())
            .and_then(|binding| subscriber.encoded_stats(binding));
        format!("worker Opus sequence {sequence}, receiver stats: {stats:?}")
    })
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
    publisher.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications: vec![PublicationIntent {
            slot: "voice".into(),
            track_id: "voice".into(),
            kind: MediaKind::Audio,
            active: true,
        }],
        ..Default::default()
    }))?;
    publisher.command(AgentCommand::Join(ROOM.into()))?;
    let source = loop {
        check(&publisher)?;
        if let Some(source) = publisher.rtp_publication(MediaKind::Audio, "voice") {
            break source;
        }
        publisher
            .turn()
            .await
            .map_err(|error| anyhow!("native publish install: {error:?}"))?;
    };
    let publication = source.publication_id().to_owned();
    subscriber.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        audio: AudioSubscription {
            pinned: vec![publication.clone()],
            ..Default::default()
        },
        ..Default::default()
    }))?;
    let mut reader = Reader::new(
        MediaKind::Audio,
        publication.clone(),
        Policy::Recording {
            reorder: Duration::from_millis(25),
        },
    );
    subscriber.command(AgentCommand::Join(ROOM.into()))?;
    let first = loop {
        check(&subscriber)?;
        if let Some(binding) = reader
            .sync(&mut subscriber)
            .map_err(|error| anyhow!("worker attach: {error:?}"))?
        {
            break binding;
        }
        drive_pair(&mut publisher, &mut subscriber).await?;
    };
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
        publication,
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
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        &mut reader,
        0,
        &mut store,
    )
    .await?;
    publisher.rebind(local).await?;
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        &mut reader,
        2,
        &mut store,
    )
    .await?;
    ensure!(publisher.snapshot().generation == Some(generation) && publisher.quic_peer() == peer);

    close(&mut subscriber).await?;
    ensure!(
        reader
            .sync(&mut subscriber)
            .map_err(|error| anyhow!("retire worker reader: {error:?}"))?
            .is_none()
    );
    subscriber.command(AgentCommand::Join(ROOM.into()))?;
    let next = loop {
        check(&subscriber)?;
        if let Some(binding) = reader
            .sync(&mut subscriber)
            .map_err(|error| anyhow!("worker reattach: {error:?}"))?
        {
            break binding;
        }
        drive_pair(&mut publisher, &mut subscriber).await?;
    };
    ensure!(first != next && subscriber.quic_peer() == peer);
    send_read(
        &mut publisher,
        &mut subscriber,
        &source,
        &mut reader,
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
        &mut reader,
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
        &mut reader,
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
