use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    actor::{Actor, ActorError},
    message,
    net::PacketSocket,
};

#[derive(Debug)]
pub enum UdpSinkMessage {
    UdpPacket(message::EgressUDPPacket),
}

pub struct UdpSinkActor<S> {
    socket: S,
    receiver: mpsc::Receiver<UdpSinkMessage>,
}

impl<S: PacketSocket> Actor for UdpSinkActor<S> {
    type ID = usize;

    fn kind(&self) -> &'static str {
        "udp_source"
    }

    fn id(&self) -> Self::ID {
        0
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        // TODO: this is far from ideal. sendmmsg can be used to reduce the syscalls.
        // In the future, we'll rewrite the source and sink with a dedicated thread of io-uring.
        //
        // tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
        // TODO: use quinn-udp optimizations, https://github.com/quinn-rs/quinn/blob/4f8a0f13cf7931ef9be573af5089c7a4a49387ae//quinn/src/runtime/tokio.rs#L1-L102
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                UdpSinkMessage::UdpPacket(packet) => {
                    let res = self.socket.send_to(&packet.raw, packet.dst).await;
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
    pub fn new<S: PacketSocket>(socket: S) -> (Self, UdpSinkActor<S>) {
        let (sender, receiver) = mpsc::channel(2048);
        let handle = Self { sender };
        let actor = UdpSinkActor { socket, receiver };
        (handle, actor)
    }

    pub fn send(&self, msg: message::EgressUDPPacket) -> Result<(), TrySendError<UdpSinkMessage>> {
        // TODO: monitor backpressure and packet dropping
        let res = self.sender.try_send(UdpSinkMessage::UdpPacket(msg));

        if let Err(err) = &res {
            tracing::warn!("sink raw packet is dropped: {err}");
        }

        res
    }
}
