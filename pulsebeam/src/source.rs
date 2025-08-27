use std::{collections::HashMap, net::SocketAddr};

use crate::{ice, message::UDPPacket, net::PacketSocket, participant::ParticipantHandle};
use bytes::Bytes;
use pulsebeam_runtime::{actor, net};

pub enum SourceControlMessage {
    AddParticipant(String, ParticipantHandle),
    RemoveParticipant(String),
}

pub struct SourceActor {
    local_addr: SocketAddr,
    socket: net::UnifiedSocket,
    conns: HashMap<String, ParticipantHandle>,
    mapping: HashMap<SocketAddr, ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
}

impl actor::Actor for SourceActor {
    type HighPriorityMessage = SourceControlMessage;
    type LowPriorityMessage = ();
    type ID = usize;

    fn kind(&self) -> &'static str {
        "source"
    }

    fn id(&self) -> Self::ID {
        0
    }

    async fn run(&mut self, mut ctx: actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];
        drop(ctx.lo_rx); // unused channel

        loop {
            tokio::select! {
                biased;
                msg = ctx.hi_rx.recv() => {
                    match msg {
                        Some(msg) => self.handle_control(msg),
                        None => {
                            tracing::info!("all controllers have exited, will gracefully shutdown");
                            break;
                        }
                    }
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

        let _ = participant_handle.forward(UDPPacket {
            raw: Bytes::copy_from_slice(packet),
            src: source,
            dst: self.local_addr,
        });
    }

    pub fn handle_control(&mut self, msg: SourceControlMessage) {
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
    fn new(local_addr: SocketAddr, socket: net::UnifiedSocket) -> Self {
        let actor = SourceActor {
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
        };
    }
}

pub type SourceHandle = actor::LocalActorHandle<SourceActor>;
