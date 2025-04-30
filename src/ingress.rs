use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crate::{
    ice,
    message::{ActorResult, UDPPacket},
    peer::PeerHandle,
};
use bytes::Bytes;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, error::SendError},
    task::JoinHandle,
};

pub enum IngressMessage {
    AddPeer(String, PeerHandle),
    RemovePeer(String),
}

pub struct IngressActor {
    receiver: mpsc::Receiver<IngressMessage>,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    conns: HashMap<String, PeerHandle>,
    mapping: HashMap<SocketAddr, PeerHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
}

impl IngressActor {
    pub async fn run(mut self) -> ActorResult {
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];

        loop {
            tokio::select! {
                biased;

                res = self.socket.recv_from(&mut buf) => {
                    match res {
                        Ok((size, source)) => self.handle_packet(source, &buf[..size]),
                        Err(err) => {
                            tracing::error!("udp socket is failing: {err}");
                            break;
                        },
                    }
                }

                msg = self.receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_control(msg),
                        None => {
                            tracing::info!("all controllers have exited, will gracefully shutdown");
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("ingress has exited");
        Ok(())
    }

    pub fn handle_packet(&mut self, source: SocketAddr, packet: &[u8]) {
        let peer_handle = if let Some(peer_handle) = self.mapping.get(&source) {
            tracing::trace!("found connection from mapping: {source}");
            peer_handle.clone()
        } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(packet) {
            tracing::trace!("found {ufrag} in STUN packet: {:?}", self.conns);
            if let Some(peer_handle) = self.conns.get(ufrag) {
                tracing::trace!("found connection from ufrag: {ufrag} -> {source}");
                self.mapping.insert(source, peer_handle.clone());
                self.reverse
                    .entry(ufrag.to_string())
                    .or_default()
                    .push(source);
                peer_handle.clone()
            } else {
                tracing::trace!("dropped a packet from {source} due to unregistered stun binding");
                return;
            }
        } else {
            tracing::trace!(
                "dropped a packet from {source} due to unexpected message flow from an unknown source"
            );
            return;
        };

        let _ = peer_handle.forward(UDPPacket {
            raw: Bytes::copy_from_slice(packet),
            src: source,
            dst: self.local_addr,
        });
    }

    pub fn handle_control(&mut self, msg: IngressMessage) {
        match msg {
            IngressMessage::AddPeer(ufrag, peer) => {
                tracing::trace!("added {ufrag} to connection map");
                self.conns.insert(ufrag, peer);
            }
            IngressMessage::RemovePeer(ufrag) => {
                self.conns.remove(&ufrag);
                if let Some(addrs) = self.reverse.remove(&ufrag) {
                    for addr in addrs.iter() {
                        self.mapping.remove(addr);
                    }
                }
            }
        };
    }
}

#[derive(Clone, Debug)]
pub struct IngressHandle {
    sender: mpsc::Sender<IngressMessage>,
}

impl IngressHandle {
    pub fn new(local_addr: SocketAddr, socket: Arc<UdpSocket>) -> (Self, IngressActor) {
        let (sender, receiver) = mpsc::channel(1);
        let handle = Self { sender };
        let actor = IngressActor {
            receiver,
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn add_peer(
        &self,
        ufrag: String,
        peer: PeerHandle,
    ) -> Result<(), SendError<IngressMessage>> {
        self.sender.send(IngressMessage::AddPeer(ufrag, peer)).await
    }

    pub async fn remove_peer(&self, ufrag: String) -> Result<(), SendError<IngressMessage>> {
        self.sender.send(IngressMessage::RemovePeer(ufrag)).await
    }
}
