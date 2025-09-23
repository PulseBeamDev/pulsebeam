use crate::{ice, participant};
use pulsebeam_runtime::{actor, net};
use std::{collections::HashMap, io, net::SocketAddr};

#[derive(Debug)]
pub enum GatewayControlMessage {
    AddParticipant(String, participant::ParticipantHandle),
    RemoveParticipant(String),
}

#[derive(Debug)]
pub enum GatewayDataMessage {
    Packet(net::SendPacket),
}

pub struct GatewayMessageSet;

impl actor::MessageSet for GatewayMessageSet {
    type HighPriorityMsg = GatewayControlMessage;
    type LowPriorityMsg = GatewayDataMessage;
    type Meta = usize;
    type ObservableState = ();
}

pub struct GatewayActor {
    socket: net::UnifiedSocket<'static>,
    conns: HashMap<String, participant::ParticipantHandle>,
    mapping: HashMap<SocketAddr, participant::ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
    recv_batch: Vec<net::RecvPacket>,
    send_incoming_batch: Vec<net::SendPacket>,
    send_outgoing_batch: Vec<net::SendPacket>,
}

impl actor::Actor<GatewayMessageSet> for GatewayActor {
    fn meta(&self) -> usize {
        0
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            // biased toward writing to socket
            Ok(_) = self.socket.writable(), if !self.send_outgoing_batch.is_empty() || !self.send_incoming_batch.is_empty() => {
                if let Err(err) = self.write_socket() {
                    tracing::error!("failed to write socket: {err}");
                    break;
                }
            }
            Ok(_) = self.socket.readable() => {
                if let Err(err) = self.read_socket() {
                    tracing::error!("failed to read socket: {err}");
                    break;
                }
            }
        });

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
        msg: GatewayControlMessage,
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
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
        msg: GatewayDataMessage,
    ) -> () {
        match msg {
            // TODO: each participant needs to own their pacers and bandwidth estimator.
            // TODO: gateway needs to batch packets from multiple participants
            GatewayDataMessage::Packet(packet) => {
                // If the outgoing buffer is empty, swap it with the incoming one.
                if self.send_outgoing_batch.is_empty() {
                    std::mem::swap(&mut self.send_incoming_batch, &mut self.send_outgoing_batch);
                }
                self.send_incoming_batch.push(packet);
            }
        }
    }
}

impl GatewayActor {
    const BATCH_SIZE: usize = 64;

    pub fn new(socket: net::UnifiedSocket<'static>) -> Self {
        // Pre-allocate receive batch with MTU-sized buffers
        let recv_batch = Vec::with_capacity(Self::BATCH_SIZE);
        let send_incoming_batch = Vec::with_capacity(Self::BATCH_SIZE);
        let send_outgoing_batch = Vec::with_capacity(Self::BATCH_SIZE);

        Self {
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
            recv_batch,
            send_incoming_batch,
            send_outgoing_batch,
        }
    }

    fn read_socket(&mut self) -> io::Result<()> {
        // the loop after reading should always clear the buffer
        assert!(self.recv_batch.is_empty());
        let batch_size = self.recv_batch.capacity();
        let count = self
            .socket
            .try_recv_batch(&mut self.recv_batch, batch_size)?;

        tracing::trace!("received {count} packets from socket");
        for packet in self.recv_batch.drain(..) {
            let mut participant_handle = if let Some(participant_handle) =
                self.mapping.get(&packet.src)
            {
                tracing::trace!(
                    "found connection from mapping: {} -> {:?}",
                    packet.src,
                    participant_handle
                );
                participant_handle.clone()
            } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(&packet.buf) {
                if let Some(participant_handle) = self.conns.get(ufrag) {
                    tracing::debug!(
                        "found connection from ufrag: {} -> {} -> {:?}",
                        ufrag,
                        packet.src,
                        participant_handle
                    );
                    self.mapping.insert(packet.src, participant_handle.clone());
                    self.reverse
                        .entry(ufrag.to_string())
                        .or_default()
                        .push(packet.src);
                    participant_handle.clone()
                } else {
                    tracing::trace!(
                        "dropped a packet from {} due to unregistered stun binding: {}",
                        packet.src,
                        ufrag
                    );
                    continue;
                }
            } else {
                tracing::trace!(
                    "dropped a packet from {} due to unexpected message flow from an unknown source",
                    packet.src
                );
                continue;
            };

            let _ = participant_handle
                .try_send_low(participant::ParticipantDataMessage::UdpPacket(packet));
        }

        Ok(())
    }

    fn write_socket(&mut self) -> io::Result<()> {
        // If the outgoing buffer is empty, swap it with the incoming one.
        if self.send_outgoing_batch.is_empty() {
            std::mem::swap(&mut self.send_incoming_batch, &mut self.send_outgoing_batch);
        }

        // writable check MUST always make sure to only proceed with non-empty buffer.
        assert!(!self.send_outgoing_batch.is_empty());
        let sent_count = self.socket.try_send_batch(&self.send_outgoing_batch)?;
        tracing::trace!("sent {sent_count} packets to socket");
        self.send_outgoing_batch.drain(..sent_count);
        Ok(())
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayMessageSet>;
