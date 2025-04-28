use std::{net::SocketAddr, sync::Arc};

use bytes::BytesMut;
use tokio::net::UdpSocket;

use crate::{ice, message::IngressUDPPacket, peer::PeerHandle};

pub struct Ingress(Arc<IngressState>);

pub struct IngressState {
    socket: Arc<UdpSocket>,
    conns: dashmap::ReadOnlyView<String, PeerHandle>,
    mapping: dashmap::DashMap<SocketAddr, PeerHandle>,
}

impl Ingress {
    pub fn new(socket: Arc<UdpSocket>, conns: dashmap::ReadOnlyView<String, PeerHandle>) -> Self {
        let state = IngressState {
            socket,
            conns,
            mapping: dashmap::DashMap::new(),
        };
        Self(Arc::new(state))
    }

    async fn run(self) {
        let state = self.0;
        let mut buf = BytesMut::with_capacity(128 * 1024);

        while let Ok((size, source)) = state.socket.recv_from(&mut buf).await {
            let packet = buf.split_to(size).freeze();

            let peer_handle = if let Some(peer_handle) = state.mapping.get(&source) {
                peer_handle.clone()
            } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(&packet) {
                if let Some(peer_handle) = state.conns.get(ufrag) {
                    peer_handle.clone()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            peer_handle.forward(IngressUDPPacket {
                raw: packet,
                src: source,
            });
        }

        tracing::info!("ingress has exited");
    }
}
