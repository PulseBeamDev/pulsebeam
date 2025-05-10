use std::{net::SocketAddr, sync::Arc};

use pulsebeam::{
    controller::{ControllerActor, ControllerHandle},
    net::UdpSocket,
    rng::Rng,
    sink::{UdpSinkActor, UdpSinkHandle},
    source::{UdpSourceActor, UdpSourceHandle},
};
use rand::SeedableRng;

pub struct SimulatedNetwork {
    source: UdpSourceActor,
    sink: UdpSinkActor,
    controller: ControllerActor,
}

impl SimulatedNetwork {
    pub async fn new() -> (Self, ControllerHandle) {
        // TODO: turmoil doesn't support other addresses other than localhost
        let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
        let socket = UdpSocket::bind(local_addr).await.unwrap();
        let local_addr: SocketAddr = "1.2.3.4:3478".parse().unwrap();
        let socket = Arc::new(socket);

        let rng = Rng::seed_from_u64(1);
        let (source_handle, source_actor) = UdpSourceHandle::new(local_addr, socket.clone());
        let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
        let (controller_handle, controller_actor) =
            ControllerHandle::new(rng, source_handle, sink_handle, vec![local_addr]);

        let network = Self {
            source: source_actor,
            sink: sink_actor,
            controller: controller_actor,
        };

        (network, controller_handle)
    }

    pub async fn run(self) {
        let res = tokio::try_join!(self.source.run(), self.sink.run(), self.controller.run());
        if let Err(err) = res {
            tracing::error!("pipeline ended with an error: {err}")
        }
    }
}
