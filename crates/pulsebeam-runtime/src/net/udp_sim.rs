//! Host network simulation, outside the reusable UDP I/O crate.
use super::{ReceiveTime, UdpMode, bound_udp::ReuseportMember, shaper};
use crate::sync::Arc;
use pulsebeam_udp::{
    Capabilities, SendDatagram,
    simulation::{Io, Metadata, Readiness},
};
use std::{io, net::SocketAddr, rc::Rc};
use turmoil::net::UdpSocket;

type Arrival = (Vec<u8>, SocketAddr, SocketAddr, ReceiveTime);
struct Network {
    socket: Arc<UdpSocket>,
    shaper: shaper::Shaper,
    wake: Rc<tokio::sync::Notify>,
}

impl Drop for Network {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

pub(super) struct HostIo {
    member: ReuseportMember,
    mode: UdpMode,
    pending: Option<Arrival>,
    network: Rc<Network>,
    local_addr: SocketAddr,
}

impl HostIo {
    pub(super) fn new(
        socket: Arc<UdpSocket>,
        member: ReuseportMember,
        mode: UdpMode,
    ) -> io::Result<Self> {
        let local_addr = socket.local_addr()?;
        let network = Rc::new(Network {
            socket,
            shaper: shaper::Shaper::default(),
            wake: Rc::new(tokio::sync::Notify::new()),
        });
        let weak = Rc::downgrade(&network);
        tokio::task::spawn_local(async move {
            loop {
                let (release, wake) = match weak.upgrade() {
                    Some(network) => (network.shaper.next_release(), network.wake.clone()),
                    None => break,
                };
                tokio::select! {
                    () = wake.notified() => {}
                    () = async {
                        match release {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending().await,
                        }
                    } => {}
                }
                let Some(network) = weak.upgrade() else { break };
                let mut shaper = network.shaper.clone();
                for (destination, payload) in shaper.drain_due(tokio::time::Instant::now()) {
                    // Preserve the host link's impairment stream: duplication is
                    // sampled only for pass-through sends, not queued releases.
                    if !shaper.should_drop_packet(destination.ip()) {
                        let _ = network.socket.try_send_to(&payload, destination);
                    }
                }
            }
        });
        Ok(Self {
            member,
            mode,
            pending: None,
            network,
            local_addr,
        })
    }
}

fn offer(socket: &UdpSocket, shaper: &mut shaper::Shaper, destination: SocketAddr, payload: &[u8]) {
    if let shaper::Shaped::PassThrough =
        shaper.offer(tokio::time::Instant::now(), destination, payload)
    {
        deliver(socket, shaper, destination, payload);
    }
}

fn deliver(
    socket: &UdpSocket,
    shaper: &mut shaper::Shaper,
    destination: SocketAddr,
    payload: &[u8],
) {
    // These are post-admission network effects. They must not change the I/O
    // report for an entry already admitted to the bounded simulated device.
    if shaper.should_drop_packet(destination.ip()) {
        return;
    }
    if shaper.should_duplicate_packet(destination.ip()) {
        let _ = socket.try_send_to(payload, destination);
    }
    let _ = socket.try_send_to(payload, destination);
}

impl Io for HostIo {
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    fn capabilities(&self) -> Capabilities {
        let batch = self.mode == UdpMode::Batch;
        Capabilities {
            max_batch_messages: if batch { pulsebeam_udp::BATCH_SIZE } else { 1 },
            gro: batch,
            max_gso_segments: if batch {
                pulsebeam_udp::MAX_GSO_SEGMENTS
            } else {
                1
            },
        }
    }
    fn readable(&self) -> Readiness<'_> {
        Box::pin(async {
            if self.pending.is_some() {
                Ok(())
            } else {
                self.member.readable().await
            }
        })
    }
    fn writable(&self) -> Readiness<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn receive(&mut self, payload: &mut [u8]) -> io::Result<Metadata> {
        let (bytes, source, destination, received_at) = self
            .pending
            .take()
            .map_or_else(|| self.member.try_recv(), Ok)?;
        let stride = bytes.len().max(1);
        let mut len = bytes.len();
        if let Some(target) = payload.get_mut(..len) {
            target.copy_from_slice(&bytes);
        }
        if self.capabilities().gro
            && shaper::gro_enabled(destination.ip())
            && !bytes.is_empty()
            && len <= payload.len()
        {
            for _ in 1..pulsebeam_udp::MAX_GSO_SEGMENTS {
                let next = match self.member.try_recv() {
                    Ok(next) => next,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                };
                if next.1 != source
                    || next.2 != destination
                    || next.3 != received_at
                    || next.0.is_empty()
                    || next.0.len() > stride
                    || len.saturating_add(next.0.len()) > payload.len()
                {
                    self.pending = Some(next);
                    break;
                }
                let end = len.saturating_add(next.0.len());
                if let Some(target) = payload.get_mut(len..end) {
                    target.copy_from_slice(&next.0);
                }
                len = end;
                if next.0.len() < stride {
                    break;
                }
            }
            let count = len.div_ceil(stride);
            if count > 1 {
                shaper::record_gro_batch(destination.ip(), count);
            }
        }
        Ok(Metadata {
            source,
            destination,
            len,
            stride,
            received_at,
        })
    }
    fn submit(&mut self, entry: &SendDatagram<'_>) -> io::Result<()> {
        if entry.destination.is_ipv4() != self.local_addr.is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "simulated socket destination family mismatch",
            ));
        }
        let stride = entry.gso_segment_size.map_or_else(
            || entry.payload.len().max(1),
            |size| usize::from(size.get()),
        );
        let segments = entry.payload.len().max(1).div_ceil(stride);
        if segments > 1 {
            shaper::record_gso_batch(entry.destination.ip(), segments);
        }
        // Admission is synchronous, as in Turmoil. An extra task queue would
        // introduce local loss and scheduling delay ahead of the modeled link.
        let mut shaper = self.network.shaper.clone();
        if entry.payload.is_empty() {
            offer(&self.network.socket, &mut shaper, entry.destination, &[]);
        }
        for payload in entry.payload.chunks(stride) {
            offer(
                &self.network.socket,
                &mut shaper,
                entry.destination,
                payload,
            );
        }
        if !shaper.is_empty() {
            self.network.wake.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_udp::{SendDatagram, SendStop};

    #[test]
    fn wildcard_destination_metadata_is_independent_of_advertisement() {
        for version in [turmoil::IpVersion::V4, turmoil::IpVersion::V6] {
            let mut sim = turmoil::Builder::new().ip_version(version).build();
            sim.client("receiver", async {
                let host_ip = turmoil::lookup("receiver");
                let (wildcard, loopback): (SocketAddr, std::net::IpAddr) = if host_ip.is_ipv4() {
                    ("0.0.0.0:0".parse().unwrap(), "127.0.0.1".parse().unwrap())
                } else {
                    ("[::]:0".parse().unwrap(), "::1".parse().unwrap())
                };
                let advertised = "198.51.100.19:9999".parse().unwrap();
                let mut receiver =
                    super::super::bind_udp_socket(wildcard, UdpMode::Batch, Some(advertised), 0)
                        .await?
                        .activate()?;
                assert!(receiver.local_addr().ip().is_unspecified());
                let sender = UdpSocket::bind(wildcard).await?;
                for (ip, size) in [(host_ip, 7), (loopback, 2048)] {
                    let destination = SocketAddr::new(ip, receiver.local_addr().port());
                    let payload = vec![7; size];
                    sender.send_to(&payload, destination).await?;
                    receiver.readable().await?;
                    let batch = receiver.try_recv_batch()?;
                    let datagram = batch.iter().next().unwrap();
                    assert_eq!(datagram.payload, payload.as_slice());
                    assert_eq!(datagram.source.ip(), ip);
                    assert_eq!(datagram.destination, destination);
                    assert_ne!(datagram.destination, advertised);
                    assert!(super::super::install_steering_flow(
                        datagram.source,
                        datagram.destination,
                        0
                    ));
                }
                Ok(())
            });
            sim.run().unwrap();
        }
    }

    #[test]
    fn empty_datagrams_apply_network_loss_after_device_admission() {
        let destination: SocketAddr = "198.18.0.42:9000".parse().unwrap();
        shaper::set_loss(destination.ip(), shaper::Loss::Independent(1.0));
        let dropped = shaper::stats(destination.ip()).dropped_loss;
        let mut sim = turmoil::Builder::new().build();
        sim.client("sender", async move {
            let mut socket = super::super::bind_udp_socket(
                "0.0.0.0:0".parse().unwrap(),
                UdpMode::Batch,
                None,
                0,
            )
            .await?
            .activate()?;
            let entries = [SendDatagram {
                destination,
                payload: &[],
                gso_segment_size: None,
            }; pulsebeam_udp::BATCH_SIZE];
            let report = socket.try_send_batch(&entries);
            assert_eq!(report.accepted, entries.len());
            assert!(report.stop.is_none());
            socket.writable().await?;
            assert_eq!(
                shaper::stats(destination.ip()).dropped_loss,
                dropped + entries.len() as u64
            );
            assert_eq!(socket.counters().submitted_bytes, 0);
            Ok(())
        });
        sim.run().unwrap();
        shaper::set_loss(destination.ip(), shaper::Loss::Independent(0.0));
    }

    #[test]
    fn unsupported_family_is_rejected_before_device_admission() {
        for version in [turmoil::IpVersion::V4, turmoil::IpVersion::V6] {
            let mut sim = turmoil::Builder::new().ip_version(version).build();
            sim.client("sender", async {
                let host_ip = turmoil::lookup("sender");
                let local: SocketAddr = if host_ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
                let mut socket = super::super::bind_udp_socket(local, UdpMode::Batch, None, 0)
                    .await?.activate()?;
                let wrong_family = if local.is_ipv4() { "[::1]:1234" } else { "127.0.0.1:1234" };
                let invalid = SendDatagram { destination: wrong_family.parse().unwrap(), payload: b"invalid", gso_segment_size: None };
                let report = socket.try_send_batch(&[invalid]);
                assert_eq!(report.accepted, 0);
                assert!(matches!(report.stop, Some(SendStop::Error(ref error)) if error.kind() == io::ErrorKind::InvalidInput));
                assert_eq!(socket.counters().submitted_datagrams, 0);
                let valid = SendDatagram { destination: SocketAddr::new(host_ip, socket.local_addr().port()), payload: b"valid", gso_segment_size: None };
                let report = socket.try_send_batch(&[valid, invalid]);
                assert_eq!(report.accepted, 1);
                assert!(matches!(report.stop, Some(SendStop::Error(_))));
                assert_eq!(socket.counters().submitted_datagrams, 1);
                Ok(())
            });
            sim.run().unwrap();
        }
    }
}
