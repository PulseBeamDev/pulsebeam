use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use noq_proto::{
    Connection, DatagramEvent, EcnCodepoint, Endpoint, FourTuple, ServerConfig, Transmit,
};

pub use noq_proto::ConnectionHandle;
use pulsebeam_core::fabric::Profile;

use crate::data_bindings::FabricOffer;

use crate::frame::{Kind, Phase};
use crate::records::Admit;

use super::control::{ControlStream, Read};
use super::{
    PacketScheduler, Readiness, UDP_PAYLOAD_BYTES, close, data_readiness, endpoint,
    forward_endpoint_event, service_timeout,
};

const MAX_PRE_ADMISSION: usize = 64;
const MAX_CONNECTIONS: usize = 256;
const POLL_BUDGET: usize = 32;
const ENDPOINT_EVENT_BUDGET: usize = 16;
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Listener {
    endpoint: Endpoint,
    peers: HashMap<ConnectionHandle, Association>,
    ready: VecDeque<ConnectionHandle>,
    pre_admission: usize,
    remaining: usize,
}

pub struct Association {
    pub connection: Connection,
    pub control: Option<ControlStream>,
    pub scheduler: PacketScheduler,
    pub control_runnable: bool,
    admission_deadline: Option<Instant>,
    pre_admission: bool,
    redeem_started: bool,
    peer_fabric: Option<FabricOffer>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    Profile(Readiness),
    Stream(super::control::Error),
    Missing,
    Malformed,
}

impl Association {
    /// Mark the response fully admitted to the transport stream.
    pub fn finish_admission(&mut self) -> bool {
        if !self.pre_admission || !self.redeem_started || self.connection.is_closed() {
            return false;
        }
        self.admission_deadline = None;
        self.pre_admission = false;
        true
    }

    pub const fn peer_fabric(&self) -> Option<FabricOffer> {
        self.peer_fabric
    }

    /// Consume the exact attempt body once, after TLS, ALPN and path budget
    /// checks. The controller authenticates these bytes and owns the outcome.
    /// Never log the returned credential-bearing body.
    pub fn take_admit(&mut self, now: Instant) -> Result<Option<Vec<u8>>, AdmitError> {
        if !self.pre_admission || self.redeem_started {
            return Ok(None);
        }
        match data_readiness(&mut self.connection) {
            Readiness::Handshaking => return Ok(None),
            Readiness::Ready => {}
            state => return Err(AdmitError::Profile(state)),
        }
        if self.control.is_none() {
            self.control =
                ControlStream::accept(&mut self.connection).map_err(AdmitError::Stream)?;
        }
        let Some(control) = &mut self.control else {
            return Ok(None);
        };
        match control
            .read(&mut self.connection, Phase::Bootstrap, now)
            .map_err(AdmitError::Stream)?
        {
            Read::Pending => Ok(None),
            Read::Finished => Err(AdmitError::Missing),
            Read::Frame(frame) if frame.kind == Kind::Admit => {
                let admit = Admit::decode(&frame.body).map_err(|_| AdmitError::Malformed)?;
                FabricOffer::CURRENT
                    .bind(admit.fabric, Profile::Data, 0)
                    .map_err(|_| AdmitError::Malformed)?;
                self.peer_fabric = Some(admit.fabric);
                self.redeem_started = true;
                Ok(Some(frame.body))
            }
            Read::Frame(_) => Err(AdmitError::Malformed),
        }
    }
}

impl Listener {
    pub fn new(config: ServerConfig) -> Result<Self, noq_proto::ConfigError> {
        Ok(Self::with_endpoint(endpoint(Some(config))?))
    }

    pub fn on_shard(config: ServerConfig, shard: u16) -> Result<Self, noq_proto::ConfigError> {
        Ok(Self::with_endpoint(super::endpoint_on_shard(
            config, shard,
        )?))
    }

    fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            peers: HashMap::new(),
            ready: VecDeque::new(),
            pre_admission: 0,
            remaining: 0,
        }
    }

    pub fn association_count(&self) -> usize {
        self.ready.len()
    }

    pub fn begin_turn(&mut self) {
        self.remaining = self.ready.len().min(POLL_BUDGET);
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.peers
            .values()
            .flat_map(|peer| {
                let open = !peer.connection.is_closed();
                [
                    peer.connection.poll_timeout(),
                    peer.admission_deadline.filter(|_| open),
                    peer.control
                        .as_ref()
                        .filter(|_| open)
                        .and_then(ControlStream::deadline),
                ]
                .into_iter()
                .flatten()
            })
            .min()
    }

    pub fn get(&mut self, handle: ConnectionHandle) -> Option<&mut Association> {
        self.peers.get_mut(&handle)
    }

    /// Call only after successful controller commit and admission-result write.
    pub fn admitted(&mut self, handle: ConnectionHandle) -> bool {
        let Some(peer) = self.peers.get_mut(&handle) else {
            return false;
        };
        if !peer.finish_admission() {
            return false;
        }
        self.pre_admission = self.pre_admission.saturating_sub(1);
        true
    }

    /// Process one UDP packet. If Noq returns an immediate transmit, its
    /// bytes are in `out`; use the same bounded egress slot as normal output.
    pub fn receive(
        &mut self,
        now: Instant,
        path: FourTuple,
        ecn: Option<EcnCodepoint>,
        packet: &[u8],
        out: &mut Vec<u8>,
    ) -> Option<Transmit> {
        self.receive_then(now, path, ecn, packet, out, |_, _| {})
    }

    #[allow(
        clippy::disallowed_types,
        reason = "Noq owns packet bytes after the UDP receive buffer advances"
    )]
    pub(super) fn receive_then(
        &mut self,
        now: Instant,
        path: FourTuple,
        ecn: Option<EcnCodepoint>,
        packet: &[u8],
        out: &mut Vec<u8>,
        service: impl FnOnce(ConnectionHandle, &mut Association),
    ) -> Option<Transmit> {
        out.clear();
        if packet.is_empty() || packet.len() > usize::from(UDP_PAYLOAD_BYTES) {
            return None;
        }
        match self
            .endpoint
            .handle(now, path, ecn, bytes::BytesMut::from(packet), out)?
        {
            DatagramEvent::ConnectionEvent(handle, event) => {
                if let Some(peer) = self.peers.get_mut(&handle) {
                    peer.connection.handle_event(event);
                    if !peer.pre_admission && !peer.connection.is_closed() {
                        service(handle, peer);
                    }
                }
                None
            }
            DatagramEvent::Response(transmit) => Some(transmit),
            DatagramEvent::NewConnection(incoming) => {
                let deadline = now.checked_add(ADMISSION_TIMEOUT);
                if self.pre_admission >= MAX_PRE_ADMISSION
                    || self.peers.len() >= MAX_CONNECTIONS
                    || deadline.is_none()
                {
                    return Some(self.endpoint.refuse(incoming, out));
                }
                match self.endpoint.accept(incoming, now, out, None) {
                    Ok((handle, connection)) => {
                        self.ready.push_back(handle);
                        self.peers.insert(
                            handle,
                            Association {
                                connection,
                                control: None,
                                scheduler: PacketScheduler::default(),
                                control_runnable: false,
                                admission_deadline: deadline,
                                pre_admission: true,
                                redeem_started: false,
                                peer_fabric: None,
                            },
                        );
                        self.pre_admission = self.pre_admission.saturating_add(1);
                        None
                    }
                    Err(error) => error.response,
                }
            }
        }
    }

    /// Visit at most one association per call. The callback handles physical
    /// admission/control and sees transport closure before removal. The caller
    /// may pause the turn while a committed socket write is blocked.
    pub fn poll_one(
        &mut self,
        now: Instant,
        out: &mut Vec<u8>,
        mut service: impl FnMut(ConnectionHandle, &mut Association),
    ) -> Option<(ConnectionHandle, Option<Transmit>)> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        let handle = self.ready.pop_front()?;
        let peer = self.peers.get_mut(&handle)?;
        service_timeout(&mut peer.connection, now);
        if peer
            .admission_deadline
            .is_some_and(|deadline| deadline <= now)
            || peer
                .control
                .as_ref()
                .and_then(ControlStream::deadline)
                .is_some_and(|deadline| deadline <= now)
        {
            close(&mut peer.connection, now);
        }
        for _ in 0..ENDPOINT_EVENT_BUDGET {
            if !forward_endpoint_event(&mut self.endpoint, handle, &mut peer.connection) {
                break;
            }
        }
        let was_pre_admission = peer.pre_admission;
        service(handle, peer);
        if was_pre_admission && !peer.pre_admission {
            self.pre_admission = self.pre_admission.saturating_sub(1);
        }
        if peer.connection.is_drained() {
            if peer.pre_admission {
                self.pre_admission = self.pre_admission.saturating_sub(1);
            }
            self.peers.remove(&handle);
            return Some((handle, None));
        }
        self.ready.push_back(handle);
        Some((
            handle,
            peer.scheduler
                .poll(&mut peer.connection, now, out, peer.control_runnable),
        ))
    }
}
