use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc, time::Duration};

use bytes::Bytes;
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    channel::{ChannelData, ChannelId},
    error::SdpError,
    media::{Direction, KeyframeRequest, MediaAdded, MediaData, MediaKind, Mid, Simulcast},
    net::{self, Transmit},
};
use tokio::{
    sync::mpsc::{
        self,
        error::{SendError, TrySendError},
    },
    time::Instant,
};

use crate::{
    entity::{ParticipantId, TrackId},
    message::{self, EgressUDPPacket, TrackIn},
    proto,
    rng::Rng,
    room::RoomHandle,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
    track::TrackHandle,
};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::rpc";

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
pub enum ParticipantControlMessage {
    NewTrack(TrackHandle),
}

#[derive(Debug)]
pub enum ParticipantDataMessage {
    UdpPacket(message::UDPPacket),
    ForwardMedia(Arc<TrackIn>, Arc<MediaData>),
    KeyframeRequest(Arc<TrackId>, message::KeyframeRequest),
}

#[derive(Debug)]
struct TrackOut {
    handle: TrackHandle,
    mid: Option<Mid>,
}

struct MidOutSlot {
    kind: MediaKind,
    simulcast: Option<Simulcast>,
    track_id: Option<Arc<TrackId>>,
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
    rng: Rng,
    handle: ParticipantHandle,
    data_receiver: mpsc::Receiver<ParticipantDataMessage>,
    control_receiver: mpsc::Receiver<ParticipantControlMessage>,
    sink: UdpSinkHandle,
    room: RoomHandle,
    participant_id: Arc<ParticipantId>,
    rtc: str0m::Rtc,
    cid: Option<ChannelId>,

    published_tracks: HashMap<Mid, TrackHandle>,
    subscribed_tracks: HashMap<Arc<TrackId>, TrackOut>,
    mid_out_slots: HashMap<Mid, MidOutSlot>,
}

