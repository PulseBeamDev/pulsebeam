use std::{net::SocketAddr, sync::Arc};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::net::UdpSocket;

use crate::{ice, message::UDPPacket, peer::PeerHandle};

#[derive(Clone)]
pub struct Ingress(Arc<IngressState>);

pub struct IngressState {
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    conns: Arc<DashMap<String, PeerHandle>>,
    mapping: DashMap<SocketAddr, PeerHandle>,
}

impl Ingress {
    pub fn new(
        local_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        conns: Arc<DashMap<String, PeerHandle>>,
    ) -> Self {
        let state = IngressState {
            local_addr,
            socket,
            conns,
            mapping: dashmap::DashMap::new(),
        };
        Self(Arc::new(state))
    }

    pub async fn run(self) {
        let state = self.0;
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];

        while let Ok((size, source)) = state.socket.recv_from(&mut buf).await {
            let packet = &buf[..size];
            // let packet = buf.split_to(size).freeze();

            let peer_handle = if let Some(peer_handle) = state.mapping.get(&source) {
                tracing::trace!("found connection from mapping: {source}");
                peer_handle.clone()
            } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(packet) {
                tracing::trace!("found {ufrag} in STUN packet: {:?}", state.conns);
                if let Some(peer_handle) = state.conns.get(ufrag) {
                    tracing::trace!("found connection from ufrag: {ufrag} -> {source}");
                    state.mapping.insert(source, peer_handle.clone());
                    peer_handle.clone()
                } else {
                    tracing::trace!(
                        "dropped a packet from {source} due to unregistered stun binding"
                    );
                    continue;
                }
            } else {
                tracing::trace!(
                    "dropped a packet from {source} due to unexpected message flow from an unknown source"
                );
                continue;
            };

            let _ = peer_handle.forward(UDPPacket {
                raw: Bytes::copy_from_slice(packet),
                src: source,
                dst: state.local_addr,
            });
        }

        tracing::info!("ingress has exited");
    }
}
