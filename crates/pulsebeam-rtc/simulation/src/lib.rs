#![allow(clippy::expect_used, reason = "controlled test world invariants")]

pub use pulsebeam_webrtc_sys::*;
mod h264_fixture;
pub use h264_fixture::{
    NativeH264AccessUnit, NativeH264FixtureEncoding, stock_h264_fixture,
    stock_h264_fixture_for_encoding, stock_h264_solid_fixture,
};

use std::{
    cell::RefCell,
    collections::BTreeMap,
    net::IpAddr,
    rc::{Rc, Weak},
    sync::MutexGuard,
    time::{Duration, Instant},
};

#[allow(
    clippy::disallowed_types,
    reason = "test-only lease serializes libwebrtc's process-global controlled driver"
)]
static LEASE: std::sync::Mutex<()> = std::sync::Mutex::new(());
thread_local! {
    static WORLD: RefCell<Weak<SimulationWorld>> = const { RefCell::new(Weak::new()) };
}

/// A shared caller-thread world for every peer participating in one scenario.
/// The native driver is process-exclusive, so parallel Rust tests serialize
/// world ownership, not protocol time or packet delivery.
pub struct SimulationWorld {
    pub controlled: ControlledWorld,
    clock_origin: RefCell<Option<(Instant, Duration)>>,
    _lease: MutexGuard<'static, ()>,
}

impl SimulationWorld {
    pub fn acquire() -> Rc<Self> {
        WORLD.with(|slot| {
            if let Some(world) = slot.borrow().upgrade() {
                return world;
            }
            // The mutex protects exclusivity, not state. Native RAII releases the world even on test panic.
            let lease = LEASE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let world = Rc::new(Self {
                controlled: ControlledWorld::acquire(11, Duration::from_secs(10))
                    .expect("pinned controlled native engine"),
                clock_origin: RefCell::new(None),
                _lease: lease,
            });
            *slot.borrow_mut() = Rc::downgrade(&world);
            world
        })
    }

    pub fn bind_clock(&self, origin: Instant) {
        self.clock_origin
            .borrow_mut()
            .get_or_insert((origin, self.controlled.now()));
    }

    pub fn native_time_at(&self, at: Instant) -> Duration {
        let (origin, native_origin) = self.clock_origin.borrow().expect("bound scenario clock");
        native_origin + at.duration_since(origin)
    }

    pub fn monotonic_now(&self) -> Instant {
        let (origin, native_origin) = self.clock_origin.borrow().expect("bound scenario clock");
        origin + self.controlled.now().saturating_sub(native_origin)
    }

    pub fn advance_clock_to(&self, at: Instant) {
        self.advance_to(self.native_time_at(at));
    }

    pub fn pump(&self) {
        self.controlled.pump(512);
    }

    pub fn advance_to(&self, at: Duration) {
        if at > self.controlled.now() {
            self.controlled
                .advance(at - self.controlled.now())
                .expect("forward simulated time");
        }
        self.pump();
    }
}

/// Packet-only adapter between controlled native endpoints and an external
/// simulated network. The external network still carries the original native
/// ICE/DTLS/SRTP/RTCP bytes. No media, signaling or clock policy lives here.
///
/// Keep this adapter alive alongside its runtime factories so the process lease
/// outlives every graph. All methods stay on the world's caller thread.
pub struct PacketBridge {
    network: ControlledSimulatedNetwork,
    peers: BTreeMap<IpAddr, Weak<NetworkEndpoint>>,
    external_endpoints: BTreeMap<IpAddr, NetworkEndpoint>,
    external_udp: BTreeMap<NetworkAddress, SimulatedUdpSocket>,
    // Last: the process lease must outlive the network and adapter sockets.
    pub world: Rc<SimulationWorld>,
}

/// One external transport decision. Dropping it rejects the pending native
/// decision, including when a bounded transport queue or task is cancelled.
pub struct ExternalPacket {
    packet: OutboundPacket,
    network: ControlledSimulatedNetwork,
    _world: Rc<SimulationWorld>,
    decided: bool,
}

impl ExternalPacket {
    pub fn packet(&self) -> &OutboundPacket {
        &self.packet
    }

    /// Admit a TCP connection only after the external stream has connected.
    /// Datagram/data bytes have left this network through the external adapter.
    pub fn accept(mut self) -> Result<(), NetworkError> {
        match self.packet.kind {
            OutboundKind::TcpConnect => self.network.deliver(self.packet.id)?,
            OutboundKind::Udp | OutboundKind::TcpData => {
                self.network.drop_packet(self.packet.id)?
            }
        }
        self.decided = true;
        Ok(())
    }
}

impl Drop for ExternalPacket {
    fn drop(&mut self) {
        if !self.decided {
            // Graph/socket retirement may already have removed the decision.
            let _ = self.network.drop_packet(self.packet.id);
        }
    }
}

