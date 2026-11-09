use std::collections::{HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::task::Poll;

use pulsebeam_link::io::{NetworkDriver, SendOutcome};
use pulsebeam_link::{LinkTime, NetworkId, ReceiveTime};
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::time::{Duration, Instant};

use crate::shard::worker::ShardFrame;

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

const WORK: usize = 64;
const INBOUND: usize = 64;
pub(crate) const PEERS: usize = 64;
const DATAGRAM: usize = 1200;

pub(crate) enum Command {
    Install(Box<Install>),
    Inspect(
        pulsebeam_runtime::sync::oneshot::Sender<
            Result<super::HostHealth, pulsebeam_link::LinkError>,
        >,
    ),
}

pub(crate) struct Install {
    pub id: NetworkId,
    pub endpoint: SocketAddr,
    pub driver: NetworkDriver<ShardFrame>,
}

struct Peer {
    id: NetworkId,
    driver: NetworkDriver<ShardFrame>,
}

struct Pending {
    peer: NetworkId,
    source: SocketAddr,
    bytes: [u8; DATAGRAM],
    length: usize,
    observed: ReceiveTime,
}

impl Pending {
    fn receive(
        &self,
        peers: &mut HashMap<SocketAddr, Peer>,
    ) -> Option<Result<(), pulsebeam_link::io::Error>> {
        let payload = self.bytes.get(..self.length)?;
        // Endpoint reuse does not transfer retained input to a new association.
        peers
            .get_mut(&self.source)
            .filter(|peer| peer.id == self.peer)
            .map(|peer| peer.driver.receive(self.source, payload, self.observed))
    }
}

/// The sole socket receiver and sole owner of every association on that socket.
/// No identity or receive binding is inferred from incoming UDP or gossip.
pub(crate) struct Host {
    socket: pulsebeam_udp::UdpSocket,
    peers: HashMap<SocketAddr, Peer>,
    installs: pulsebeam_queue::Receiver<Command>,
    closed: VecDeque<super::PeerHealth>,
    pending: VecDeque<Pending>,
    origin: Instant,
    overflow: u64,
}

fn remember_closed(history: &mut VecDeque<super::PeerHealth>, health: super::PeerHealth) {
    if history.len() == PEERS {
        history.pop_front();
    }
    history.push_back(health);
}

impl Host {
    pub(crate) async fn bind(
        address: SocketAddr,
        origin: Instant,
    ) -> std::io::Result<(Self, pulsebeam_queue::Sender<Command>)> {
        let mut options = pulsebeam_udp::BindOptions::new(address);
        options.max_datagram_bytes = DATAGRAM;
        let socket = pulsebeam_udp::bind(options).await?.activate()?;
        let (sender, installs) = pulsebeam_queue::bounded(NonZeroUsize::MIN);
        Ok((
            Self {
                socket,
                peers: HashMap::new(),
                installs,
                closed: VecDeque::with_capacity(PEERS),
                pending: VecDeque::with_capacity(INBOUND),
                origin,
                overflow: 0,
            },
            sender,
        ))
    }

    fn ticks(&self, instant: Instant) -> LinkTime {
        LinkTime::from_nanos(
            u64::try_from(instant.saturating_duration_since(self.origin).as_nanos())
                .unwrap_or(u64::MAX),
        )
    }

    fn poll(&mut self) -> std::io::Result<()> {
        let instant = Instant::now();
        let now = self.ticks(instant);
        let socket = &mut self.socket;
        let closed = &mut self.closed;
        self.peers.retain(|endpoint, peer| {
            match peer.driver.poll(now, WORK, |datagram| {
                let sent = socket.try_send_batch(&[pulsebeam_udp::SendDatagram {
                    destination: datagram.destination,
                    payload: datagram.payload,
                    gso_segment_size: None,
                }]);
                if sent.accepted == 1 {
                    SendOutcome::Accepted {
                        bytes: datagram.payload.len(),
                    }
                } else {
                    SendOutcome::Rejected
                }
            }) {
                Ok(_) => true,
                Err(error) => {
                    remember_closed(
                        closed,
                        super::PeerHealth {
                            peer: peer.id,
                            endpoint: *endpoint,
                            transport: peer.driver.health(),
                            failure: Some(error),
                        },
                    );
                    tracing::debug!(?error, "Link association closed");
                    false
                }
            }
        });

        if let Ok(command) = self.installs.try_recv() {
            match command {
                Command::Install(install) => {
                    let mut install = *install;
                    // Closed owners are removed before their endpoint can be reused.
                    let result = if self.peers.contains_key(&install.endpoint) {
                        Err(pulsebeam_link::io::Error::Busy)
                    } else if self.peers.len() == PEERS {
                        Err(pulsebeam_link::io::Error::Capacity)
                    } else {
                        install.driver.start(now, instant.into_std())
                    };
                    match result {
                        Ok(()) => {
                            self.peers.insert(
                                install.endpoint,
                                Peer {
                                    id: install.id,
                                    driver: install.driver,
                                },
                            );
                        }
                        Err(error) => {
                            install.driver.close();
                            remember_closed(
                                &mut self.closed,
                                super::PeerHealth {
                                    peer: install.id,
                                    endpoint: install.endpoint,
                                    transport: install.driver.health(),
                                    failure: Some(error),
                                },
                            );
                        }
                    }
                }
                Command::Inspect(reply) => {
                    let _ = reply.send(Ok(self.health()));
                }
            }
        }

        for _ in 0..WORK {
            let Some(packet) = self.pending.front() else {
                break;
            };
            let result = packet.receive(&mut self.peers);
            if matches!(result, Some(Err(pulsebeam_link::io::Error::Busy))) {
                break;
            }
            if let Some(Err(error)) = result {
                if let Some(mut peer) = self.peers.remove(&packet.source) {
                    peer.driver.close();
                    remember_closed(
                        &mut self.closed,
                        super::PeerHealth {
                            peer: peer.id,
                            endpoint: packet.source,
                            transport: peer.driver.health(),
                            failure: Some(error),
                        },
                    );
                }
            }
            self.pending.pop_front();
        }
        // Finish processing the retained batch before reading another. Copies
        // release the receive arena so DTLS output can use this same socket.
        if self.pending.is_empty() {
            let origin = self.origin;
            match self.socket.try_recv_batch() {
                Ok(batch) => {
                    let mut packets = batch.iter();
                    for packet in packets.by_ref().take(INBOUND) {
                        let Some(peer) = self.peers.get(&packet.source) else {
                            continue;
                        };
                        if self.pending.len() == INBOUND || packet.payload.len() > DATAGRAM {
                            self.overflow = self.overflow.saturating_add(1);
                            continue;
                        }
                        let mut bytes = [0; DATAGRAM];
                        let Some(destination) = bytes.get_mut(..packet.payload.len()) else {
                            self.overflow = self.overflow.saturating_add(1);
                            continue;
                        };
                        destination.copy_from_slice(packet.payload);
                        self.pending.push_back(Pending {
                            peer: peer.id,
                            source: packet.source,
                            bytes,
                            length: packet.payload.len(),
                            observed: ReceiveTime {
                                monotonic: LinkTime::from_nanos(
                                    u64::try_from(
                                        packet
                                            .received_at
                                            .monotonic
                                            .saturating_duration_since(origin)
                                            .as_nanos(),
                                    )
                                    .unwrap_or(u64::MAX),
                                ),
                                wall: Some(packet.received_at.wall),
                            },
                        });
                    }
                    if packets.next().is_some() {
                        self.overflow = self.overflow.saturating_add(1);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn health(&self) -> super::HostHealth {
        let mut peers: Vec<_> = self
            .peers
            .iter()
            .map(|(endpoint, peer)| super::PeerHealth {
                peer: peer.id,
                endpoint: *endpoint,
                transport: peer.driver.health(),
                failure: None,
            })
            .collect();
        peers.extend(self.closed.iter().cloned());
        super::HostHealth {
            local_addr: self.socket.local_addr(),
            peers,
            overload_events: self.overflow,
        }
    }

    async fn ready(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            return Ok(());
        }
        let deadline = self
            .peers
            .values()
            .filter_map(|peer| peer.driver.next_deadline())
            .min_by_key(|at| at.as_nanos());
        let origin = self.origin;
        let timer = async move {
            match deadline.and_then(|at| origin.checked_add(Duration::from_nanos(at.as_nanos()))) {
                Some(at) => pulsebeam_runtime::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let peers = &mut self.peers;
        let wake = poll_fn(|cx| {
            for peer in peers.values_mut() {
                let mut ready = std::pin::pin!(peer.driver.readable());
                if ready.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        });
        pulsebeam_runtime::select! {
            result = self.socket.readable() => result,
            live = self.installs.readable() => if live { Ok(()) } else {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Link controller closed"))
            },
            _ = wake => Ok(()),
            _ = timer => Ok(()),
        }
    }

    pub(crate) async fn run(mut self, shutdown: CancellationToken) -> std::io::Result<()> {
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            self.poll()?;
            pulsebeam_runtime::select! {
                _ = shutdown.cancelled() => return Ok(()),
                result = self.ready() => if let Err(error) = result {
                    if shutdown.is_cancelled() { return Ok(()); }
                    return Err(error);
                },
            }
            // Bound work even when several associations remain immediately ready.
            pulsebeam_runtime::rt::yield_now().await;
        }
    }
}
