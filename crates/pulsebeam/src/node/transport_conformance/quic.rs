//! Kernel loopback witness for the same Noq packet engines used by the Agent and shard.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    num::{NonZeroU16, NonZeroUsize},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use noq_proto::{FourTuple, PathId, Transmit, rustls};
use pulsebeam_agent_quic::{
    records::PacketFraming,
    transport::{
        self, NativeDatagram, Readiness,
        client::ClientTransport,
        framing::{Receiver, Sender, WHOLE_CAPACITY},
        listener::Listener,
    },
};
use pulsebeam_udp::ReceivedDatagram;
use rustls::{RootCertStore, pki_types::PrivatePkcs8KeyDer, time_provider::DefaultTimeProvider};

const AUTHORITY: &str = "edge.example";
const PAYLOAD_BYTES: usize = WHOLE_CAPACITY + 1;
const BINDING: NonZeroU16 = NonZeroU16::MIN;

fn received_packet(
    connection: &mut noq_proto::Connection,
    receiver: &mut Receiver,
    now: Instant,
    expected: &[u8],
) -> bool {
    transport::read_native_datagram(connection, |datagram| {
        let NativeDatagram::Packet {
            binding: BINDING,
            wire,
        } = datagram
        else {
            return false;
        };
        receiver
            .receive(now, wire)
            .is_ok_and(|complete| complete.is_some_and(|bytes| bytes.as_ref() == expected))
    })
    .is_some_and(|result| result == Ok(true))
}

fn bind(ip: IpAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn send(socket: &UdpSocket, packet: Transmit, bytes: &[u8]) -> Result<()> {
    ensure!(packet.segment_size.is_none());
    let bytes = bytes.get(..packet.size).context("QUIC transmit length")?;
    ensure!(bytes.len() <= usize::from(transport::UDP_PAYLOAD_BYTES));
    ensure!(socket.send_to(bytes, packet.destination)? == bytes.len());
    Ok(())
}

fn receive(
    socket: &UdpSocket,
    mut consume: impl FnMut(SocketAddr, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut bytes = [0; 2048];
    for _ in 0..16 {
        match socket.recv_from(&mut bytes) {
            Ok((len, source)) => consume(source, bytes.get(..len).context("UDP receive length")?)?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls require shared immutable TLS configuration"
)]
fn witness(ip: IpAddr) -> Result<()> {
    let identity = rcgen::generate_simple_self_signed(vec![AUTHORITY.into()])?;
    let mut roots = RootCertStore::empty();
    roots.add(identity.cert.der().clone())?;
    let tls = transport::server_config(
        vec![identity.cert.der().clone()],
        PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
        Arc::new(DefaultTimeProvider),
        Arc::new(noq_proto::StdSystemTime),
        NonZeroUsize::MIN,
    )
    .map_err(|error| anyhow!("server TLS: {error:?}"))?;
    let config = transport::client_config(roots, Arc::new(DefaultTimeProvider))
        .map_err(|error| anyhow!("client TLS: {error:?}"))?;
    let server_socket = bind(ip)?;
    let mut socket = bind(ip)?;
    let server_addr = server_socket.local_addr()?;
    let mut local = socket.local_addr()?;
    let mut listener = Listener::new(tls)?;
    let mut now = Instant::now();
    let mut client = ClientTransport::connect(now, config, server_addr, AUTHORITY)
        .map_err(|error| anyhow!("client start: {error:?}"))?;
    let mut out = Vec::new();
    let mut association = None;
    let framing =
        PacketFraming::negotiate(1).map_err(|error| anyhow!("packet framing: {error:?}"))?;
    let mut upload = Sender::new(framing, BINDING);
    let mut echo = Sender::new(framing, BINDING);
    let mut server_receive = Receiver::new(framing, BINDING);
    let mut client_receive = Receiver::new(framing, BINDING);

    // Drive Noq's clock deterministically. The UDP traffic itself goes through
    // the kernel, with distinct source ports for both rebinding modes.
    for (phase, value) in [0x61, 0x62, 0x63].into_iter().enumerate() {
        if phase > 0 {
            let replacement = bind(ip)?;
            let next = replacement.local_addr()?;
            ensure!(local.port() != next.port());
            socket = replacement;
            local = next;
            if phase == 2 {
                client.connection().handle_network_change(None, now);
            }
        }
        let payload = [value; PAYLOAD_BYTES];
        upload
            .start(now, Box::from(payload.as_slice()))
            .map_err(|error| anyhow!("starting client packet: {error:?}"))?;
        let (mut server_received, mut downloaded) = (false, false);
        for _ in 0..1200 {
            now += Duration::from_millis(1);
            if upload.is_pending()
                && transport::data_readiness(client.connection()) == Readiness::Ready
            {
                client
                    .offer_packet(&mut upload, now)
                    .map_err(|error| anyhow!("client packet admission: {error:?}"))?;
            }
            if let (Some(packet), _) = client.poll(now, &mut out) {
                send(&socket, packet, &out)?;
            }
            receive(&server_socket, |source, bytes| {
                if let Some(packet) =
                    listener.receive(now, FourTuple::new(source, Some(ip)), None, bytes, &mut out)
                {
                    send(&server_socket, packet, &out)?;
                }
                Ok(())
            })?;
            listener.begin_turn();
            let mut offered = Ok(());
            if let Some((handle, packet)) = listener.poll_one(now, &mut out, |_, peer| {
                if transport::data_readiness(&mut peer.connection) != Readiness::Ready {
                    return;
                }
                if !server_received
                    && received_packet(&mut peer.connection, &mut server_receive, now, &payload)
                {
                    server_received = true;
                    if let Err(error) = echo.start(now, Box::from(payload.as_slice())) {
                        offered = Err(anyhow!("starting server packet: {error:?}"));
                        return;
                    }
                }
                if echo.is_pending()
                    && let Err(error) = echo.offer(&mut peer.scheduler, &mut peer.connection, now)
                {
                    offered = Err(anyhow!("server packet admission: {error:?}"));
                }
            }) {
                offered?;
                ensure!(*association.get_or_insert(handle) == handle);
                if let Some(packet) = packet {
                    send(&server_socket, packet, &out)?;
                }
            }
            receive(&socket, |source, bytes| {
                client.receive(
                    now,
                    ReceivedDatagram {
                        source,
                        destination: local,
                        payload: bytes,
                        received_at: pulsebeam_runtime::net::ReceiveTime::now(),
                        ecn: None,
                    },
                );
                Ok(())
            })?;
            downloaded |= received_packet(client.connection(), &mut client_receive, now, &payload);
            let selected = association
                .and_then(|handle| listener.get(handle))
                .and_then(|peer| peer.connection.network_path(PathId::ZERO).ok())
                .is_some_and(|path| path.remote() == local);
            if downloaded && selected {
                break;
            }
        }
        ensure!(
            downloaded && server_received,
            "missing bounded bidirectional QUIC payload for {ip}, phase {phase}"
        );
        let handle = association.context("live server association")?;
        ensure!(listener.association_count() == 1);
        ensure!(
            listener
                .get(handle)
                .and_then(|peer| peer.connection.network_path(PathId::ZERO).ok())
                .is_some_and(|path| path.remote() == local),
            "server did not select rebound address {local}"
        );
    }
    Ok(())
}

#[test]
fn native_quic_kernel_v4_v6_preserve_association_on_rebind() -> Result<()> {
    witness(Ipv4Addr::LOCALHOST.into())?;
    witness(Ipv6Addr::LOCALHOST.into())
}
