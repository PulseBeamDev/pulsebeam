use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use crate::{
    egress::EgressHandle,
    ingress::Ingress,
    message::{GroupId, PeerId},
    peer::PeerHandle,
};
use dashmap::DashMap;
use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::SdpError};
use tokio::{
    net::UdpSocket,
    sync::{Mutex, mpsc},
};

use systemstat::{Platform, System};

pub fn select_host_address() -> IpAddr {
    let system = System::new();
    let networks = system.networks().unwrap();

    for net in networks.values() {
        for n in &net.addrs {
            if let systemstat::IpAddr::V4(v) = n.addr {
                if !v.is_loopback() && !v.is_link_local() && !v.is_broadcast() {
                    return IpAddr::V4(v);
                }
            }
        }
    }

    panic!("Found no usable network interface");
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),

    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] RtcError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

#[derive(Clone)]
pub struct Controller(Arc<ControllerState>);

pub struct ControllerState {
    ingress: Ingress,
    egress: EgressHandle,
    conns: Arc<DashMap<String, PeerHandle>>,
    groups: Arc<DashMap<Arc<GroupId>, Group>>,

    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
}

impl Controller {
    pub async fn spawn() -> Result<Self, ControllerError> {
        // TODO: replace this with a config
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        // tokio requires this
        socket.set_nonblocking(true)?;
        socket.set_reuse_address(true)?;
        socket.set_send_buffer_size(4 * 1024 * 1024)?;
        socket.set_recv_buffer_size(4 * 1024 * 1024)?;

        let ip = select_host_address();
        let local_addr: SocketAddr = format!("{ip}:3478").parse().expect("valid bind addr");
        socket.bind(&local_addr.into())?;

        let socket = tokio::net::UdpSocket::from_std(socket.into())?;
        let socket = Arc::new(socket);

        let conns = Arc::new(DashMap::new());
        let ingress = Ingress::new(local_addr, socket.clone(), Arc::clone(&conns));
        let egress = EgressHandle::spawn(socket.clone());

        tokio::spawn(ingress.clone().run());

        let controller_state = ControllerState {
            ingress,
            egress,
            conns,
            groups: Arc::new(DashMap::new()),
            socket,
            local_addr,
        };
        let controller = Self(Arc::new(controller_state));
        Ok(controller)
    }

    pub async fn allocate(
        &self,
        group_id: GroupId,
        peer_id: PeerId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let offer = SdpOffer::from_sdp_string(&offer)?;
        let mut rtc = Rtc::builder()
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            // .set_ice_lite(true)
            .build();

        // Add the shared UDP socket as a host candidate
        let candidate = Candidate::host(self.0.local_addr, "udp").expect("a host candidate");
        rtc.add_local_candidate(candidate);

        // Create an SDP Answer.
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ControllerError::OfferRejected)?;

        let group_id = Arc::new(group_id);
        let entry = self
            .0
            .groups
            .entry(group_id.clone())
            .or_insert_with(|| Group::new(self.clone(), group_id));

        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let peer_handle = entry.spawn(peer_id, rtc).await;
        tracing::trace!("added {ufrag} to connection map");
        self.0.conns.insert(ufrag, peer_handle);

        Ok(answer.to_sdp_string())
    }

    pub fn egress(&self) -> EgressHandle {
        self.0.egress.clone()
    }
}

#[derive(Clone)]
pub struct Group(Arc<GroupState>);

pub struct GroupState {
    controller: Controller,
    group_id: Arc<GroupId>,
    peers: Arc<DashMap<Arc<PeerId>, PeerHandle>>,
    routers: Arc<DashMap<usize, RouterHandle>>,
    spawn_lock: Arc<Mutex<usize>>,
}

impl Group {
    fn new(controller: Controller, group_id: Arc<GroupId>) -> Self {
        let state = GroupState {
            controller,
            group_id,
            peers: Arc::new(DashMap::new()),
            routers: Arc::new(DashMap::new()),
            spawn_lock: Arc::new(Mutex::new(0)),
        };
        Self(Arc::new(state))
    }

    pub fn size(&self) -> usize {
        self.0.peers.len()
    }

    pub async fn spawn(&self, peer_id: PeerId, rtc: Rtc) -> PeerHandle {
        {
            let mut n = self.0.spawn_lock.lock().await;

            if self.0.routers.is_empty() {
                let router = RouterHandle::spawn(self.clone());
                self.0.routers.insert(*n, router);
                *n += 1;
            }

            // TODO: handle scaling for big groups
        }

        let peer_id = Arc::new(peer_id);
        PeerHandle::spawn(self.0.controller.egress(), self.clone(), peer_id, rtc)
    }

    pub fn propagate(&self, mut msg: RouterMessage) {
        for e in self.0.routers.iter() {
            match e.sender.try_send(msg) {
                Ok(_) => {
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(bounced)) => {
                    msg = bounced;
                    // TODO: handle closed channel
                }
                Err(mpsc::error::TrySendError::Full(bounced)) => {
                    // TODO: handle scaling for big groups
                    msg = bounced;
                }
            }
        }
    }
}

pub enum RouterMessage {}

pub struct RouterActor {
    group: Group,
    receiver: mpsc::Receiver<RouterMessage>,
}

impl RouterActor {
    async fn run(self) {}
}

#[derive(Clone)]
pub struct RouterHandle {
    pub sender: mpsc::Sender<RouterMessage>,
}

impl RouterHandle {
    fn spawn(group: Group) -> Self {
        // TODO: channel size
        let (tx, rx) = mpsc::channel(8);
        let actor = RouterActor {
            group,
            receiver: rx,
        };
        tokio::spawn(actor.run());
        Self { sender: tx }
    }
}
