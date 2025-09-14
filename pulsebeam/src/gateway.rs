use std::{collections::HashMap, net::SocketAddr};

use crate::{
    ice,
    message::{self, UDPPacket},
    participant,
};
use bytes::Bytes;
use pulsebeam_runtime::{actor, net};

#[derive(Debug)]
pub enum GatewayControlMessage {
    AddParticipant(String, participant::ParticipantHandle),
    RemoveParticipant(String),
}

#[derive(Debug)]
pub enum GatewayDataMessage {
    Packet(message::EgressUDPPacket),
}

pub struct GatewayActor {
    local_addr: SocketAddr,
    socket: net::UnifiedSocket<'static>,
    conns: HashMap<String, participant::ParticipantHandle>,
    mapping: HashMap<SocketAddr, participant::ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
}

impl actor::Actor for GatewayActor {
    type HighPriorityMsg = GatewayControlMessage;
    type LowPriorityMsg = GatewayDataMessage;
    type Meta = usize;
    type ObservableState = ();

    fn meta(&self) -> Self::Meta {
        0
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        // let mut buf = BytesMut::with_capacity(128 * 1024);
        let mut buf = vec![0; 2000];

        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            res = self.socket.recv_from(&mut buf) => {
                match res {
                    Ok((size, source)) => self.handle_packet(source, &buf[..size]),
                    Err(err) => {
                        tracing::error!("udp socket is failing: {err}");
                        break;
                    },
                }
            }
        });

        tracing::info!("ingress has exited");
        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        match msg {
            GatewayControlMessage::AddParticipant(ufrag, participant) => {
                tracing::debug!("added {ufrag} to connection map");
                self.conns.insert(ufrag, participant);
            }
            GatewayControlMessage::RemoveParticipant(ufrag) => {
                tracing::debug!("removed {ufrag} from connection map");
                self.conns.remove(&ufrag);
                if let Some(addrs) = self.reverse.remove(&ufrag) {
                    for addr in addrs.iter() {
                        self.mapping.remove(addr);
                    }
                }
            }
        };
    }

    async fn on_low_priority(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
        match msg {
            GatewayDataMessage::Packet(packet) => {
                let res = self.socket.send_to(&packet.raw, packet.dst).await;
                if let Err(err) = res {
                    tracing::warn!("failed to send packet to {:?}: {:?}", packet.dst, err);
                }
            }
        }
    }
}

impl GatewayActor {
    // TODO: TCP candidate
    // TODO: TLS candidate
    // TODO: NAT rebinding
    pub fn handle_packet(&mut self, source: SocketAddr, packet: &[u8]) {
        let participant_handle = if let Some(participant_handle) = self.mapping.get(&source) {
            tracing::trace!("found connection from mapping: {source} -> {participant_handle:?}");
            participant_handle.clone()
        } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(packet) {
            if let Some(participant_handle) = self.conns.get(ufrag) {
                tracing::debug!(
                    "found connection from ufrag: {ufrag} -> {source} -> {participant_handle:?}"
                );
                self.mapping.insert(source, participant_handle.clone());
                self.reverse
                    .entry(ufrag.to_string())
                    .or_default()
                    .push(source);
                participant_handle.clone()
            } else {
                tracing::debug!(
                    "dropped a packet from {source} due to unregistered stun binding: {ufrag}"
                );
                return;
            }
        } else {
            tracing::debug!(
                "dropped a packet from {source} due to unexpected message flow from an unknown source"
            );
            return;
        };

        let _ = participant_handle.try_send_low(participant::ParticipantDataMessage::UdpPacket(
            UDPPacket {
                raw: Bytes::copy_from_slice(packet),
                src: source,
                dst: self.local_addr,
            },
        ));
    }
}

impl GatewayActor {
    pub fn new(local_addr: SocketAddr, socket: net::UnifiedSocket) -> Self {
        GatewayActor {
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
        }
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayActor>;
