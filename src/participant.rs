use std::{
    collections::HashMap,
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
    media::{MediaAdded, MediaData, Mid},
    net,
};
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    time::Instant,
};

use crate::{
    egress::EgressHandle,
    ingress::IngressHandle,
    message::{self, EgressUDPPacket, ParticipantId, TrackIn, TrackKey},
    proto,
    room::RoomHandle,
    track::TrackHandle,
};

use proto::sfu::client_message as client;
use proto::sfu::server_message as server;

const DATA_CHANNEL_LABEL: &str = "pulsebeam::sfu";

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("invalid sdp offer format: {0}")]
    InvalidOfferFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(RtcError),

    #[error("invalid rpc format: {0}")]
    InvalidRPCFormat(#[from] DecodeError),
}

#[derive(Debug)]
pub enum ParticipantMessage {
    UdpPacket(message::UDPPacket),
    NewTrack(TrackKey, TrackHandle),
    ForwardMedia(TrackKey, Arc<MediaData>),
}

struct TrackOut {
    track: TrackHandle,
    // pending=true means it needs to be negotiated
    pending: bool,
}

/// Reponsibilities:
/// * Manage Client Signaling
/// * Manage WebRTC PeerConnection
/// * Interact with Room
/// * Process Inbound Media
/// * Route Published Media to Track actor
/// * Manage Downlink Congestion Control
/// * Determine Subscription Layers
/// * Communicate Layer Preferences to Track actor
/// * Process Outbound Media from Track actor
/// * Send Outbound Media to Egress
/// * Route Subscriber RTCP Feedback to origin via Track actor
pub struct ParticipantActor {
    handle: ParticipantHandle,
    receiver: mpsc::Receiver<ParticipantMessage>,
    egress: EgressHandle,
    room: RoomHandle,
    participant_id: Arc<ParticipantId>,
    rtc: str0m::Rtc,
    cid: Option<ChannelId>,

    published_tracks: HashMap<Mid, TrackHandle>,
    subscribed_tracks: HashMap<TrackKey, TrackOut>,
}

impl ParticipantActor {
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

        // TODO: cleanup in the room
    }

    #[inline]
    async fn handle_message(&mut self, msg: ParticipantMessage) {
        match msg {
            ParticipantMessage::UdpPacket(packet) => {
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
            ParticipantMessage::NewTrack(key, track) => {
                if key.origin == self.participant_id {
                    // successfully publish a track
                    self.published_tracks.insert(key.mid, track);
                } else {
                    // new tracks from other participants
                    self.subscribed_tracks.insert(
                        key,
                        TrackOut {
                            track,
                            pending: true,
                        },
                    );
                }
            }
            ParticipantMessage::ForwardMedia(key, data) => {
                // forwarded media from track
            }
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
                            self.handle_new_media(e).await;
                        }
                        Event::ChannelOpen(cid, label) => {
                            if label == DATA_CHANNEL_LABEL {
                                self.cid = Some(cid);
                            }
                        }
                        Event::ChannelData(data) => {
                            if Some(data.id) == self.cid {
                                if let Err(err) = self.handle_rpc(data) {
                                    tracing::warn!("data channel dropped due to an error: {err}");
                                }
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
                            let key = TrackKey {
                                origin: self.participant_id.clone(),
                                mid: e.mid,
                            };

                            if let Some(track) = self.published_tracks.get(&e.mid) {
                                todo!();
                            }
                        }

                        _ => todo!(),
                    }
                    continue;
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

    fn send_server_event(&mut self, msg: proto::sfu::server_message::Message) {
        // TODO: handle when data channel is closed

        if let Some(mut ch) = self.cid.and_then(|cid| self.rtc.channel(cid)) {
            let encoded = proto::sfu::ServerMessage { message: Some(msg) }.encode_to_vec();
            ch.write(true, encoded.as_slice());
        }
    }

    fn handle_rpc(&mut self, data: ChannelData) -> Result<(), ParticipantError> {
        let msg = proto::sfu::ClientMessage::decode(data.data.as_slice())
            .map_err(ParticipantError::InvalidRPCFormat)?;

        match msg.message {
            Some(client::Message::Offer(sdp)) => {
                self.handle_offer(sdp)?;
            }
            _ => todo!(),
        };
        Ok(())
    }

    async fn handle_new_media(&mut self, media: MediaAdded) {
        // TODO: handle back pressure by buffering temporarily
        self.room
            .sender
            .send(crate::room::RoomMessage::PublishMedia(
                self.participant_id.clone(),
                TrackIn {
                    kind: media.kind,
                    mid: media.mid,
                    simulcast: media.simulcast,
                },
            ))
            .await;
    }

    fn handle_offer(&mut self, offer: String) -> Result<(), ParticipantError> {
        let offer =
            SdpOffer::from_sdp_string(&offer).map_err(ParticipantError::InvalidOfferFormat)?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ParticipantError::OfferRejected)?;
        self.send_server_event(server::Message::Answer(answer.to_sdp_string()));
        Ok(())
    }

    fn handle_renegotiate(&mut self) {
        let sdp = self.rtc.sdp_api();
        if !sdp.has_changes() {
            return;
        }

        let Some((offer, pending)) = sdp.apply() else {
            return;
        };

        self.send_server_event(server::Message::Answer(offer.to_sdp_string()));
    }
}

#[derive(Clone, Debug)]
pub struct ParticipantHandle {
    pub sender: mpsc::Sender<ParticipantMessage>,
    pub participant_id: Arc<ParticipantId>,
}

impl ParticipantHandle {
    pub fn new(
        ingress: IngressHandle,
        egress: EgressHandle,
        room: RoomHandle,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> (Self, ParticipantActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self {
            sender,
            participant_id: participant_id.clone(),
        };
        let actor = ParticipantActor {
            handle: handle.clone(),
            receiver,
            egress,
            room,
            participant_id,
            rtc,
            published_tracks: HashMap::new(),
            subscribed_tracks: HashMap::new(),
            cid: None,
        };
        (handle, actor)
    }

    pub fn forward(&self, msg: message::UDPPacket) -> Result<(), TrySendError<ParticipantMessage>> {
        self.sender.try_send(ParticipantMessage::UdpPacket(msg))
    }
}

impl Display for ParticipantHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.participant_id.as_str())
    }
}

impl Hash for ParticipantHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.participant_id.hash(state);
    }
}

impl PartialEq for ParticipantHandle {
    fn eq(&self, other: &Self) -> bool {
        self.participant_id == other.participant_id
    }
}

impl Eq for ParticipantHandle {}

impl PartialOrd for ParticipantHandle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParticipantHandle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.participant_id
            .as_str()
            .cmp(other.participant_id.as_str())
    }
}
