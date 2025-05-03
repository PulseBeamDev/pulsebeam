use std::sync::Arc;

use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, error::TrySendError},
};

use crate::message::{self, ActorResult};

#[derive(Debug)]
pub enum UdpSinkMessage {
    UdpPacket(message::EgressUDPPacket),
}

pub struct UdpSinkActor {
    socket: Arc<UdpSocket>,
    receiver: mpsc::Receiver<UdpSinkMessage>,
}

impl UdpSinkActor {
    pub async fn run(mut self) -> ActorResult {
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                UdpSinkMessage::UdpPacket(packet) => {
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
pub struct UdpSinkHandle {
    sender: mpsc::Sender<UdpSinkMessage>,
}

impl UdpSinkHandle {
    pub fn new(socket: Arc<UdpSocket>) -> (Self, UdpSinkActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self { sender };
        let actor = UdpSinkActor { socket, receiver };
        (handle, actor)
    }

    pub fn send(&self, msg: message::EgressUDPPacket) -> Result<(), TrySendError<UdpSinkMessage>> {
        // TODO: implement double buffering, https://blog.digital-horror.com/blog/how-to-avoid-over-reliance-on-mpsc/
        // TODO: monitor backpressure and packet dropping
        self.sender.try_send(UdpSinkMessage::UdpPacket(msg))
    }
}
