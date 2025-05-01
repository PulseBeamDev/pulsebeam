use std::{
    fmt::Display,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::SdpOffer,
    channel::{ChannelData, ChannelId},
    error::SdpError,
    media::{Direction, MediaAdded, MediaData},
    net,
};
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    time::Instant,
};

use crate::{
    egress::EgressHandle,
    group::{GroupHandle, GroupMessage},
    message::{self, EgressUDPPacket, MediaKey, PeerId},
    proto,
};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::sfu";

#[derive(thiserror::Error, Debug)]
pub enum PeerError {
    #[error("invalid sdp offer format: {0}")]
    InvalidOfferFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(RtcError),
}

#[derive(thiserror::Error, Debug)]
pub enum RPCError {
    #[error("invalid rpc format: {0}")]
    InvalidRPCFormat(#[from] DecodeError),

    #[error("invalid sdp offer format: {0}")]
    InvalidOfferFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(#[from] RtcError),
}

#[derive(Debug)]
pub enum PeerMessage {
    UdpPacket(message::UDPPacket),
    PublishMedia(MediaKey, Arc<MediaAdded>),
    SubscribeMedia(MediaKey, Arc<MediaAdded>),
    ForwardMedia(MediaKey, Arc<MediaData>),
}

pub struct PeerActor {
    handle: PeerHandle,
    receiver: mpsc::Receiver<PeerMessage>,
    egress: EgressHandle,
    group: GroupHandle,
    peer_id: Arc<PeerId>,
    rtc: str0m::Rtc,
    cid: Option<ChannelId>,
}

impl PeerActor {
    pub async fn run(mut self) {
        // TODO: notify ingress to add self to the routing table

        loop {
            let deadline = if let Some(deadline) = self.poll().await {
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
                        Some(msg) => self.handle_message(msg).await,
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
    async fn handle_message(&mut self, msg: PeerMessage) {
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
            PeerMessage::PublishMedia(key, media) => {
                // TODO: selective based on the client instead of auto subscribing

                self.group
                    .sender
                    .send(GroupMessage::SubscribeMedia(key, self.handle.clone()))
                    .await;
            }
            PeerMessage::SubscribeMedia(key, media) => {
                let mut sdp = self.rtc.sdp_api();
                let mid = sdp.add_media(
                    media.kind,
                    Direction::SendOnly,
                    Some(key.peer_id.to_string()),
                    None,
                    None,
                );
            }
            PeerMessage::ForwardMedia(key, data) => {}
        }
    }

    async fn poll(&mut self) -> Option<Duration> {
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
                        Event::MediaAdded(e) => {
                            // TODO: handle back pressure by buffering temporarily
                            self.group
                                .sender
                                .send(crate::group::GroupMessage::PublishMedia(
                                    MediaKey {
                                        peer_id: self.peer_id.clone(),
                                        mid: e.mid,
                                    },
                                    Arc::new(e),
                                ))
                                .await;
                        }
                        Event::ChannelOpen(cid, label) => {
                            if label == DATA_CHANNEL_LABEL {
                                self.cid = Some(cid);
                            }
                        }
                        Event::ChannelData(data) => {
                            if Some(data.id) == self.cid {
                                self.handle_rpc(data);
                            } else {
                                todo!("forward data channel");
                            }
                        }
                        Event::ChannelClose(cid) => {
                            if Some(cid) == self.cid {
                                self.rtc.disconnect();
                            } else {
                                todo!("forward data channel");
                            }
                        }
                        Event::MediaData(e) => {
                            todo!();
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

    fn handle_rpc(&mut self, data: ChannelData) -> Result<(), RPCError> {
        use proto::sfu::client_message as client;
        use proto::sfu::server_message as server;

        let msg = proto::sfu::ClientMessage::decode(data.data.as_slice())
            .map_err(RPCError::InvalidRPCFormat)?;

        let reply: server::Message = match msg.message {
            Some(client::Message::Offer(sdp)) => {
                let offer =
                    SdpOffer::from_sdp_string(&sdp).map_err(RPCError::InvalidOfferFormat)?;
                let answer = self.rtc.sdp_api().accept_offer(offer)?;
                server::Message::Answer(answer.to_sdp_string())
            }
            _ => todo!(),
        };

        // TODO: handle when data channel is closed
        if let Some(mut ch) = self.rtc.channel(data.id) {
            let encoded = proto::sfu::ServerMessage {
                message: Some(reply),
            }
            .encode_to_vec();
            ch.write(true, encoded.as_slice());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PeerHandle {
    pub sender: mpsc::Sender<PeerMessage>,
    pub peer_id: Arc<PeerId>,
}

impl PeerHandle {
    pub fn new(
        egress: EgressHandle,
        group: GroupHandle,
        peer_id: Arc<PeerId>,
        rtc: Rtc,
    ) -> (Self, PeerActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self {
            sender,
            peer_id: peer_id.clone(),
        };
        let actor = PeerActor {
            handle: handle.clone(),
            receiver,
            egress,
            group,
            peer_id,
            rtc,
            cid: None,
        };
        (handle, actor)
    }

    pub fn forward(&self, msg: message::UDPPacket) -> Result<(), TrySendError<PeerMessage>> {
        self.sender.try_send(PeerMessage::UdpPacket(msg))
    }
}

impl Display for PeerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.peer_id.as_str())
    }
}

impl Hash for PeerHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.peer_id.hash(state);
    }
}

impl PartialEq for PeerHandle {
    fn eq(&self, other: &Self) -> bool {
        self.peer_id == other.peer_id
    }
}

impl Eq for PeerHandle {}

impl PartialOrd for PeerHandle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PeerHandle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.peer_id.as_str().cmp(other.peer_id.as_str())
    }
}
