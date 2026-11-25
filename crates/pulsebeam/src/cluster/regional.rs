//! Dedicated IPv6 gRPC carrier for neighbor control frames. Fabric media does
//! not use this listener or its stream lifecycle.

use std::{
    net::SocketAddrV6,
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pulsebeam_core::identity::NodeId;

use pulsebeam_link::{LinkTime, PeerControl, StreamControl};
use pulsebeam_runtime::{
    net::tcp,
    sync::{CancellationToken, Semaphore, mpsc},
    task,
    time::Instant,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Request, Response, Status, Streaming,
    metadata::{Ascii, MetadataMap, MetadataValue},
    transport,
};

use super::{
    REGIONAL_FRAME_BYTES, RegionTree, RegionalControlConfig, config::read_identity_file,
    declarations::Role,
};

#[allow(
    clippy::disallowed_types,
    reason = "generated tonic RPC body and service ownership; not shard media state"
)]
mod wire {
    tonic::include_proto!("pulsebeam.cluster.v1");
}

use wire::{
    ControlFrame,
    neighbor_control_client::NeighborControlClient,
    neighbor_control_server::{NeighborControl, NeighborControlServer},
};

// Protobuf adds a single field tag and a length varint. The exact payload
// bound is checked again at the lane, before allocating mailbox space.
const MAX_MESSAGE_BYTES: usize = REGIONAL_FRAME_BYTES + 6;
const FRAME_LIMIT: NonZeroUsize = const { NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap() };
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

const ORIGIN: &str = "pulsebeam-origin";
const TARGET: &str = "pulsebeam-target";

type Generation = (NodeId, NonZeroU64);

#[derive(Clone)]
struct Binding {
    local: MetadataValue<Ascii>,
    peer: MetadataValue<Ascii>,
}

impl Binding {
    fn new(
        region: &str,
        local: (Generation, Role),
        peer: (Generation, Role),
    ) -> anyhow::Result<Self> {
        Self::with_regions((region, region), None, local, peer)
    }

    fn with_regions(
        regions: (&str, &str),
        tree: Option<&[u8; 32]>,
        local: (Generation, Role),
        peer: (Generation, Role),
    ) -> anyhow::Result<Self> {
        let tree = tree.map(|hash| URL_SAFE_NO_PAD.encode(hash));
        let encode = |region: &str, ((node, generation), role): (Generation, Role)| {
            let role = role.wire_name();
            match &tree {
                Some(tree) => format!("{tree}/{region}/{role}/{node}/{generation}"),
                None => format!("{region}/{role}/{node}/{generation}"),
            }
            .parse()
        };
        Ok(Self {
            local: encode(regions.0, local)?,
            peer: encode(regions.1, peer)?,
        })
    }

    fn matches(&self, metadata: &MetadataMap) -> bool {
        metadata.get(ORIGIN) == Some(&self.peer) && metadata.get(TARGET) == Some(&self.local)
    }

    fn write(&self, metadata: &mut MetadataMap) {
        metadata.insert(ORIGIN, self.local.clone());
        metadata.insert(TARGET, self.peer.clone());
    }
}

pub(crate) enum Adjacency {
    Listen {
        listener: Box<NeighborListener>,
        peer_certificate: Box<[u8]>,
    },
    Dial(Box<NeighborDialer>),
}

