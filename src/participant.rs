use std::{
    collections::HashMap,
    fmt::{self, Display},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

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
    task::JoinSet,
    time::Instant,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    entity::{EntityId, ParticipantId, TrackId},
    message::{self, EgressUDPPacket, TrackIn},
    proto::sfu,
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
    TracksAdded(Arc<Vec<TrackHandle>>),
    TracksRemoved(Arc<Vec<Arc<TrackId>>>),
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
    source: UdpSourceHandle,
    data_receiver: mpsc::Receiver<ParticipantDataMessage>,
    control_receiver: mpsc::Receiver<ParticipantControlMessage>,
    sink: UdpSinkHandle,
    room: RoomHandle,
    participant_id: Arc<ParticipantId>,
    rtc: str0m::Rtc,
    cid: Option<ChannelId>,
    track_tasks: JoinSet<Arc<TrackId>>,

    published_tracks: HashMap<Mid, TrackHandle>,
    // InternalTrackId -> TrackOut
    available_tracks: HashMap<Arc<EntityId>, TrackOut>,
    mid_out_slots: HashMap<Mid, MidOutSlot>,
}

impl fmt::Debug for ParticipantActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParticipantActor")
            .field("participant_id", &self.participant_id)
            .finish()
    }
}

impl Actor for ParticipantActor {
    type ID = Arc<ParticipantId>;

    fn kind(&self) -> &'static str {
        "participant"
    }

    fn id(&self) -> Self::ID {
        self.participant_id.clone()
    }

    async fn pre_start(&mut self) -> Result<(), crate::actor::ActorError> {
        let ufrag = self.rtc.direct_api().local_ice_credentials().ufrag;
        self.source
            .add_participant(ufrag, self.handle.clone())
            .await
            .map_err(|_| ActorError::PreStartFailed("source is closed".to_string()))
    }

    async fn run(&mut self) -> Result<(), crate::actor::ActorError> {
        // TODO: notify ingress to add self to the routing table
        // WARN: be careful with spending too much time in this loop.
        // We should yield back to the scheduler based on some heuristic here.

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

                Some(Ok(key)) = self.track_tasks.join_next() => {
                    // TODO: clean up track
                }

                else => break,
            }
        }

        Ok(())
    }

    async fn post_stop(&mut self) -> Result<(), ActorError> {
        let ufrag = self.rtc.direct_api().local_ice_credentials().ufrag;
        self.source
            .remove_participant(ufrag)
            .await
            .map_err(|_| ActorError::PostStopFailed("source is closed".to_string()))
    }
}

impl ParticipantActor {
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
        use sfu::server_message::Payload;

        match msg {
            ParticipantControlMessage::TracksAdded(tracks) => {
                let mut should_reconfigure: bool = false;
                let mut new_tracks = Vec::new();

                for track in tracks.iter() {
                    if track.meta.id.origin_participant == self.participant_id {
                        // successfully publish a track
                        tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "published track");
                        self.published_tracks
                            .insert(track.meta.id.origin_mid, track.clone());
                    } else {
                        // new tracks from other participants
                        tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "subscribed track");
                        let track_id = track.meta.id.clone();

                        self.available_tracks.insert(
                            track_id.internal.clone(),
                            TrackOut {
                                handle: track.clone(),
                                mid: None,
                            },
                        );
                        let kind = if track.meta.kind.is_video() {
                            sfu::TrackKind::Video
                        } else {
                            sfu::TrackKind::Audio
                        };

                        new_tracks.push(sfu::TrackInfo {
                            track_id: track.meta.id.to_string(),
                            kind: kind as i32,
                            participant_id: track.meta.id.origin_participant.to_string(),
                        });
                        should_reconfigure = true;
                    }
                }

