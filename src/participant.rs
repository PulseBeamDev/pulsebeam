use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc, time::Duration};

use bytes::Bytes;
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    channel::{ChannelData, ChannelId},
    error::SdpError,
    media::{MediaAdded, MediaData, Mid},
    net::{self, Transmit},
};
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    time::Instant,
};

use crate::{
    entity::{ParticipantId, TrackId},
    message::{self, EgressUDPPacket, TrackIn},
    proto,
    room::RoomHandle,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
    track::TrackHandle,
};

use proto::sfu::client_message as client;
use proto::sfu::server_message as server;

const DATA_CHANNEL_LABEL: &str = "pulsebeam::sfu";

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("invalid sdp format: {0}")]
    InvalidSdpFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(RtcError),

    #[error("invalid rpc format: {0}")]
    InvalidRPCFormat(#[from] DecodeError),
}

#[derive(Debug)]
pub enum ParticipantMessage {
    UdpPacket(message::UDPPacket),
    NewTrack(TrackHandle),
    ForwardMedia(Arc<TrackIn>, Arc<MediaData>),
}

#[derive(Debug)]
struct TrackOut {
    track_in: Arc<TrackIn>,
    state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m) | TrackOutState::Open(m) => Some(m),
        }
    }
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
    sink: UdpSinkHandle,
    room: RoomHandle,
    participant_id: Arc<ParticipantId>,
    rtc: str0m::Rtc,
    cid: Option<ChannelId>,
    pending: Option<SdpPendingOffer>,

    published_tracks: HashMap<Mid, TrackHandle>,
    subscribed_tracks: HashMap<Arc<TrackId>, TrackOut>,
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
            ParticipantMessage::NewTrack(track) => {
                if track.meta.id.origin_participant == self.participant_id {
                    // successfully publish a track
                    self.published_tracks
                        .insert(track.meta.id.origin_mid, track);
                } else {
                    // new tracks from other participants
                    let track_out = TrackOut {
                        track_in: track.meta.clone(),
                        state: TrackOutState::ToOpen,
                    };
                    self.subscribed_tracks
                        .insert(track.meta.id.clone(), track_out);
                }
            }
            ParticipantMessage::ForwardMedia(track, data) => {
                // forwarded media from track
            }
        }
    }

    async fn poll(&mut self) -> Option<Duration> {
        // WARN: be careful with spending too much time in this loop.
        // We should yield back to the scheduler based on some heuristic here.
        while self.rtc.is_alive() {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            let timeout = match self.rtc.poll_output().unwrap() {
                // Stop polling when we get the timeout.
                Output::Timeout(v) => Instant::from_std(v),

                // Transmit this data to the remote peer. Typically via
                // a UDP socket. The destination IP comes from the ICE
                // agent. It might change during the session.
                Output::Transmit(v) => {
                    self.handle_output_transmit(v);
                    continue;
                }

                // Events are mainly incoming media data from the remote
                // peer, but also data channel data and statistics.
                Output::Event(v) => {
                    self.handle_output_event(v).await;
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

        None
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
            Some(client::Message::Answer(sdp)) => {
                self.handle_answer(sdp)?;
            }
            _ => todo!(),
        };
        Ok(())
    }

    fn handle_output_transmit(&mut self, t: Transmit) {
        let packet = Bytes::copy_from_slice(&t.contents);
        self.sink.send(EgressUDPPacket {
            raw: packet,
            dst: t.destination,
        });
    }

    async fn handle_output_event(&mut self, event: Event) {
        match event {
            // Abort if we disconnect.
            Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                self.rtc.disconnect();
            }
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
                if let Some(track) = self.published_tracks.get(&e.mid) {
                    todo!();
                }
            }

            _ => todo!(),
        }
    }

    async fn handle_new_media(&mut self, media: MediaAdded) {
        // TODO: handle back pressure by buffering temporarily
        let track_id = TrackId::new(self.participant_id.clone(), media.mid);
        let track_id = Arc::new(track_id);
        self.room
            .sender
            .send(crate::room::RoomMessage::PublishMedia(TrackIn {
                id: track_id,
                kind: media.kind,
                simulcast: media.simulcast,
            }))
            .await;
    }

    fn handle_offer(&mut self, offer: String) -> Result<(), ParticipantError> {
        let offer =
            SdpOffer::from_sdp_string(&offer).map_err(ParticipantError::InvalidSdpFormat)?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ParticipantError::OfferRejected)?;

        // Keep local track state in sync, cancelling any pending negotiation
        // so we can redo it after this offer is handled.
        for (_, track) in &mut self.subscribed_tracks {
            if let TrackOutState::Negotiating(_) = track.state {
                track.state = TrackOutState::ToOpen;
            }
        }

        self.send_server_event(server::Message::Answer(answer.to_sdp_string()));
        Ok(())
    }

    fn handle_answer(&mut self, answer: String) -> Result<(), ParticipantError> {
        let answer =
            SdpAnswer::from_sdp_string(&answer).map_err(ParticipantError::InvalidSdpFormat)?;

        if let Some(pending) = self.pending.take() {
            self.rtc
                .sdp_api()
                .accept_answer(pending, answer)
                .expect("answer to be accepted");

            for (_, track) in self.subscribed_tracks.iter_mut() {
                if let TrackOutState::Negotiating(m) = track.state {
                    track.state = TrackOutState::Open(m);
                }
            }
        }
        Ok(())
    }

    fn negotiation_if_needed(&mut self) -> bool {
        if self.cid.is_none() || self.pending.is_some() {
            // Don't negotiate if there is no data channel, or if we have pending changes already.
            return false;
        }

        let mut sdp = self.rtc.sdp_api();

        for (track_id, track) in self.subscribed_tracks.iter_mut() {
            if track.state == TrackOutState::ToOpen {
                let mid = sdp.add_media(
                    track.track_in.kind,
                    str0m::media::Direction::SendOnly,
                    Some(track_id.origin_participant.to_string()),
                    Some(track_id.to_string()),
                    None,
                );
                track.state = TrackOutState::Negotiating(mid);
            }
        }

        if !sdp.has_changes() {
            return false;
        }

        let Some((offer, pending)) = sdp.apply() else {
            return false;
        };

        self.pending.replace(pending);
        self.send_server_event(server::Message::Offer(offer.to_sdp_string()));

        true
    }
}

#[derive(Clone, Debug)]
pub struct ParticipantHandle {
    pub sender: mpsc::Sender<ParticipantMessage>,
    pub participant_id: Arc<ParticipantId>,
}

impl ParticipantHandle {
    pub fn new(
        source: UdpSourceHandle,
        sink: UdpSinkHandle,
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
            sink,
            room,
            participant_id,
            rtc,
            published_tracks: HashMap::new(),
            subscribed_tracks: HashMap::new(),
            cid: None,
            pending: None,
        };
        (handle, actor)
    }

    pub fn forward(&self, msg: message::UDPPacket) -> Result<(), TrySendError<ParticipantMessage>> {
        self.sender.try_send(ParticipantMessage::UdpPacket(msg))
    }
}

impl Display for ParticipantHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.participant_id.deref().as_ref())
    }
}