impl Adjacency {
    pub(crate) async fn open(
        config: RegionalControlConfig,
        local: Generation,
        peer: Generation,
        tree: Option<&RegionTree>,
    ) -> anyhow::Result<Self> {
        let initiator = local.0 < peer.0;
        let identities = ((local, config.role), (peer, config.peer_role));
        let binding = if let Some(tree) = tree {
            let scope = config.scoped(tree, local, peer)?;
            Binding::with_regions(
                (scope.local_region, scope.peer_region),
                Some(tree.fingerprint()),
                identities.0,
                identities.1,
            )?
        } else {
            Binding::new(&config.region, identities.0, identities.1)?
        };
        let (identity, ca, config, peer_certificate) =
            task::spawn_blocking(move || -> anyhow::Result<_> {
                let identity = transport::Identity::from_pem(
                    read_identity_file(&config.certificate_pem)?,
                    read_identity_file(&config.private_key_pem)?,
                );
                let ca = transport::Certificate::from_pem(read_identity_file(&config.peer_ca_pem)?);
                let peer_certificate = if initiator {
                    None
                } else {
                    Some(read_identity_file(&config.peer_certificate_der)?.into_boxed_slice())
                };
                Ok((identity, ca, config, peer_certificate))
            })
            .await??;
        if let Some(peer_certificate) = peer_certificate {
            let listen = RegionalControlConfig::ipv6(config.listen, true)?;
            let listener = NeighborListener::bind(listen, identity, ca, binding).await?;
            Ok(Self::Listen {
                listener: Box::new(listener),
                peer_certificate,
            })
        } else {
            let endpoint = RegionalControlConfig::ipv6(config.endpoint, false)?;
            Ok(Self::Dial(Box::new(NeighborDialer::new(
                endpoint,
                config.peer_name,
                ca,
                identity,
                binding,
            )?)))
        }
    }

    pub(crate) async fn run(
        self,
        origin: Instant,
        sessions: mpsc::Sender<PeerControl>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        match self {
            Self::Listen {
                listener,
                peer_certificate,
            } => {
                listener
                    .serve(peer_certificate, origin, sessions, shutdown)
                    .await?;
            }
            Self::Dial(dialer) => loop {
                if let Err(error) = dialer.run(origin, &sessions, shutdown.clone()).await {
                    tracing::debug!(%error, "regional control connection closed");
                }
                pulsebeam_runtime::select! {
                    _ = shutdown.cancelled() => break,
                    _ = sessions.closed() => break,
                    _ = pulsebeam_runtime::time::sleep(RECONNECT_DELAY) => {},
                }
            },
        }
        Ok(())
    }
}

pub(crate) struct NeighborListener {
    listener: tcp::TcpListener,
    server: transport::Server,
    binding: Binding,
}

impl NeighborListener {
    async fn bind(
        listen: SocketAddrV6,
        identity: transport::Identity,
        client_ca: transport::Certificate,
        binding: Binding,
    ) -> anyhow::Result<Self> {
        let server = transport::Server::builder()
            .tls_config(
                transport::ServerTlsConfig::new()
                    .identity(identity)
                    .client_ca_root(client_ca)
                    .timeout(HANDSHAKE_TIMEOUT),
            )?
            .max_concurrent_streams(Some(1));
        Ok(Self {
            listener: tcp::bind_ipv6(listen).await?,
            server,
            binding,
        })
    }

    pub(crate) async fn serve(
        mut self,
        expected_client_cert_der: Box<[u8]>,
        origin: Instant,
        sessions: mpsc::Sender<PeerControl>,
        shutdown: CancellationToken,
    ) -> Result<(), transport::Error> {
        let service = Service::new(
            expected_client_cert_der,
            self.binding,
            origin,
            sessions,
            shutdown.clone(),
        );
        let service = NeighborControlServer::new(service)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        self.server
            .add_service(service)
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(self.listener),
                shutdown.cancelled_owned(),
            )
            .await
    }
}

#[derive(Clone)]
pub(crate) struct NeighborDialer {
    endpoint: transport::Endpoint,
    binding: Binding,
}

