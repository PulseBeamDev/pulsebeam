use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    message::{self, ActorResult},
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

impl<S: PacketSocket> UdpSinkActor<S> {
    pub async fn run(mut self) -> ActorResult {
        let mut buf = Vec::with_capacity(256);
        loop {
            let size = self.receiver.recv_many(&mut buf, 256).await;
            if size == 0 {
                break;
            }

            // TODO: this is far from ideal. sendmmsg can be used to reduce the syscalls.
            // In the future, we'll rewrite the source and sink with a dedicated thread of io-uring.
            //
            // tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
            for msg in buf.iter() {
                match msg {
                    UdpSinkMessage::UdpPacket(packet) => {
                        let res = self.socket.send_to(&packet.raw, packet.dst).await;
                        if let Err(err) = res {
                            tracing::warn!(
                                "failed to send udp packet to {:?}: {:?}",
                                packet.dst,
                                err
                            );
                        }
                    }
                }
            }
            buf.clear();
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
        // TODO: implement double buffering, https://blog.digital-horror.com/blog/how-to-avoid-over-reliance-on-mpsc/
        // TODO: monitor backpressure and packet dropping
        let res = self.sender.try_send(UdpSinkMessage::UdpPacket(msg));

        if let Err(err) = &res {
            tracing::warn!("sink raw packet is dropped: {err}");
        }

        res
    }
}
