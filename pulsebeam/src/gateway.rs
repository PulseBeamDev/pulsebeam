use crate::{ice, participant};
use bytes::BytesMut;
use pulsebeam_runtime::{actor, net};
use std::{collections::HashMap, net::SocketAddr};

#[derive(Debug)]
pub enum GatewayControlMessage {
    AddParticipant(String, participant::ParticipantHandle),
    RemoveParticipant(String),
}

#[derive(Debug)]
pub enum GatewayDataMessage {
    Packet(net::SendPacket),
}

pub struct GatewayActor {
    local_addr: SocketAddr,
    socket: net::UnifiedSocket<'static>,
    conns: HashMap<String, participant::ParticipantHandle>,
    mapping: HashMap<SocketAddr, participant::ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
    recv_batch: Vec<net::RecvPacket>, // Pre-allocated for receives
    send_batch: Vec<net::SendPacket>, // Pre-allocated for sends
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
        const BATCH_SIZE: usize = 32; // Tune based on load (e.g., packet rate)

        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {
            // Send any pending packets in send_batch
            if !self.send_batch.is_empty() {
                match self.socket.send_batch(&self.send_batch).await {
                    Ok(sent) => {
                        tracing::debug!("Sent {} packets in batch", sent);
                        self.send_batch.clear(); // Clear after successful send
                    }
                    Err(err) => {
                        tracing::warn!("Failed to send batch: {:?}", err);
                        // Keep packets for retry in next iteration
                    }
                }
            }
        },
        select: {
            // Receive a batch of packets
            res = self.socket.recv_batch(&mut self.recv_batch) => {
                match res {
                    Ok(num_received) if num_received > 0 => {
                        self.handle_packet_batch(&self.recv_batch[..num_received]);
                    }
                    Ok(_) => {} // No packets received; continue
                    Err(err) => {
                        tracing::error!("UDP socket failed in recv_batch: {}", err);
                        break;
                    }
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
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
        match msg {
            GatewayDataMessage::Packet(packet) => {
                // Add to send_batch (zero-copy, reuse Bytes)
                self.send_batch.push(packet);
            }
        }
    }
}

impl GatewayActor {
    pub fn new(local_addr: SocketAddr, socket: net::UnifiedSocket<'static>) -> Self {
        const BATCH_SIZE: usize = 32;
        // Pre-allocate receive batch with MTU-sized buffers
        let recv_batch = (0..BATCH_SIZE)
            .map(|_| net::RecvPacket {
                buf: BytesMut::with_capacity(1500), // MTU for RTP packets
                len: 0,
                src: "0.0.0.0:0".parse().unwrap(),
            })
            .collect();
        // Pre-allocate send batch capacity
        let send_batch = Vec::with_capacity(BATCH_SIZE);

        GatewayActor {
            local_addr,
            socket,
            conns: HashMap::new(),
            mapping: HashMap::new(),
            reverse: HashMap::new(),
            recv_batch,
            send_batch,
        }
    }

    fn handle_packet_batch(&mut self, packets: &[net::RecvPacket]) {
        for packet in packets {
            let payload = &packet.buf[..packet.len];
            let participant_handle = if let Some(participant_handle) = self.mapping.get(&packet.src)
            {
                tracing::trace!(
                    "found connection from mapping: {} -> {:?}",
                    packet.src,
                    participant_handle
                );
                participant_handle.clone()
            } else if let Some(ufrag) = ice::parse_stun_remote_ufrag(payload) {
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
                    tracing::debug!(
                        "dropped a packet from {} due to unregistered stun binding: {}",
                        packet.src,
                        ufrag
                    );
                    continue;
                }
            } else {
                tracing::debug!(
                    "dropped a packet from {} due to unexpected message flow from an unknown source",
                    packet.src
                );
                continue;
            };

            // Zero-copy: Create Bytes from received buffer
            let _ = participant_handle.try_send_low(
                participant::ParticipantDataMessage::UdpPacket(net::RecvPacket {
                    buf: packet.buf.split_to(packet.len),
                    len: packet.len,
                    src: packet.src,
                    dst: self.local_addr,
                }),
            );
        }
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayActor>;
