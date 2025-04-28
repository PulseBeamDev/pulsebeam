use std::sync::Arc;

use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, error::TrySendError},
};

use crate::message;

#[derive(Debug)]
pub enum EgressMessage {
    UdpPacket(message::EgressUDPPacket),
}

pub struct EgressActor {
    socket: Arc<UdpSocket>,
}

impl EgressActor {
    async fn run(self, mut receiver: mpsc::Receiver<EgressMessage>) {
        while let Some(msg) = receiver.recv().await {
            match msg {
                EgressMessage::UdpPacket(packet) => {
                    let res = self.socket.send_to(&packet.raw, &packet.dst).await;
                    if let Err(err) = res {
                        tracing::warn!("failed to send udp packet to {:?}: {:?}", packet.dst, err);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct EgressHandle {
    sender: mpsc::Sender<EgressMessage>,
}

impl EgressHandle {
    pub fn spawn(socket: Arc<UdpSocket>) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let actor = EgressActor { socket };
        tokio::spawn(actor.run(receiver));
        Self { sender }
    }

    pub async fn send(
        &self,
        msg: message::EgressUDPPacket,
    ) -> Result<(), TrySendError<EgressMessage>> {
        self.sender.try_send(EgressMessage::UdpPacket(msg))
    }
}
