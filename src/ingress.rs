use std::{net::SocketAddr, sync::Arc};

use bytes::BytesMut;
use tokio::net::UdpSocket;

use crate::{ice, message::UDPPacket, peer::PeerHandle};

#[derive(Clone)]
pub struct Ingress(Arc<IngressState>);

pub struct IngressState {
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    conns: dashmap::ReadOnlyView<String, PeerHandle>,
    mapping: dashmap::DashMap<SocketAddr, PeerHandle>,
}

impl Ingress {
    pub fn new(
        local_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        conns: dashmap::ReadOnlyView<String, PeerHandle>,
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
        let mut buf = BytesMut::with_capacity(128 * 1024);

        while let Ok((size, source)) = state.socket.recv_from(&mut buf).await {
            let packet = buf.split_to(size).freeze();

            let peer_handle = if let Some(peer_handle) = state.mapping.get(&source) {
                peer_handle.clone()
            } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(&packet) {
                if let Some(peer_handle) = state.conns.get(ufrag) {
                    state.mapping.insert(source, peer_handle.clone());
                    peer_handle.clone()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let _ = peer_handle.forward(UDPPacket {
                raw: packet,
                src: source,
                dst: state.local_addr,
            });
        }

        tracing::info!("ingress has exited");
    }
}
