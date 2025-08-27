use pulsebeam_runtime::actor;
use pulsebeam_runtime::mailbox;
use pulsebeam_runtime::net;

use crate::message;

#[derive(Debug)]
pub enum SinkMessage {
    Packet(message::EgressUDPPacket),
}

pub struct SinkActor {
    socket: net::UnifiedSocket,
}

impl actor::Actor for SinkActor {
    type LowPriorityMessage = SinkMessage;
    type HighPriorityMessage = ();
    type ID = usize;

    fn kind(&self) -> &'static str {
        "sink"
    }

    fn id(&self) -> Self::ID {
        0
    }

    async fn run(
        &mut self,
        hi_rx: mailbox::Receiver<Self::HighPriorityMessage>,
        mut lo_rx: mailbox::Receiver<Self::LowPriorityMessage>,
    ) -> Result<(), actor::ActorError> {
        drop(hi_rx); // unused for now
        // TODO: this is far from ideal. sendmmsg can be used to reduce the syscalls.
        // In the future, we'll rewrite the source and sink with a dedicated thread of io-uring.
        //
        // tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
        // TODO: use quinn-udp optimizations, https://github.com/quinn-rs/quinn/blob/4f8a0f13cf7931ef9be573af5089c7a4a49387ae//quinn/src/runtime/tokio.rs#L1-L102
        while let Some(msg) = lo_rx.recv().await {
            match msg {
                SinkMessage::Packet(packet) => {
                    let res = self.socket.send_to(&packet.raw, packet.dst).await;
                    if let Err(err) = res {
                        tracing::warn!("failed to send packet to {:?}: {:?}", packet.dst, err);
                    }
                }
            }
        }
        Ok(())
    }
}

impl SinkActor {
    pub fn new(socket: net::UnifiedSocket) -> Self {
        Self { socket }
    }
}

pub type SinkHandle = actor::LocalActorHandle<SinkActor>;
