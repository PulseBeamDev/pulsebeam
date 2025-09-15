use crate::{ice, participant};
use bytes::BytesMut;
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

pub struct GatewayActor {
    local_addr: SocketAddr,
    socket: net::UnifiedSocket<'static>,
    conns: HashMap<String, participant::ParticipantHandle>,
    mapping: HashMap<SocketAddr, participant::ParticipantHandle>,
    reverse: HashMap<String, Vec<SocketAddr>>,
    recv_batch: Vec<net::RecvPacket>,
    send_batch: Vec<net::SendPacket>,
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
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Ok(_) = self.socket.readable() => {
                if let Err(err) = self.read_socket() {
                    tracing::error!("failed to read socket: {err}");
                    break;
                }
            }
            Ok(_) = self.socket.writable() => {
                if let Err(err) = self.write_socket() {
                    tracing::error!("failed to write socket: {err}");
                    break;
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
            // TODO: each participant needs to own their pacers and bandwidth estimator.
            // TODO: gateway needs to batch packets from multiple participants
            GatewayDataMessage::Packet(packet) => {
                todo!("buffer packets to send_batch");
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
                buf: BytesMut::with_capacity(1500),
                len: 0,
                src: "0.0.0.0:0".parse().unwrap(),
                dst: "0.0.0.0:0".parse().unwrap(),
            })
            .collect();
        // Pre-allocate send batch capacity
        let send_batch: Vec<net::SendPacket> = Vec::with_capacity(BATCH_SIZE);

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

    fn read_socket(&mut self) -> io::Result<()> {
        let num_received = self.socket.try_recv_batch(&mut self.recv_batch)?;

        for packet in &mut self.recv_batch[..num_received] {
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

        Ok(())
    }

    fn write_socket(&mut self) -> io::Result<()> {
        self.socket.try_send_batch(&self.send_batch)?;
        Ok(())
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayActor>;