impl ParticipantActor {
    #[tracing::instrument(
        skip(self),
        fields(participant_id=?self.participant_id)
    )]
    pub async fn run(mut self) {
        // TODO: notify ingress to add self to the routing table
        // WARN: be careful with spending too much time in this loop.
        // We should yield back to the scheduler based on some heuristic here.

        tracing::info!("created");
        loop {
            let delay = if let Some(delay) = self.poll().await {
                delay
            } else {
                // Rtc timeout
                break;
            };

            tokio::select! {
                Some(msg) = self.data_receiver.recv() => {
                    self.handle_data_message(msg).await;
                }

                msg = self.control_receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_control_message(msg).await,
                        None => break,
                    }
                }

                _ = tokio::time::sleep(delay) => {
                    // explicit empty, next loop polls again
                    // tracing::warn!("woke up from sleep: {}us", delay.as_micros());
                }

                else => break,
            }
        }

        // TODO: cleanup in the room
        tracing::info!("exited");
    }

    #[inline]
    async fn handle_data_message(&mut self, msg: ParticipantDataMessage) {
        match msg {
            ParticipantDataMessage::UdpPacket(packet) => {
                let now = Instant::now();
                let res = self.rtc.handle_input(Input::Receive(
                    now.into_std(),
                    net::Receive {
                        proto: net::Protocol::Udp,
                        source: packet.src,
                        destination: packet.dst,
                        contents: (&*packet.raw).try_into().unwrap(),
                    },
                ));

                if let Err(err) = res {
                    tracing::warn!("dropped a UDP packet: {err}");
                }
            }
            ParticipantDataMessage::ForwardMedia(track, data) => {
                self.handle_forward_media(track, data);
            }

            ParticipantDataMessage::KeyframeRequest(track_id, req) => {
                let Some(mut writer) = self.rtc.writer(track_id.origin_mid) else {
                    tracing::warn!(mid=?track_id.origin_mid, "mid is not found for regenerating a keyframe");
                    return;
                };

                if let Err(err) = writer.request_keyframe(req.rid, req.kind) {
                    tracing::warn!("failed to request a keyframe from the publisher: {err}");
                }
            }
        }
    }

    async fn handle_control_message(&mut self, msg: ParticipantControlMessage) {
        match msg {
            ParticipantControlMessage::NewTrack(track) => {
                if track.meta.id.origin_participant == self.participant_id {
                    // successfully publish a track
                    tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "published track");
                    self.published_tracks
                        .insert(track.meta.id.origin_mid, track);
                } else {
                    // new tracks from other participants
                    tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "subscribed track");
                    let track_id = track.meta.id.clone();

                    self.subscribed_tracks.insert(
                        track_id,
                        TrackOut {
                            handle: track,
                            mid: None,
                        },
                    );
                    self.reconfigure_downstreams().await;
                }
            }
        }
    }

    async fn poll(&mut self) -> Option<Duration> {
        while self.rtc.is_alive() {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            match self.rtc.poll_output().unwrap() {
                // Stop polling when we get the timeout.
                Output::Timeout(deadline) => {
                    // WARN: be careful in mixing tokio vs std Instant. str0m expects std Instant
                    // Every conversion can be lossy and can create a spin loop here if not
                    // precise.
                    let now = Instant::now().into_std();
                    let duration = deadline - now;
                    if !duration.is_zero() {
                        return Some(duration);
                    }

                    // forward clock never fails
                    self.rtc.handle_input(Input::Timeout(now)).unwrap();
                }

                // Transmit this data to the remote peer. Typically via
                // a UDP socket. The destination IP comes from the ICE
                // agent. It might change during the session.
                Output::Transmit(v) => {
                    self.handle_output_transmit(v);
                }

                // Events are mainly incoming media data from the remote
                // peer, but also data channel data and statistics.
                Output::Event(v) => {
                    self.handle_output_event(v).await;
                }
            }
        }

        None
    }

    fn send_server_event(&mut self, msg: proto::sfu::server_message::Payload) {
        // TODO: handle when data channel is closed

        if let Some(mut ch) = self.cid.and_then(|cid| self.rtc.channel(cid)) {
            let encoded = proto::sfu::ServerMessage { payload: Some(msg) }.encode_to_vec();
            ch.write(true, encoded.as_slice());
        }
    }

    fn handle_rpc(&mut self, data: ChannelData) -> Result<(), ParticipantError> {
        let msg = proto::sfu::ClientMessage::decode(data.data.as_slice())
            .map_err(ParticipantError::InvalidRPCFormat)?;

        match msg.payload {
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
            Event::IceConnectionStateChange(ice_state) => match ice_state {
                str0m::IceConnectionState::Disconnected => self.rtc.disconnect(),
                state => tracing::trace!("ice state: {:?}", state),
            },
            Event::MediaAdded(e) => {
                self.handle_new_media(e).await;
            }
            Event::ChannelOpen(cid, label) => {
                if label == DATA_CHANNEL_LABEL {
                    self.cid = Some(cid);
                    tracing::warn!(label, "data channel is open");
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
                    tracing::warn!("channel closed: {:?}", cid);
                }
            }
            Event::MediaData(e) => {
                if let Some(track) = self.published_tracks.get(&e.mid) {
                    track.forward_media(Arc::new(e));
                }
            }
            Event::KeyframeRequest(req) => self.handle_keyframe_request(req),
            Event::Connected => {
                tracing::info!("connected");
            }
            event => tracing::warn!("unhandled output event: {:?}", event),
        }
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        let Some(MidOutSlot {
            track_id: Some(track_id),
            ..
        }) = self.mid_out_slots.get(&req.mid)
        else {
            return;
        };

        let Some(track) = self.subscribed_tracks.get(track_id) else {
            return;
        };

        track.handle.request_keyframe(req.into());
    }

    async fn handle_new_media(&mut self, media: MediaAdded) {
        match media.direction {
            // client -> SFU
            Direction::RecvOnly => {
                tracing::info!(?media, "handle_new_media from client");
                // TODO: handle back pressure by buffering temporarily
                let track_id = TrackId::new(&mut self.rng, self.participant_id.clone(), media.mid);
                let track_id = Arc::new(track_id);
                let track = TrackIn {
                    id: track_id,
                    kind: media.kind,
                    simulcast: media.simulcast,
                };
                if let Err(err) = self.room.publish(track).await {
                    // this participant should get cleaned up by the supervisor
                    tracing::warn!("failed to publish track to room: {err}");
                }
            }
            // SFU -> client
            Direction::SendOnly => {
                tracing::info!(?media, "handle_new_media from other participant");
                self.mid_out_slots.insert(
                    media.mid,
                    MidOutSlot {
                        kind: media.kind,
                        simulcast: media.simulcast,
                        track_id: None,
                    },
                );

                self.reconfigure_downstreams().await;
            }
            dir => {
                tracing::warn!("{dir} transceiver is unsupported, shutdown misbehaving client");
                self.rtc.disconnect();
                return;
            }
        }
    }

    fn handle_forward_media(&mut self, track: Arc<TrackIn>, data: Arc<MediaData>) {
        tracing::debug!("handle forward media data");
        let Some(track) = self.subscribed_tracks.get(&track.id) else {
            return;
        };

        let Some(mid) = track.mid else {
            return;
        };

        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };

        // WebRTC Clients might use different PT for the same codec, e.g. Firefox vs Chrome
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(err) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            tracing::error!("failed to write media: {}", err);
            self.rtc.disconnect();
        }
    }

    async fn reconfigure_downstreams(&mut self) {
        for (mid, mid_slot) in &mut self.mid_out_slots {
            if mid_slot.track_id.is_some() {
                continue;
            }

            for (track_id, track) in &mut self.subscribed_tracks {
                if track.mid.is_some() {
                    continue;
                }

                if track.handle.meta.kind == mid_slot.kind {
                    let Ok(_) = track.handle.subscribe(self.handle.clone()).await else {
                        continue;
                    };
                    track.mid = Some(*mid);
                    mid_slot.track_id = Some(track_id.clone());
                    break;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParticipantHandle {
    pub data_sender: mpsc::Sender<ParticipantDataMessage>,
    pub control_sender: mpsc::Sender<ParticipantControlMessage>,
    pub participant_id: Arc<ParticipantId>,
}

impl ParticipantHandle {
    pub fn new(
        rng: Rng,
        source: UdpSourceHandle,
        sink: UdpSinkHandle,
        room: RoomHandle,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> (Self, ParticipantActor) {
        let (data_sender, data_receiver) = mpsc::channel(128);
        let (control_sender, control_receiver) = mpsc::channel(1);
        let handle = Self {
            data_sender,
            control_sender,
            participant_id: participant_id.clone(),
        };
        let actor = ParticipantActor {
            rng,
            handle: handle.clone(),
            data_receiver,
            control_receiver,
            sink,
            room,
            participant_id,
            rtc,
            published_tracks: HashMap::new(),
            subscribed_tracks: HashMap::new(),
            mid_out_slots: HashMap::new(),
            cid: None,
        };
        (handle, actor)
    }

    pub fn forward(
        &self,
        msg: message::UDPPacket,
    ) -> Result<(), TrySendError<ParticipantDataMessage>> {
        let res = self
            .data_sender
            .try_send(ParticipantDataMessage::UdpPacket(msg));

        if let Err(err) = &res {
            tracing::warn!("raw packet is dropped: {err}");
        }
        res
    }

    pub async fn new_track(
        &self,
        track: TrackHandle,
    ) -> Result<(), SendError<ParticipantControlMessage>> {
        self.control_sender
            .send(ParticipantControlMessage::NewTrack(track))
            .await
    }

    pub fn forward_media(
        &self,
        track: Arc<TrackIn>,
        data: Arc<MediaData>,
    ) -> Result<(), TrySendError<ParticipantDataMessage>> {
        let res = self
            .data_sender
            .try_send(ParticipantDataMessage::ForwardMedia(track, data));

        if let Err(err) = &res {
            tracing::warn!("media packet is dropped: {err}");
        }

        res
    }

    pub fn request_keyframe(&self, track_id: Arc<TrackId>, req: message::KeyframeRequest) {
        if let Err(err) = self
            .data_sender
            .try_send(ParticipantDataMessage::KeyframeRequest(track_id, req))
        {
            tracing::warn!("keyframe request is dropped by the participant actor: {err}");
        }
    }
}

impl Display for ParticipantHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.participant_id.deref().as_ref())
    }
}