impl NeighborDialer {
    fn new(
        remote: SocketAddrV6,
        expected_name: String,
        remote_ca: transport::Certificate,
        identity: transport::Identity,
        binding: Binding,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            remote.port() != 0
                && remote.scope_id() == 0
                && !remote.ip().is_unspecified()
                && !remote.ip().is_multicast()
                && !remote.ip().is_unicast_link_local()
                && remote.ip().to_ipv4_mapped().is_none(),
            "neighbor control requires a concrete IPv6 unicast endpoint"
        );
        let endpoint = transport::Endpoint::from_shared(format!("https://{remote}"))?
            .connect_timeout(HANDSHAKE_TIMEOUT)
            .tls_config(
                transport::ClientTlsConfig::new()
                    .domain_name(expected_name)
                    .ca_certificate(remote_ca)
                    .identity(identity)
                    .timeout(HANDSHAKE_TIMEOUT),
            )?;
        Ok(Self { endpoint, binding })
    }

    pub(crate) async fn run(
        &self,
        origin: Instant,
        sessions: &mpsc::Sender<PeerControl>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel(1);
        let open = async {
            let channel = self.endpoint.connect().await?;
            let mut client = NeighborControlClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES);
            let mut request = Request::new(ReceiverStream::new(rx));
            self.binding.write(request.metadata_mut());
            let response = client.exchange(request).await?;
            if !self.binding.matches(response.metadata()) {
                return Err(Status::failed_precondition(
                    "neighbor origin does not match provisioning",
                )
                .into());
            }
            Ok::<_, anyhow::Error>(response.into_inner())
        };
        let rx = pulsebeam_runtime::select! {
            result = pulsebeam_runtime::time::timeout(HANDSHAKE_TIMEOUT, open) => result??,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let admission = sessions
            .try_reserve()
            .map_err(|_| anyhow::anyhow!("controller cannot accept a stream"))?;
        let (peer, stream) = StreamControl::new_authenticated(FRAME_LIMIT);
        admission.send(peer);
        drive(stream, rx, &tx, std::convert::identity, origin, shutdown).await?;
        Ok(())
    }
}

struct Service {
    expected_client_cert_der: Box<[u8]>,
    binding: Binding,
    origin: Instant,
    sessions: mpsc::Sender<PeerControl>,
    shutdown: CancellationToken,
    #[allow(
        clippy::disallowed_types,
        reason = "one permit fences concurrent control RPCs, outside the media path"
    )]
    active: std::sync::Arc<Semaphore>,
}

impl Service {
    #[allow(
        clippy::disallowed_types,
        reason = "one permit fences concurrent control RPCs, outside the media path"
    )]
    fn new(
        expected_client_cert_der: Box<[u8]>,
        binding: Binding,
        origin: Instant,
        sessions: mpsc::Sender<PeerControl>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            expected_client_cert_der,
            binding,
            origin,
            sessions,
            shutdown,
            active: std::sync::Arc::new(Semaphore::new(1)),
        }
    }
}

#[tonic::async_trait]
impl NeighborControl for Service {
    type ExchangeStream = ReceiverStream<Result<ControlFrame, Status>>;

    async fn exchange(
        &self,
        request: Request<Streaming<ControlFrame>>,
    ) -> Result<Response<Self::ExchangeStream>, Status> {
        let certs = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        if certs
            .first()
            .is_none_or(|leaf| leaf.as_ref() != self.expected_client_cert_der.as_ref())
        {
            return Err(Status::permission_denied(
                "client is not the pinned neighbor",
            ));
        }
        if !self.binding.matches(request.metadata()) {
            return Err(Status::failed_precondition(
                "neighbor origin does not match provisioning",
            ));
        }
        let permit =
            self.active.clone().try_acquire_owned().map_err(|_| {
                Status::resource_exhausted("neighbor control stream is already active")
            })?;
        let admission = self
            .sessions
            .try_reserve()
            .map_err(|_| Status::unavailable("controller cannot accept a stream"))?;
        let (peer, stream) = StreamControl::new_authenticated(FRAME_LIMIT);
        let (send, receive) = mpsc::channel(1);
        admission.send(peer);
        let origin = self.origin;
        let shutdown = self.shutdown.clone();
        task::spawn(async move {
            let _permit = permit;
            let status = drive(stream, request.into_inner(), &send, Ok, origin, shutdown).await;
            if let Err(status) = status {
                let _ = send.try_send(Err(status));
            }
        });
        let mut response = Response::new(ReceiverStream::new(receive));
        self.binding.write(response.metadata_mut());
        Ok(response)
    }
}

