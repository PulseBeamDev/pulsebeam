use std::{collections::VecDeque, net::SocketAddr};

use crate::clock::WallAnchor;
use pulsebeam_rtc::{
    DataChannel, DataPayload, DatagramProtocol, DepartureReceipt, IngressDatagram, RtcEvent,
    RtcPeer, RtcPeerError, Transmit,
};
use pulsebeam_runtime::net::{RecvPacketBatch, Transport};
use tokio::time::Instant;

const MAX_INGRESS_PER_TICK: usize = 64;

pub struct DirectTransport {
    peer: RtcPeer,
    ingress: VecDeque<RecvPacketBatch>,
    last_ingress: Option<(SocketAddr, SocketAddr)>,
}

impl DirectTransport {
    pub fn new(peer: RtcPeer) -> Self {
        Self {
            peer,
            ingress: VecDeque::with_capacity(MAX_INGRESS_PER_TICK),
            last_ingress: None,
        }
    }

    pub fn enqueue(&mut self, batch: RecvPacketBatch) {
        self.last_ingress = Some((batch.src, batch.dst));
        if self.ingress.len() >= MAX_INGRESS_PER_TICK {
            let _ = self.ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.ingress.push_back(batch);
    }

    pub fn ingress_context(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.last_ingress
    }

    pub fn process_ingress(
        &mut self,
        now: Instant,
        wall: &WallAnchor,
    ) -> Result<usize, RtcPeerError> {
        let mut processed = 0usize;
        while processed < MAX_INGRESS_PER_TICK {
            let Some(batch) = self.ingress.front_mut() else {
                break;
            };
            let source = batch.src;
            let destination = batch.dst;
            let protocol = match batch.transport {
                Transport::Udp(_) => DatagramProtocol::Udp,
                Transport::Tcp => DatagramProtocol::Tcp,
            };
            let Some(bytes) = batch.next_packet() else {
                let _ = self.ingress.pop_front();
                continue;
            };
            self.peer.handle_datagram(
                now.into_std(),
                IngressDatagram::new(
                    protocol,
                    source,
                    destination,
                    bytes.to_vec(),
                    wall.project(now).map_err(|error| {
                        RtcPeerError::Transport(format!(
                            "ingress wall projection failed: {error:?}"
                        ))
                    })?,
                ),
            )?;
            processed = processed.saturating_add(1);
        }
        Ok(processed)
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        self.peer.handle_timeout(now.into_std())
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        self.peer.next_deadline().map(Instant::from_std)
    }

    pub fn poll_event(&mut self) -> Option<RtcEvent> {
        self.peer.poll_event()
    }

    pub fn forward(
        &mut self,
        now: Instant,
        slot: pulsebeam_rtc::EgressSlot,
        packet: &pulsebeam_rtc::MediaPacket,
        rewrite: pulsebeam_rtc::MediaRewrite,
    ) -> Result<(), RtcPeerError> {
        self.peer.forward(now.into_std(), slot, packet, rewrite)
    }

    pub fn forward_transit(
        &mut self,
        now: Instant,
        slot: pulsebeam_rtc::EgressSlot,
        packet: &pulsebeam_rtc::TransitMediaPacket,
        rewrite: pulsebeam_rtc::MediaRewrite,
    ) -> Result<(), RtcPeerError> {
        self.peer
            .forward_transit(now.into_std(), slot, packet, rewrite)
    }

    pub fn request_keyframe(
        &mut self,
        now: Instant,
        stream: pulsebeam_rtc::IngressStream,
    ) -> Result<(), RtcPeerError> {
        self.peer.request_keyframe(now.into_std(), stream)
    }

    pub fn set_desired_bitrate(
        &mut self,
        now: Instant,
        bitrate_bps: u64,
    ) -> Result<(), RtcPeerError> {
        self.peer.set_desired_bitrate(now.into_std(), bitrate_bps)
    }

    pub fn set_current_bitrate(
        &mut self,
        now: Instant,
        bitrate_bps: u64,
    ) -> Result<(), RtcPeerError> {
        self.peer.set_current_bitrate(now.into_std(), bitrate_bps)
    }

    pub fn send_data(
        &mut self,
        channel: DataChannel,
        payload: DataPayload,
        now: Instant,
    ) -> Result<(), RtcPeerError> {
        self.peer.send_data(now.into_std(), channel, payload)
    }

    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        self.peer.poll_transmit(now.into_std())
    }

    pub fn confirm_departure(
        &mut self,
        receipt: DepartureReceipt,
        now: Instant,
    ) -> Result<Option<pulsebeam_rtc::ForwardingLatency>, RtcPeerError> {
        self.peer.confirm_departure(receipt, now.into_std())
    }

    pub fn abandon_departure(&mut self, receipt: DepartureReceipt) -> Result<(), RtcPeerError> {
        self.peer.abandon_departure(receipt)
    }
}
