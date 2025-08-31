use pulsebeam_runtime::actor;
use pulsebeam_runtime::net;

use crate::message;

#[derive(Debug)]
pub enum SinkMessage {
    Packet(message::EgressUDPPacket),
}

pub struct SinkActor {
    socket: net::UnifiedSocket,
}

// TODO: this is far from ideal. sendmmsg can be used to reduce the syscalls.
// In the future, we'll rewrite the source and sink with a dedicated thread of io-uring.
//
// tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
// TODO: use quinn-udp optimizations, https://github.com/quinn-rs/quinn/blob/4f8a0f13cf7931ef9be573af5089c7a4a49387ae//quinn/src/runtime/tokio.rs#L1-L102
impl actor::Actor for SinkActor {
    type LowPriorityMsg = SinkMessage;
    type HighPriorityMsg = ();
    type ActorId = usize;

    fn id(&self) -> Self::ActorId {
        0
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
        match msg {
            SinkMessage::Packet(packet) => {
                let res = self.socket.send_to(&packet.raw, packet.dst).await;
                if let Err(err) = res {
                    tracing::warn!("failed to send packet to {:?}: {:?}", packet.dst, err);
                }
            }
        }
    }
}

impl SinkActor {
    pub fn new(socket: net::UnifiedSocket) -> Self {
        Self { socket }
    }
}

pub type SinkHandle = actor::LocalActorHandle<SinkActor>;
