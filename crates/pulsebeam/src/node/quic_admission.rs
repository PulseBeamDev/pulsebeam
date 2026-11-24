use std::{
    num::{NonZeroU64, NonZeroUsize},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls require immutable shared configuration"
)]
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use noq_proto::rustls;
use pulsebeam_agent_quic::{
    frame::{Frame, Kind, Phase},
    records::{Admit, Renew, RenewResult},
    transport::{
        self,
        client::{
            ClientTransport,
            admission::{Admission, ClientAdmission},
        },
        control::{ControlStream, Read, Write},
        socket::{Egress, Flush},
    },
};
use pulsebeam_core::auth::{
    DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID, KeyRegistryEntry,
};
use pulsebeam_runtime::{net::tcp::TcpStream, time::Instant};
use rustls::{RootCertStore, pki_types::PrivatePkcs8KeyDer, time_provider::DefaultTimeProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

struct Client {
    transport: ClientTransport,
    socket: pulsebeam_udp::UdpSocket,
    out: Egress,
}

impl Client {
    #[allow(
        clippy::disallowed_types,
        reason = "rustls requires a shared immutable time provider"
    )]
    async fn new(edge: SocketAddr, roots: RootCertStore) -> Result<Self> {
        let socket = transport::socket::bind(SocketAddr::new(edge.ip(), 0))
            .await?
            .activate()?;
        let config = transport::client_config(roots, Arc::new(DefaultTimeProvider))
            .map_err(|e| anyhow::anyhow!("client configuration: {e:?}"))?;
        let transport =
            ClientTransport::connect(Instant::now().into_std(), config, edge, "edge.example")
                .map_err(|e| anyhow::anyhow!("client start: {e:?}"))?;
        Ok(Self {
            transport,
            socket,
            out: Egress::default(),
        })
    }

    async fn progress(&mut self) -> Result<()> {
        let now = Instant::now().into_std();
        let local = self.socket.local_addr();
        let mut more = false;
        self.out.stage(local, |out| {
            let (packet, pending) = self.transport.poll(now, out);
            more = pending;
            packet
        })?;
        match self.out.flush(&mut self.socket)? {
            Flush::PathMtu => anyhow::bail!("fixture path MTU"),
            Flush::Blocked => {
                self.socket.writable().await?;
                return Ok(());
            }
            Flush::Sent => return Ok(()),
            Flush::Empty if more => return Ok(()),
            Flush::Empty => {}
        }
        let deadline = self
            .transport
            .deadline()
            .map(Instant::from_std)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        pulsebeam_runtime::select! {
            ready = self.socket.readable() => ready?,
            () = pulsebeam_runtime::time::sleep_until(deadline) => return Ok(()),
        }
        match self.socket.try_recv_batch() {
            Ok(batch) => {
                for item in batch.iter() {
                    self.transport.receive(Instant::now().into_std(), item);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn admit(&mut self, admit: Admit<'_>) -> Result<(Admission, ControlStream)> {
        let mut admission =
            ClientAdmission::new(&admit, (Instant::now() + Duration::from_secs(5)).into_std())
                .map_err(|error| anyhow::anyhow!("invalid admission: {error:?}"))?;
        loop {
            if let Some(accepted) = admission
                .poll(self.transport.connection(), Instant::now().into_std())
                .map_err(|error| anyhow::anyhow!("admission failed: {error:?}"))?
            {
                return Ok(accepted);
            }
            self.progress().await?;
        }
    }

    async fn send(
        &mut self,
        control: &mut ControlStream,
        kind: Kind,
        body: &[u8],
        phase: Phase,
    ) -> Result<()> {
        control
            .offer(kind, body, phase)
            .map_err(|e| anyhow::anyhow!("offering {kind:?}: {e:?}"))?;
        loop {
            let result = control
                .write(self.transport.connection(), Instant::now().into_std())
                .map_err(|e| anyhow::anyhow!("writing {kind:?}: {e:?}"))?;
            if matches!(result, Write::Admitted { .. }) {
                return Ok(());
            }
            self.progress().await?;
        }
    }

    async fn receive(
        &mut self,
        control: &mut ControlStream,
        wanted: Kind,
        phase: Phase,
    ) -> Result<Frame> {
        loop {
            match control
                .read(
                    self.transport.connection(),
                    phase,
                    Instant::now().into_std(),
                )
                .map_err(|e| anyhow::anyhow!("reading {wanted:?}: {e:?}"))?
            {
                Read::Frame(frame) if frame.kind == wanted => return Ok(frame),
                Read::Frame(frame) if frame.kind == Kind::ServerState => {}
                Read::Pending => self.progress().await?,
                _ => anyhow::bail!("unexpected control input waiting for {wanted:?}"),
            }
        }
    }
}

async fn issue(
    address: SocketAddr,
    token: &str,
    continuation: Option<(&str, &[u8])>,
) -> Result<String> {
    let mut socket = TcpStream::connect(address).await?;
    let body = match continuation {
        Some((owner, successor)) => {
            serde_json::json!({"profile": "pulsebeam-rt-v1", "room": "stage", "owner": owner, "successor_receipt": URL_SAFE_NO_PAD.encode(successor)})
        }
        None => serde_json::json!({"profile": "pulsebeam-rt-v1", "room": "stage"}),
    };
    let body = serde_json::to_string(&body)?;
    let request = format!(
        "POST /api/v1/native/admissions HTTP/1.1\r\nHost: edge.example\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    socket.take(8_192).read_to_string(&mut response).await?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("admission HTTP response")?;
    ensure!(
        head.starts_with("HTTP/1.1 200"),
        "admission HTTP status: {}",
        head.lines().next().unwrap_or_default()
    );
    let result: serde_json::Value = serde_json::from_str(body)?;
    ensure!(
        result.get("profile").and_then(serde_json::Value::as_str) == Some("pulsebeam-rt-v1")
            && result
                .pointer("/authority/host")
                .and_then(serde_json::Value::as_str)
                == Some("edge.example")
    );
    Ok(result
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .context("ownership proof")?
        .to_owned())
}

#[test]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_types,
    reason = "fixture owns one node, bounded I/O and immutable TLS providers"
)]
fn dedicated_quic_listener_redeems_renews_and_closes() {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    pulsebeam_runtime::testing::run_local(host, async {
        let result = pulsebeam_runtime::time::timeout(Duration::from_secs(10), witness()).await;
        result.unwrap().unwrap();
    });
}

#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls require shared immutable configuration and clocks"
)]
async fn witness() -> Result<()> {
    let host = std::net::Ipv4Addr::LOCALHOST.into();
    let edge = SocketAddr::new(host, 8443);
    let http = TcpListener::bind("127.0.0.1:0".parse()?).await?;
    let address = http.local_addr()?;
    let identity = rcgen::generate_simple_self_signed(vec!["edge.example".to_owned()])?;
    let mut roots = RootCertStore::empty();
    roots.add(identity.cert.der().clone())?;
    let tls = transport::server_config(
        vec![identity.cert.der().clone()],
        PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
        Arc::new(DefaultTimeProvider),
        Arc::new(noq_proto::StdSystemTime),
        NonZeroUsize::new(64).context("incoming limit")?,
    )
    .map_err(|e| anyhow::anyhow!("server TLS configuration: {e:?}"))?;
    let keys = KeyRegistry::new(
        *DEVELOPMENT_PROJECT_ID,
        vec![KeyRegistryEntry {
            key_id: *DEVELOPMENT_API_KEY_ID,
            public_key: *DEVELOPMENT_API_PUBLIC_KEY,
        }],
    )?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let grants = pulsebeam_core::auth::grants::GrantSet::new(vec![
        pulsebeam_core::auth::grants::Grant::new(
            vec!["stage".to_owned()],
            vec![pulsebeam_core::auth::grants::Action::Join],
            None,
            None,
            None,
        )?,
    ])?;
    let key = pulsebeam_server::insecure_dev_api_key()?;
    let sign = |lifetime| {
        pulsebeam_server::sign_participant_token(
            &key,
            &grants,
            "alice",
            now.saturating_add(lifetime),
        )
    };
    let credential = sign(60)?.token;
    let renewal = sign(120)?.token;
    let (notify, mut ready) = pulsebeam_runtime::sync::mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let builder = NodeBuilder::new()
        .with_current_runtime()
        .local_addr(SocketAddr::new(host, 0))
        .advertise_bound_udp()
        .ice_tcp(IceTcpConfig::Disabled)
        .rng(rand::Rng::seed_from_u64(19))
        .with_http_api_listener(http)
        .with_key_registry(keys)
        .with_sim_startup_notification(notify)
        .quic_listeners(vec![QuicListener {
            bind: edge,
            host: "edge.example".to_owned(),
            port: edge.port(),
            tls,
        }]);
    let mut node = pulsebeam_runtime::task::spawn_local(builder.run(shutdown.clone()));
    pulsebeam_runtime::select! {
        notification = ready.recv() => ensure!(notification.is_some(), "node stopped before startup"),
        result = &mut node => anyhow::bail!("node stopped before startup: {result:?}"),
    }
    let result = async {
        let owner = issue(address, &credential, None).await?;
        let mut client = Client::new(edge, roots.clone()).await?;
        let (accepted, mut control) = client
            .admit(Admit {
                attempt: NonZeroU64::MIN,
                room: Some("stage"),
                owner: &owner,
                token: &credential,
                successor: None,
            })
            .await?;
        ensure!(accepted.room.as_deref() == Some("stage") && accepted.participant == "alice");
        let predecessor = accepted.connection;
        let successor = loop {
            let state = client
                .receive(&mut control, Kind::ServerState, Phase::Admitted)
                .await?;
            let batch = pulsebeam_proto::state::codec::decode_quic_server(&state.body)
                .map_err(|e| anyhow::anyhow!("state: {e:?}"))?;
            if let Some(receipt) =
                batch
                    .records
                    .into_iter()
                    .find_map(|record| match record.confirmed?.value? {
                        pulsebeam_proto::state::confirmed_state::Value::Session(state) => {
                            state.successor_receipt
                        }
                        _ => None,
                    })
            {
                break receipt.proof;
            }
        };
        let body = Renew {
            revision: NonZeroU64::MIN,
            token: &renewal,
        }
        .encode()
        .map_err(|e| anyhow::anyhow!("encoding renewal: {e:?}"))?;
        client
            .send(&mut control, Kind::Renew, &body, Phase::Admitted)
            .await?;
        let result = client
            .receive(&mut control, Kind::RenewResult, Phase::Admitted)
            .await?;
        let result = RenewResult::decode(&result.body)
            .map_err(|e| anyhow::anyhow!("renewal response: {e:?}"))?;
        let lease = result.accepted.context("renewal accepted")?;
        ensure!(
            result.request == NonZeroU64::MIN
                && lease.revision.get() == 2
                && lease.expires_unix_seconds.get() == now.saturating_add(120)
        );
        let issued = issue(address, &renewal, Some((&owner, &successor))).await?;
        ensure!(
            issued == owner,
            "continuation must not mint a fresh owner proof"
        );
        let mut next = Client::new(edge, roots).await?;
        let (accepted, mut control) = next
            .admit(Admit {
                attempt: NonZeroU64::new(2).context("valid attempt")?,
                room: Some("stage"),
                owner: &owner,
                token: &renewal,
                successor: Some(&successor),
            })
            .await?;
        ensure!(accepted.connection != predecessor && accepted.room.as_deref() == Some("stage"));
        while !client.transport.connection().is_closed() {
            client.progress().await?;
        }
        next.send(&mut control, Kind::Close, &[], Phase::Admitted)
            .await?;
        while !next.transport.connection().is_closed() {
            next.progress().await?;
        }
        Ok(())
    }
    .await;
    shutdown.cancel();
    node.await??;
    result
}
