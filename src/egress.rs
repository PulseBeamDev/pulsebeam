use std::sync::Arc;

use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, error::TrySendError},
};

use crate::message::{self, ActorResult};

#[derive(Debug)]
pub enum EgressMessage {
    UdpPacket(message::EgressUDPPacket),
}

pub struct EgressActor {
    socket: Arc<UdpSocket>,
    receiver: mpsc::Receiver<EgressMessage>,
}

impl EgressActor {
    pub async fn run(mut self) -> ActorResult {
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                EgressMessage::UdpPacket(packet) => {
                    let res = self.socket.send_to(&packet.raw, &packet.dst).await;
                    if let Err(err) = res {
                        tracing::warn!("failed to send udp packet to {:?}: {:?}", packet.dst, err);
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct EgressHandle {
    sender: mpsc::Sender<EgressMessage>,
}

impl EgressHandle {
    pub fn new(socket: Arc<UdpSocket>) -> (Self, EgressActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self { sender };
        let actor = EgressActor { socket, receiver };
        (handle, actor)
    }

    pub fn send(&self, msg: message::EgressUDPPacket) -> Result<(), TrySendError<EgressMessage>> {
        // TODO: monitor backpressure and packet dropping
        self.sender.try_send(EgressMessage::UdpPacket(msg))
    }
}
