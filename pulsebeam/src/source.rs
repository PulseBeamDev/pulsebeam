use std::{collections::HashMap, net::SocketAddr};

use crate::{
    actor::{Actor, ActorError},
    ice,
    message::UDPPacket,
    net::PacketSocket,
    participant::ParticipantHandle,
};
use bytes::Bytes;
use tokio::sync::mpsc::{self, error::SendError};

pub enum UdpSourceMessage {
    AddParticipant(String, ParticipantHandle),
    RemoveParticipant(String),
}

pub struct UdpSourceActor<S> {
    receiver: mpsc::Receiver<UdpSourceMessage>,
    local_addr: SocketAddr,
    socket: S,
    conns: HashMap<String, ParticipantHandle>,
    mapping: HashMap<SocketAddr, ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
}

impl<S: PacketSocket> Actor for UdpSourceActor<S> {
    type ID = usize;

    fn kind(&self) -> &'static str {
        "udp_source"
    }

    fn id(&self) -> Self::ID {
        0
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];

        loop {
            tokio::select! {
                res = self.socket.recv_from(&mut buf) => {
                    match res {
                        Ok((size, source)) => self.handle_packet(source, &buf[..size]),
                        Err(err) => {
                            tracing::error!("udp socket is failing: {err}");
                            break;
                        },
                    }
                }

                msg = self.receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_control(msg),
                        None => {
                            tracing::info!("all controllers have exited, will gracefully shutdown");
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("ingress has exited");
        Ok(())
    }
}

impl<S: PacketSocket> UdpSourceActor<S> {
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

    pub fn handle_control(&mut self, msg: UdpSourceMessage) {
        match msg {
            UdpSourceMessage::AddParticipant(ufrag, participant) => {
                tracing::trace!("added {ufrag} to connection map");
                self.conns.insert(ufrag, participant);
            }
            UdpSourceMessage::RemoveParticipant(ufrag) => {
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

#[derive(Clone, Debug)]
pub struct UdpSourceHandle {
    sender: mpsc::Sender<UdpSourceMessage>,
}

impl UdpSourceHandle {
    pub fn new<S: PacketSocket>(local_addr: SocketAddr, socket: S) -> (Self, UdpSourceActor<S>) {
        let (sender, receiver) = mpsc::channel(1);
        let handle = Self { sender };
        let actor = UdpSourceActor {
            receiver,
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        ufrag: String,
        participant: ParticipantHandle,
    ) -> Result<(), SendError<UdpSourceMessage>> {
        self.sender
            .send(UdpSourceMessage::AddParticipant(ufrag, participant))
            .await
    }

    pub async fn remove_participant(
        &self,
        ufrag: String,
    ) -> Result<(), SendError<UdpSourceMessage>> {
        self.sender
            .send(UdpSourceMessage::RemoveParticipant(ufrag))
            .await
    }
}