async fn drive<T>(
    mut stream: StreamControl,
    mut rx: Streaming<ControlFrame>,
    tx: &mpsc::Sender<T>,
    envelope: impl Fn(ControlFrame) -> T,
    origin: Instant,
    shutdown: CancellationToken,
) -> Result<(), Status> {
    let (outbound, inbound) = stream.directions();
    let receive = async {
        while let Some(frame) = rx.message().await? {
            inbound
                .send(frame.payload.into_boxed_slice())
                .await
                .map_err(|error| match error {
                    pulsebeam_link::SendErrorKind::TooLarge => {
                        Status::resource_exhausted("control frame exceeds limit")
                    }
                    _ => Status::unavailable("controller closed the stream"),
                })?;
        }
        Ok(())
    };
    let send = async {
        while let Some((frame, deadline)) = outbound.recv().await {
            let Ok(slot) = tx.reserve().await else {
                break;
            };
            let elapsed = Instant::now().saturating_duration_since(origin).as_nanos();
            if deadline > LinkTime::from_nanos(u64::try_from(elapsed).unwrap_or(u64::MAX)) {
                slot.send(envelope(ControlFrame {
                    payload: frame.into_vec(),
                }));
            }
        }
        Ok(())
    };
    pulsebeam_runtime::select! {
        result = receive => result,
        result = send => result,
        _ = tx.closed() => Ok(()),
        _ = shutdown.cancelled() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    #[test]
    fn border_binding_pins_both_regions_and_exact_tree() {
        let a = ((NodeId::new(), NonZeroU64::MIN), Role::Gateway);
        let b = ((NodeId::new(), NonZeroU64::MIN), Role::Gateway);
        let sender = Binding::with_regions(("child", "parent"), Some(&[1; 32]), a, b).unwrap();
        let mut metadata = MetadataMap::new();
        sender.write(&mut metadata);
        let matching = Binding::with_regions(("parent", "child"), Some(&[1; 32]), b, a).unwrap();
        assert!(matching.matches(&metadata));
        let wrong_tree = Binding::with_regions(("parent", "child"), Some(&[2; 32]), b, a).unwrap();
        let wrong_border =
            Binding::with_regions(("parent", "sibling"), Some(&[1; 32]), b, a).unwrap();
        let unscoped = Binding::new("parent", b, a).unwrap();
        assert!(!wrong_tree.matches(&metadata));
        assert!(!wrong_border.matches(&metadata));
        assert!(!unscoped.matches(&metadata));
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn ipv6_tls_exchanges_only_with_the_pinned_neighbor() {
        pulsebeam_runtime::time::timeout(Duration::from_secs(5), async {
            let mut ca_params = CertificateParams::default();
            ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let ca_key = KeyPair::generate().unwrap();
            let ca =
                transport::Certificate::from_pem(ca_params.self_signed(&ca_key).unwrap().pem());
            let issuer = Issuer::from_params(&ca_params, ca_key);
            let issue = |usage| {
                let mut params = CertificateParams::new(vec!["neighbor.test".to_owned()]).unwrap();
                params.extended_key_usages.push(usage);
                let key = KeyPair::generate().unwrap();
                let cert = params.signed_by(&key, &issuer).unwrap();
                (
                    transport::Identity::from_pem(cert.pem(), key.serialize_pem()),
                    Box::<[u8]>::from(cert.der().as_ref()),
                )
            };
            let (server_identity, _) = issue(ExtendedKeyUsagePurpose::ServerAuth);
            let (client_identity, client_der) = issue(ExtendedKeyUsagePurpose::ClientAuth);
            let (other_identity, _) = issue(ExtendedKeyUsagePurpose::ClientAuth);
            let source = (NodeId::new(), NonZeroU64::MIN);
            let destination = (NodeId::new(), NonZeroU64::MIN);
            let source = (source, Role::Edge);
            let destination = (destination, Role::Router);
            let region = ("test-region", "test-region");
            let realm = [1; 32];
            let client_binding =
                Binding::with_regions(region, Some(&realm), source, destination).unwrap();
            let listener = NeighborListener::bind(
                "[::1]:0".parse().unwrap(),
                server_identity,
                ca.clone(),
                Binding::with_regions(region, Some(&realm), destination, source).unwrap(),
            )
            .await
            .unwrap();
            let std::net::SocketAddr::V6(remote) = listener.listener.local_addr().unwrap() else {
                panic!("expected IPv6 listener");
            };
            let origin = Instant::now();
            let shutdown = CancellationToken::new();
            let (sessions, mut accepted) = mpsc::channel(1);
            let task = task::spawn(listener.serve(client_der, origin, sessions, shutdown.clone()));
            let dial = |name: &str, identity| {
                NeighborDialer::new(
                    remote,
                    name.to_owned(),
                    ca.clone(),
                    identity,
                    client_binding.clone(),
                )
                .unwrap()
            };
            let (sessions, mut connected) = mpsc::channel(1);
            assert!(
                NeighborDialer::new(
                    "[::ffff:127.0.0.1]:1".parse().unwrap(),
                    "neighbor.test".to_owned(),
                    ca.clone(),
                    client_identity.clone(),
                    client_binding.clone(),
                )
                .is_err()
            );
            assert!(
                dial("wrong.test", client_identity.clone())
                    .run(origin, &sessions, shutdown.clone())
                    .await
                    .is_err()
            );
            let denied = dial("neighbor.test", other_identity)
                .run(origin, &sessions, shutdown.clone())
                .await
                .unwrap_err();
            assert_eq!(
                denied.downcast_ref::<Status>().unwrap().code(),
                tonic::Code::PermissionDenied
            );
            let rotated = NonZeroU64::new(2).unwrap();
            for binding in [
                Binding::with_regions(
                    region,
                    Some(&realm),
                    (((source.0).0, rotated), source.1),
                    destination,
                )
                .unwrap(),
                Binding::with_regions(
                    region,
                    Some(&realm),
                    source,
                    (((destination.0).0, rotated), destination.1),
                )
                .unwrap(),
                Binding::with_regions(
                    (region.0, "wrong-region"),
                    Some(&realm),
                    source,
                    destination,
                )
                .unwrap(),
                Binding::with_regions(region, Some(&realm), (source.0, Role::Gateway), destination)
                    .unwrap(),
                Binding::with_regions(region, Some(&[2; 32]), source, destination).unwrap(),
                Binding::new(region.0, source, destination).unwrap(),
            ] {
                let mut stale = dial("neighbor.test", client_identity.clone());
                stale.binding = binding;
                let denied = stale
                    .run(origin, &sessions, shutdown.clone())
                    .await
                    .unwrap_err();
                assert_eq!(
                    denied.downcast_ref::<Status>().unwrap().code(),
                    tonic::Code::FailedPrecondition
                );
            }
            assert!(matches!(
                accepted.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            assert!(matches!(
                connected.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));

            let dialer = dial("neighbor.test", client_identity);
            let dial_shutdown = CancellationToken::new();
            let connection = task::spawn({
                let dialer = dialer.clone();
                let sessions = sessions.clone();
                let shutdown = dial_shutdown.clone();
                async move { dialer.run(origin, &sessions, shutdown).await }
            });
            let mut peer = accepted.recv().await.unwrap();
            let mut client = connected.recv().await.unwrap();
            assert!(peer.is_ready());
            let live = LinkTime::from_nanos(u64::MAX);
            assert!(
                client
                    .try_send(Box::from([0]), LinkTime::from_nanos(0))
                    .is_ok()
            );
            assert!(client.try_send(Box::from([1]), live).is_ok());
            assert_eq!(peer.recv().await.unwrap().as_ref(), [1]);
            let large = vec![2; REGIONAL_FRAME_BYTES].into_boxed_slice();
            assert!(peer.try_send(large.clone(), live).is_ok());
            assert_eq!(client.recv().await.unwrap(), large);
            let duplicate = dialer
                .run(origin, &sessions, shutdown.clone())
                .await
                .unwrap_err();
            assert_eq!(
                duplicate.downcast_ref::<Status>().unwrap().code(),
                tonic::Code::ResourceExhausted
            );
            dial_shutdown.cancel();
            connection.await.unwrap().unwrap();
            assert!(client.is_closed());
            assert_eq!(peer.recv().await, None);
            assert!(peer.is_closed());

            let replacement = task::spawn(Adjacency::Dial(Box::new(dialer)).run(
                origin,
                sessions,
                shutdown.child_token(),
            ));
            let mut next_peer = accepted.recv().await.unwrap();
            let next_client = connected.recv().await.unwrap();
            assert!(next_client.try_send(Box::from([3]), live).is_ok());
            assert_eq!(next_peer.recv().await.unwrap().as_ref(), [3]);
            assert!(client.is_closed() && peer.is_closed());
            shutdown.cancel();
            replacement.await.unwrap().unwrap();
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}
