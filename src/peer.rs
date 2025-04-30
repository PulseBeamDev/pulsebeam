use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use str0m::{Event, Input, Output, Rtc, RtcError, error::SdpError, net};
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    time::Instant,
};

use crate::{
    egress::EgressHandle,
    group::GroupHandle,
    message::{self, EgressUDPPacket, PeerId},
};

#[derive(thiserror::Error, Debug)]
pub enum PeerError {
    #[error("invalid sdp offer format")]
    InvalidOfferFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(RtcError),
}

#[derive(Debug)]
pub enum PeerMessage {
    UdpPacket(message::UDPPacket),
}

pub struct PeerActor {
    receiver: mpsc::Receiver<PeerMessage>,
    egress: EgressHandle,
    group: GroupHandle,
    peer_id: Arc<PeerId>,
    rtc: str0m::Rtc,
}

impl PeerActor {
    pub async fn run(mut self) {
        // TODO: notify ingress to add self to the routing table

        loop {
            let deadline = if let Some(deadline) = self.poll() {
                deadline
            } else {
                // Rtc timeout
                break;
            };

            tokio::select! {
                // prioritze network inputs
                biased;

                msg = self.receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_message(msg),
                        None => break,
                    }
                }

                _ = tokio::time::sleep(deadline) => {
                    // explicit empty, next loop polls again
                }
            }
        }

        // TODO: cleanup in the group
    }

    #[inline]
    fn handle_message(&mut self, msg: PeerMessage) {
        match msg {
            PeerMessage::UdpPacket(packet) => {
                let now = Instant::now();
                self.rtc.handle_input(Input::Receive(
                    now.into_std(),
                    net::Receive {
                        proto: net::Protocol::Udp,
                        source: packet.src,
                        destination: packet.dst,
                        contents: (&*packet.raw).try_into().unwrap(),
                    },
                ));
            }
        }
    }

    fn poll(&mut self) -> Option<Duration> {
        // WARN: be careful with spending too much time in this loop.
        // We should yield back to the scheduler based on some heuristic here.
        loop {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            let timeout = match self.rtc.poll_output().unwrap() {
                // Stop polling when we get the timeout.
                Output::Timeout(v) => Instant::from_std(v),

                // Transmit this data to the remote peer. Typically via
                // a UDP socket. The destination IP comes from the ICE
                // agent. It might change during the session.
                Output::Transmit(v) => {
                    let packet = Bytes::copy_from_slice(&v.contents);
                    self.egress.send(EgressUDPPacket {
                        raw: packet,
                        dst: v.destination,
                    });
                    continue;
                }

                // Events are mainly incoming media data from the remote
                // peer, but also data channel data and statistics.
                Output::Event(v) => {
                    match v {
                        // Abort if we disconnect.
                        Event::IceConnectionStateChange(
                            str0m::IceConnectionState::Disconnected,
                        ) => return None,
                        Event::MediaAdded(e) => todo!(),
                        Event::MediaData(e) => {
                            todo!()
                        }

                        _ => continue,
                    }
                }
            };

            // Duration until timeout.
            let now = Instant::now();
            let duration = timeout - now;

            if duration.is_zero() {
                // Drive time forwards in rtc straight away.
                self.rtc
                    .handle_input(Input::Timeout(now.into_std()))
                    .unwrap();
                continue;
            }

            return Some(duration);
        }
    }
}

#[derive(Clone, Debug)]
pub struct PeerHandle {
    sender: mpsc::Sender<PeerMessage>,
}

impl PeerHandle {
    pub fn new(
        egress: EgressHandle,
        group: GroupHandle,
        peer_id: Arc<PeerId>,
        rtc: Rtc,
    ) -> (Self, PeerActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self { sender };
        let actor = PeerActor {
            receiver,
            egress,
            group,
            peer_id,
            rtc,
        };
        (handle, actor)
    }

    pub fn forward(&self, msg: message::UDPPacket) -> Result<(), TrySendError<PeerMessage>> {
        self.sender.try_send(PeerMessage::UdpPacket(msg))
    }
}
