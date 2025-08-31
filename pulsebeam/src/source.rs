use std::{collections::HashMap, net::SocketAddr};

use crate::{ice, message::UDPPacket, participant};
use bytes::Bytes;
use pulsebeam_runtime::{
    actor::{self, ActorHandle},
    net,
};

pub enum SourceControlMessage {
    AddParticipant(String, participant::ParticipantHandle),
    RemoveParticipant(String),
}

pub struct SourceActor {
    local_addr: SocketAddr,
    socket: net::UnifiedSocket,
    conns: HashMap<String, participant::ParticipantHandle>,
    mapping: HashMap<SocketAddr, participant::ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
}

impl actor::Actor for SourceActor {
    type HighPriorityMsg = SourceControlMessage;
    type LowPriorityMsg = ();
    type ActorId = usize;

    fn id(&self) -> Self::ActorId {
        0
    }

    async fn process(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
    ) -> Result<(), actor::ActorError> {
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];

        loop {
            tokio::select! {
                biased;
                Some(msg) = ctx.hi_rx.recv() => {
                    self.on_high_priority(ctx, msg).await;
                }
                res = self.socket.recv_from(&mut buf) => {
                    match res {
                        Ok((size, source)) => self.handle_packet(source, &buf[..size]),
                        Err(err) => {
                            tracing::error!("udp socket is failing: {err}");
                            break;
                        },
                    }
                }
            }
        }

        tracing::info!("ingress has exited");
        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        match msg {
            SourceControlMessage::AddParticipant(ufrag, participant) => {
                tracing::trace!("added {ufrag} to connection map");
                self.conns.insert(ufrag, participant);
            }
            SourceControlMessage::RemoveParticipant(ufrag) => {
                tracing::trace!("removed {ufrag} to connection map");
                self.conns.remove(&ufrag);
                if let Some(addrs) = self.reverse.remove(&ufrag) {
                    for addr in addrs.iter() {
                        self.mapping.remove(addr);
                    }
                }
            }
        };
    }
}

impl SourceActor {
    pub fn handle_packet(&mut self, source: SocketAddr, packet: &[u8]) {
        let participant_handle = if let Some(participant_handle) = self.mapping.get(&source) {
            tracing::trace!("found connection from mapping: {source} -> {participant_handle}");
            participant_handle.clone()
        } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(packet) {
            if let Some(participant_handle) = self.conns.get(ufrag) {
                tracing::trace!(
                    "found connection from ufrag: {ufrag} -> {source} -> {participant_handle}"
                );
                self.mapping.insert(source, participant_handle.clone());
                self.reverse
                    .entry(ufrag.to_string())
                    .or_default()
                    .push(source);
                participant_handle.clone()
            } else {
                tracing::trace!(
                    "dropped a packet from {source} due to unregistered stun binding: {ufrag}"
                );
                return;
            }
        } else {
            tracing::trace!(
                "dropped a packet from {source} due to unexpected message flow from an unknown source"
            );
            return;
        };

        let _ =
            participant_handle
                .handle
                .try_send_low(participant::ParticipantDataMessage::UdpPacket(UDPPacket {
                    raw: Bytes::copy_from_slice(packet),
                    src: source,
                    dst: self.local_addr,
                }));
    }
}

impl SourceActor {
    pub fn new(local_addr: SocketAddr, socket: net::UnifiedSocket) -> Self {
        SourceActor {
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
        }
    }
}

pub type SourceHandle = actor::LocalActorHandle<SourceActor>;
