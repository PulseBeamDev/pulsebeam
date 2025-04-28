use std::sync::Arc;

use bytes::Bytes;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, error::TrySendError},
};

use crate::peer::PeerHandle;

#[derive(Debug)]
pub enum IngressMessage {
    Ping,
}

pub struct IngressActor {
    socket: Arc<UdpSocket>,
    conns: dashmap::ReadOnlyView<String, PeerHandle>,
}

impl IngressActor {
    fn handle_message(&mut self, msg: IngressMessage) {
        tracing::info!("received: {:?}", msg);
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<IngressMessage>) {
        let mut buf = vec![0; 2000];

        loop {
            // bias toward internal loop
            tokio::select! {
                msg = receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_message(msg),
                        None => break,
                    }
                }
                res = self.socket.recv_from(&mut buf) => {
                    match res {
                        Ok((size, source)) => {
                            let bytes = Bytes::copy_from_slice(&buf[..size]);
                        },
                        Err(err) => {
                            tracing::warn!("udp error in receiving: {:?}", err);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct IngressHandle {
    sender: mpsc::Sender<IngressMessage>,
}

impl IngressHandle {
    pub fn spawn(socket: Arc<UdpSocket>, conns: dashmap::ReadOnlyView<String, PeerHandle>) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let actor = IngressActor { socket, conns };
        tokio::spawn(actor.run(receiver));
        Self { sender }
    }

    pub fn ping(&self) -> Result<(), TrySendError<IngressMessage>> {
        self.sender.try_send(IngressMessage::Ping)
    }
}
