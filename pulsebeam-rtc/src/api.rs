use std::{
    cell::Cell,
    collections::VecDeque,
    fmt,
    marker::PhantomData,
    net::SocketAddr,
    time::{Instant, SystemTime},
};

use crate::{DepartureReceipt, IngressStream, RtcConfiguration, TransmissionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramProtocol {
    Udp,
}

#[derive(Debug, PartialEq, Eq)]
pub struct IngressDatagram {
    protocol: DatagramProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
}

impl IngressDatagram {
    pub fn new(
        protocol: DatagramProtocol,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: Vec<u8>,
    ) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }

        debug_assert!(!bytes.is_empty(), "an ingress datagram contains bytes");
        Some(Self {
            protocol,
            source,
            destination,
            bytes,
        })
    }

    pub const fn protocol(&self) -> DatagramProtocol {
        self.protocol
    }
    pub const fn source(&self) -> SocketAddr {
        self.source
    }
    pub const fn destination(&self) -> SocketAddr {
        self.destination
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug)]
pub struct MediaPacket {
    bytes: Vec<u8>,
    stream: IngressStream,
    sequence: u64,
    timestamp: u64,
    marker: bool,
    packet_id: u64,
    playout_time: SystemTime,
    _owner_local: PhantomData<Cell<()>>,
}

impl MediaPacket {
    pub const fn stream(&self) -> IngressStream {
        self.stream
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn timestamp(&self) -> u64 {
        self.timestamp
    }
    pub const fn marker(&self) -> bool {
        self.marker
    }
    pub fn payload(&self) -> &[u8] {
        &self.bytes
    }
    pub fn bytes(&self) -> &[u8] {
        self.payload()
    }
    pub const fn packet_id(&self) -> u64 {
        self.packet_id
    }
    pub const fn playout_time(&self) -> SystemTime {
        self.playout_time
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaDirection {
    SendOnly,
    ReceiveOnly,
    Inactive,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataChannelMode {
    ReliableOrdered,
    UnreliableUnordered,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DataPayload {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcConnectionState {
    Configured,
    Negotiated,
    Connecting,
    Connected,
    Draining,
    Closed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Application,
    Transport,
    Protocol,
    Timeout,
    Failed,
}

#[derive(Debug)]
pub enum RtcEvent {
    ConnectionStateChanged(RtcConnectionState),
    Closed(CloseReason),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Transmit {
    protocol: DatagramProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    transmission_id: TransmissionId,
    receipt: DepartureReceipt,
}

impl Transmit {
    pub const fn protocol(&self) -> DatagramProtocol {
        self.protocol
    }
    pub const fn source(&self) -> SocketAddr {
        self.source
    }
    pub const fn destination(&self) -> SocketAddr {
        self.destination
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub const fn receipt(&self) -> DepartureReceipt {
        self.receipt
    }
    pub const fn transmission_id(&self) -> TransmissionId {
        self.transmission_id
    }
    pub fn into_parts(
        self,
    ) -> (
        DatagramProtocol,
        SocketAddr,
        SocketAddr,
        Vec<u8>,
        TransmissionId,
        DepartureReceipt,
    ) {
        (
            self.protocol,
            self.source,
            self.destination,
            self.bytes,
            self.transmission_id,
            self.receipt,
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RtcPeerError {
    Closed,
    InvalidInput,
    NotNegotiated,
    QueueFull,
}

impl fmt::Display for RtcPeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "RTC peer is closed",
            Self::InvalidInput => "invalid RTC input",
            Self::NotNegotiated => "RTC peer has not negotiated a session",
            Self::QueueFull => "RTC event queue is full",
        })
    }
}

impl std::error::Error for RtcPeerError {}

#[derive(Debug)]
pub enum ApplicationCommand {
    Datagram {
        at: Instant,
        datagram: IngressDatagram,
    },
    Timeout {
        at: Instant,
    },
    Close {
        at: Instant,
        reason: CloseReason,
    },
}

pub struct RtcPeer {
    configuration: RtcConfiguration,
    state: RtcConnectionState,
    close_reason: Option<CloseReason>,
    last_now: Option<Instant>,
    events: VecDeque<RtcEvent>,
    transmissions: VecDeque<Transmit>,
    next_deadline: Option<Instant>,
}

impl RtcPeer {
    pub fn new(configuration: RtcConfiguration) -> Self {
        Self {
            configuration,
            state: RtcConnectionState::Configured,
            close_reason: None,
            last_now: None,
            events: VecDeque::from([RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Configured,
            )]),
            transmissions: VecDeque::new(),
            next_deadline: None,
        }
    }

    pub const fn state(&self) -> RtcConnectionState {
        self.state
    }
    pub fn configuration(&self) -> &RtcConfiguration {
        &self.configuration
    }
    pub const fn close_reason(&self) -> Option<CloseReason> {
        self.close_reason
    }

    pub fn apply(&mut self, command: ApplicationCommand) -> Result<(), RtcPeerError> {
        match command {
            ApplicationCommand::Datagram { at, datagram } => self.handle_datagram(at, datagram),
            ApplicationCommand::Timeout { at } => self.handle_timeout(at),
            ApplicationCommand::Close { at, reason } => self.close(at, reason),
        }
    }

    pub fn handle_datagram(
        &mut self,
        now: Instant,
        datagram: IngressDatagram,
    ) -> Result<(), RtcPeerError> {
        self.observe(now)?;
        if datagram.bytes.is_empty() {
            return Err(RtcPeerError::InvalidInput);
        }
        Err(RtcPeerError::NotNegotiated)
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        self.observe(now)?;
        Err(RtcPeerError::NotNegotiated)
    }

    pub fn poll_event(&mut self) -> Option<RtcEvent> {
        self.events.pop_front()
    }

    pub const fn next_deadline(&self) -> Option<Instant> {
        self.next_deadline
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        let transmission = self.transmissions.pop_front();
        debug_assert!(self.transmissions.len() <= self.configuration.max_transmissions() as usize);
        transmission
    }

    pub fn close(&mut self, now: Instant, reason: CloseReason) -> Result<(), RtcPeerError> {
        self.observe(now)?;
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            return Err(RtcPeerError::Closed);
        }
        let max_events = self.configuration.max_events() as usize;
        if self.events.len() >= max_events {
            return Err(RtcPeerError::QueueFull);
        }

        self.state = if reason == CloseReason::Failed {
            RtcConnectionState::Failed
        } else {
            RtcConnectionState::Closed
        };
        self.close_reason = Some(reason);
        self.events.push_back(RtcEvent::Closed(reason));
        debug_assert!(self.events.len() <= self.configuration.max_events() as usize);
        Ok(())
    }

    fn observe(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            return Err(RtcPeerError::Closed);
        }
        if let Some(previous) = self.last_now {
            if now < previous {
                return Err(RtcPeerError::InvalidInput);
            }
            debug_assert!(now >= previous, "caller time must be monotonic");
        }
        self.last_now = Some(now);
        Ok(())
    }
}
