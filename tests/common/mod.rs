use std::net::SocketAddr;

use pulsebeam::{
    controller::{ControllerActor, ControllerHandle},
    entity::{ParticipantId, RoomId},
    net::SimulatedSocket,
    sink::{UdpSinkActor, UdpSinkHandle},
    source::{UdpSourceActor, UdpSourceHandle},
};
use str0m::Candidate;

pub struct SimulatedNetwork {
    source: UdpSourceActor<SimulatedSocket>,
    sink: UdpSinkActor<SimulatedSocket>,
    controller: ControllerActor,
}

impl SimulatedNetwork {
    pub fn new() -> (Self, ControllerHandle) {
        let local_addr: SocketAddr = "1.2.3.4:3478".parse().unwrap();
        let socket = SimulatedSocket::new(8192);

        let (source_handle, source_actor) = UdpSourceHandle::new(local_addr, socket.clone());
        let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
        let (controller_handle, controller_actor) =
            ControllerHandle::new(source_handle, sink_handle, vec![local_addr]);

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