                if should_reconfigure {
                    self.send_server_event(Payload::TrackPublished(sfu::TrackPublishedPayload {
                        remote_tracks: new_tracks,
                    }));
                    self.reconfigure_downstreams().await;
                }
            }
            ParticipantControlMessage::TracksRemoved(track_ids) => {
                for track_id in track_ids.iter() {
                    let Some(track) = self.available_tracks.remove(&track_id.internal) else {
                        return;
                    };

                    let Some(mid) = track.mid else {
                        return;
                    };

                    // We don't reconfigure downstreams here because the client will
                    // likely rearrange their layout and subscribe for new streams.
                    self.mid_out_slots.remove(&mid);
                }
                self.send_server_event(Payload::TrackUnpublished(sfu::TrackUnpublishedPayload {
                    remote_track_ids: track_ids.iter().map(|t| t.to_string()).collect(),
                }));
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
                    self.handle_output_transmit(v).await;
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

    fn send_server_event(&mut self, msg: sfu::server_message::Payload) {
        // TODO: handle when data channel is closed

        if let Some(mut ch) = self.cid.and_then(|cid| self.rtc.channel(cid)) {
            let encoded = sfu::ServerMessage { payload: Some(msg) }.encode_to_vec();
            if let Err(err) = ch.write(true, encoded.as_slice()) {
                tracing::warn!("failed to send rpc via data channel: {err}");
            }
        }
    }

    async fn handle_rpc(&mut self, data: ChannelData) -> Result<(), ParticipantError> {
        let msg = sfu::ClientMessage::decode(data.data.as_slice())
            .map_err(ParticipantError::InvalidRPCFormat)?;

        let Some(payload) = msg.payload else {
            return Ok(());
        };

        match payload {
            sfu::client_message::Payload::Subscribe(subscribe) => {
                let mid = Mid::from(subscribe.mid.as_str());
                if let Some(track) = self.available_tracks.get_mut(&subscribe.remote_track_id) {
                    if let Some(slot) = self.mid_out_slots.get_mut(&mid) {
                        track.mid.replace(mid);
                        if let Some(last_track) =
                            slot.track_id.replace(track.handle.meta.id.clone())
                        {
                            self.handle_unsubscribe(last_track).await;
                        }
                    }
                }
            }
            sfu::client_message::Payload::Unsubscribe(unsubscribe) => {
                let mid = Mid::from(unsubscribe.mid.as_str());
                if let Some(slot) = self.mid_out_slots.get_mut(&mid) {
                    if let Some(last_track) = slot.track_id.take() {
                        self.handle_unsubscribe(last_track).await;
                    }
                }
            }
        };

        Ok(())
    }

    async fn handle_unsubscribe(&mut self, track_id: Arc<TrackId>) {
        // TODO: handle unsubscribe
        // let Some(track) = self.available_tracks.get(&track_id.internal) else {
        //     return;
        // };
        // track.handle.subscribe(participant)
    }

    async fn handle_output_transmit(&mut self, t: Transmit) {
        let packet = Bytes::copy_from_slice(&t.contents);
        let _ = self
            .sink
            .send(EgressUDPPacket {
                raw: packet,
                dst: t.destination,
            })
            .await;
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
                    if let Err(err) = self.handle_rpc(data).await {
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
                    let _ = track.forward_media(Arc::new(e)).await;
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

        let Some(track) = self.available_tracks.get(&track_id.internal) else {
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
                    id: track_id.clone(),
                    kind: media.kind,
                    simulcast: media.simulcast,
                };

                let (handle, actor) = TrackHandle::new(self.handle.clone(), Arc::new(track));
                self.track_tasks.spawn(
                    async move {
                        actor::run(actor).await;
                        track_id
                    }
                    .in_current_span(),
                );
                if let Err(err) = self.room.publish(handle).await {
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
        let Some(track) = self.available_tracks.get(&track.id.internal) else {
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

            for (_, track) in &mut self.available_tracks {
                if track.mid.is_some() {
                    continue;
                }

                if track.handle.meta.kind == mid_slot.kind {
                    let Ok(_) = track.handle.subscribe(self.handle.clone()).await else {
                        continue;
                    };
                    track.mid = Some(*mid);
                    mid_slot.track_id = Some(track.handle.meta.id.clone());
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
        let (control_sender, control_receiver) = mpsc::channel(8);
        let handle = Self {
            data_sender,
            control_sender,
            participant_id: participant_id.clone(),
        };
        let actor = ParticipantActor {
            rng,
            source,
            handle: handle.clone(),
            data_receiver,
            control_receiver,
            sink,
            room,
            participant_id,
            rtc,
            track_tasks: JoinSet::new(),
            published_tracks: HashMap::new(),
            available_tracks: HashMap::new(),
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

    pub async fn add_tracks(
        &self,
        tracks: Arc<Vec<TrackHandle>>,
    ) -> Result<(), SendError<ParticipantControlMessage>> {
        self.control_sender
            .send(ParticipantControlMessage::TracksAdded(tracks))
            .await
    }

    pub async fn remove_tracks(
        &self,
        track_ids: Arc<Vec<Arc<TrackId>>>,
    ) -> Result<(), SendError<ParticipantControlMessage>> {
        self.control_sender
            .send(ParticipantControlMessage::TracksRemoved(track_ids))
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