#[derive(Debug)]
pub enum PacketBridgeError {
    Network(NetworkError),
    ExternalSocketLimit,
}
impl From<NetworkError> for PacketBridgeError {
    fn from(error: NetworkError) -> Self {
        Self::Network(error)
    }
}
impl std::fmt::Display for PacketBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(error) => error.fmt(f),
            Self::ExternalSocketLimit => f.write_str("external UDP adapter socket limit reached"),
        }
    }
}
impl std::error::Error for PacketBridgeError {}

pub struct PacketBridgeTurn {
    /// Decisions remain owned until transport admission or rejection.
    pub outgoing: Vec<ExternalPacket>,
    /// An ordinary network outcome when a receiver socket has already closed.
    pub closed_destinations: usize,
    /// The budget was exhausted. Yield and schedule another bounded turn.
    pub budget_exhausted: bool,
}

impl PacketBridge {
    pub fn new(world: Rc<SimulationWorld>) -> Result<Self, NetworkError> {
        let network = world.controlled.create_network()?;
        Ok(Self {
            world,
            network,
            peers: BTreeMap::new(),
            external_endpoints: BTreeMap::new(),
            external_udp: BTreeMap::new(),
        })
    }

    /// Peers on one simulated host share its interface, not its RTC graph.
    /// Weak entries do not retain retired endpoints or connection generations.
    pub fn endpoint(&mut self, ip: IpAddr) -> Result<Rc<NetworkEndpoint>, NetworkError> {
        if let Some(endpoint) = self.peers.get(&ip).and_then(Weak::upgrade) {
            return Ok(endpoint);
        }
        let endpoint = Rc::new(self.network.register_endpoint(ip)?);
        self.peers.insert(ip, Rc::downgrade(&endpoint));
        Ok(endpoint)
    }

    /// Deliver internal ingress decisions and extract a bounded batch of native
    /// egress decisions. Source/destination tuples and payloads are untouched.
    pub fn poll(&self, budget: usize) -> Result<PacketBridgeTurn, NetworkError> {
        let mut turn = PacketBridgeTurn {
            outgoing: Vec::new(),
            closed_destinations: 0,
            budget_exhausted: false,
        };
        for _ in 0..budget {
            let Some(packet) = self.network.next_packet() else {
                return Ok(turn);
            };
            if self.peers.contains_key(&packet.destination.ip()) {
                match self.network.deliver(packet.id) {
                    Ok(()) => {}
                    Err(NetworkError::DestinationUnavailable) => turn.closed_destinations += 1,
                    Err(error) => return Err(error),
                }
            } else {
                turn.outgoing.push(ExternalPacket {
                    packet,
                    network: self.network.clone(),
                    _world: self.world.clone(),
                    decided: false,
                });
            }
        }
        turn.budget_exhausted = budget != 0;
        Ok(turn)
    }

    /// Recreate the external datagram's source tuple on a virtual adapter
    /// socket. Delivery to the real native receiver happens in the next poll.
    /// The socket bound limits are infrastructure bounds, not media profiles.
    pub fn receive_udp(
        &mut self,
        source: NetworkAddress,
        destination: NetworkAddress,
        payload: Vec<u8>,
    ) -> Result<(), PacketBridgeError> {
        if !self.peers.contains_key(&destination.ip()) {
            return Err(NetworkError::DestinationUnavailable.into());
        }
        if !self.external_udp.contains_key(&source) {
            if self.external_udp.len() >= 64 {
                return Err(PacketBridgeError::ExternalSocketLimit);
            }
            if !self.external_endpoints.contains_key(&source.ip()) {
                self.external_endpoints
                    .insert(source.ip(), self.network.register_endpoint(source.ip())?);
            }
            let socket = self.external_endpoints[&source.ip()].bind_udp(source.port())?;
            self.external_udp.insert(source, socket);
        }
        Ok(self.external_udp[&source].send_to(destination, payload)?)
    }

    pub fn receive_tcp(
        &self,
        source: NetworkAddress,
        destination: NetworkAddress,
        payload: &[u8],
    ) -> Result<(), NetworkError> {
        self.network.inject_tcp_data(source, destination, payload)
    }
}

pub fn complete_operation(
    world: &SimulationWorld,
    peer: &PeerConnection,
    operation: OperationId,
) -> Option<SessionDescription> {
    for _ in 0..2_000 {
        world.pump();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(completion) = event {
                assert_eq!(completion.operation_id, operation);
                return completion.result.expect("native signaling operation");
            }
        }
        world
            .controlled
            .advance(Duration::from_millis(1))
            .expect("signaling simulation step");
    }
    panic!("native signaling operation stalled: {operation:?}");
}
