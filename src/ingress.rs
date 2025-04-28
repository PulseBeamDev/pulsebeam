use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use tokio::net::UdpSocket;

use crate::peer::PeerHandle;

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
        let mut buf = vec![0; 2000];

        while let Ok((size, source)) = state.socket.recv_from(&mut buf).await {
            let bytes = Bytes::copy_from_slice(&buf[..size]);
            // TODO: demux and forward
        }

        tracing::info!("ingress has exited");
    }
}
