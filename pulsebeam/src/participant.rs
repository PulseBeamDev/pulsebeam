use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    channel::{ChannelData, ChannelId},
    error::SdpError,
    media::{Direction, KeyframeRequest, MediaAdded, MediaData, MediaKind, Mid, Simulcast},
    net::{self, Transmit},
};
use tokio::time::Instant;

use crate::{
    entity::{EntityId, ParticipantId, TrackId},
    message::{self, EgressUDPPacket, TrackMeta},
    proto::{self, sfu},
    room, sink, system, track,
};
use pulsebeam_runtime::actor;

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
    TracksSnapshot(HashMap<Arc<TrackId>, track::TrackHandle>),
    TracksPublished(Arc<HashMap<Arc<TrackId>, track::TrackHandle>>),
    TracksUnpublished(Arc<HashMap<Arc<TrackId>, track::TrackHandle>>),
    TrackPublishRejected(track::TrackHandle),
}

#[derive(Debug)]
pub enum ParticipantDataMessage {
    UdpPacket(message::UDPPacket),
    ForwardMedia(Arc<TrackMeta>, Arc<MediaData>),
    KeyframeRequest(Arc<TrackId>, message::KeyframeRequest),
}

struct TrackOut {
    track: track::TrackHandle,
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
    // Dependencies
    system_ctx: system::SystemContext,
    room_handle: room::RoomHandle,

    // Engine
    rtc: Box<str0m::Rtc>,
    cid: Option<ChannelId>,

    // Metadata
    participant_id: Arc<ParticipantId>,

    // Current local state
    published_video_tracks: HashMap<Mid, track::TrackHandle>,
    published_audio_tracks: HashMap<Mid, track::TrackHandle>,
    subscribed_video_tracks: HashMap<Mid, MidOutSlot>,
    subscribed_audio_tracks: HashMap<Mid, MidOutSlot>,
    initialized: bool,

    // Global view of available tracks. This participant view is mirrored
    // 1:1 to the client. Thus, it's possible to be slightly different than the
    // room's global view of avalable tracks.
    //
    // This participant may also only have access to a segmented view of the available tracks
    // due to a lack of permission.
    //
    // InternalTrackId -> TrackOut
    available_video_tracks: HashMap<Arc<EntityId>, TrackOut>,
    available_audio_tracks: HashMap<Arc<EntityId>, TrackOut>,

    // State sync with client
    //
    // TODO: merge available_tracks and pending_published_tracks. Participant actor
    // should not hold all published tracks in the room, only Room actor and the client
    // will have the full list.
    pending_published_tracks: Vec<proto::sfu::TrackInfo>,
    pending_unpublished_tracks: Vec<EntityId>,
    pending_switched_tracks: Vec<proto::sfu::TrackSwitchInfo>,
    should_resync: bool,
}

impl actor::Actor for ParticipantActor {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<ParticipantId>;
    type ObservableState = ();

    fn meta(&self) -> Self::Meta {
        self.participant_id.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        // TODO: notify ingress to add self to the routing table
        // WARN: be careful with spending too much time in this loop.
        // We should yield back to the scheduler based on some heuristic here.

        loop {
            let delay = if let Some(delay) = self.poll(ctx).await {
                delay
            } else {
                // Rtc timeout
                break;
            };

            tokio::select! {
                biased;
                msg = ctx.hi_rx.recv() => {
                    match msg {
                        Some(msg) => self.on_high_priority(ctx, msg).await,
                        None => break,
                    }
                }

                Some(msg) = ctx.lo_rx.recv() => {
                    self.on_low_priority(ctx, msg).await;
                }

                _ = tokio::time::sleep(delay) => {
                    // explicit empty, next loop polls again
                    // tracing::warn!("woke up from sleep: {}us", delay.as_micros());
                }

                else => break,
            }
        }

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        use sfu::server_message::Payload;

        self.should_resync = true;
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.handle_published_tracks(ctx, &tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.handle_published_tracks(ctx, tracks.as_ref());
            }
            ParticipantControlMessage::TracksUnpublished(track_ids) => {
                for track_id in track_ids.keys() {
                    let Some(track) = self.available_video_tracks.remove(&track_id.internal) else {
                        return;
                    };

                    let Some(mid) = track.mid else {
                        return;
                    };

                    match track.track.meta.kind {
                        MediaKind::Video => self.subscribed_video_tracks.remove(&mid),
                        MediaKind::Audio => self.subscribed_audio_tracks.remove(&mid),
                    };

                    self.pending_unpublished_tracks.push(track_id.to_string());
                }
            }
            ParticipantControlMessage::TrackPublishRejected(track_handle) => {
                // TODO: notify rejection to client
            }
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
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
}

impl ParticipantActor {
    async fn init_subscriptions(&mut self, ctx: &mut actor::ActorContext<Self>) {
        // auto subscribe
        let mut subscribed_tracks_iter = self.subscribed_video_tracks.iter_mut();
        let mut available_tracks_iter = self.available_video_tracks.iter();

        while let (Some(sub), Some(available)) =
            (subscribed_tracks_iter.next(), available_tracks_iter.next())
        {
            let meta = &available.1.track.meta;
            sub.1.track_id.replace(meta.id.clone());
            available
                .1
                .track
                .send_high(track::TrackControlMessage::Subscribe(ctx.handle.clone()))
                .await;
            tracing::info!("replaced track");
        }
        self.initialized = true
    }

    async fn poll(&mut self, ctx: &mut actor::ActorContext<Self>) -> Option<Duration> {
        while self.rtc.is_alive() {
            if self.should_resync {
                self.resync();
            }

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
                    self.handle_output_event(ctx, v).await;
                }
            }
        }

        None
    }

    fn resync(&mut self) {
        // resync all pending states with client
        // TODO: check pending states and resync

        self.should_resync = false;
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

                // TODO: handle subscribe
            }
            sfu::client_message::Payload::Unsubscribe(unsubscribe) => {
                let mid = Mid::from(unsubscribe.mid.as_str());

                // TODO: handle unsubscribe
            }
        };

        Ok(())
    }

    fn handle_published_tracks(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        tracks: &HashMap<Arc<TrackId>, track::TrackHandle>,
    ) {
        for track in tracks.values() {
            if track.meta.id.origin_participant == self.participant_id {
                // don't include to pending published tracks to prevent loopback on client
                // successfully publish a track
                tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "published track");

                match track.meta.kind {
                    MediaKind::Video => self
                        .published_video_tracks
                        .insert(track.meta.id.origin_mid, track.clone()),
                    MediaKind::Audio => self
                        .published_audio_tracks
                        .insert(track.meta.id.origin_mid, track.clone()),
                };
            } else {
                // new tracks from other participants
                tracing::info!(track_id = ?track.meta.id, origin = ?track.meta.id.origin_participant, "subscribed track");
                let track_id = track.meta.id.clone();

                self.available_video_tracks.insert(
                    track_id.internal.clone(),
                    TrackOut {
                        track: track.clone(),
                        mid: None,
                    },
                );
                let kind = if track.meta.kind.is_video() {
                    sfu::TrackKind::Video
                } else {
                    sfu::TrackKind::Audio
                };

                self.pending_published_tracks.push(sfu::TrackInfo {
                    track_id: track.meta.id.to_string(),
                    kind: kind as i32,
                    participant_id: track.meta.id.origin_participant.to_string(),
                });
            }
        }
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
            .system_ctx
            .sink_handle
            .send_low(sink::SinkMessage::Packet(EgressUDPPacket {
                raw: packet,
                dst: t.destination,
            }))
            .await;
    }

    async fn handle_output_event(&mut self, ctx: &mut actor::ActorContext<Self>, event: Event) {
        match event {
            // Abort if we disconnect.
            Event::IceConnectionStateChange(ice_state) => match ice_state {
                str0m::IceConnectionState::Disconnected => self.rtc.disconnect(),
                state => tracing::trace!("ice state: {:?}", state),
            },
            Event::MediaAdded(e) => {
                self.handle_new_media(ctx, e).await;
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
                if let Some(track) = self.published_video_tracks.get(&e.mid) {
                    let _ = track
                        .send_low(track::TrackDataMessage::ForwardMedia(Arc::new(e)))
                        .await;
                } else if let Some(track) = self.published_audio_tracks.get(&e.mid) {
                    let _ = track
                        .send_low(track::TrackDataMessage::ForwardMedia(Arc::new(e)))
                        .await;
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
        }) = self.subscribed_video_tracks.get(&req.mid)
        else {
            return;
        };

        let Some(track) = self.available_video_tracks.get(&track_id.internal) else {
            return;
        };

        let _ = track
            .track
            .try_send_low(track::TrackDataMessage::KeyframeRequest(req.into()));
    }

    async fn handle_new_media(&mut self, ctx: &mut actor::ActorContext<Self>, media: MediaAdded) {
        match media.direction {
            // client -> SFU
            Direction::RecvOnly => {
                tracing::info!(?media, "handle_new_media from client");
                // TODO: handle back pressure by buffering temporarily
                let track_id = TrackId::new(self.participant_id.clone(), media.mid);
                let track_id = Arc::new(track_id);
                let track = TrackMeta {
                    id: track_id.clone(),
                    kind: media.kind,
                    // TODO: double check the simulcast directions.
                    simulcast_rids: media.simulcast.map(|s| s.recv),
                };

                if let Err(err) = self
                    .room_handle
                    .send_high(room::RoomMessage::PublishTrack(Arc::new(track)))
                    .await
                {
                    // this participant should get cleaned up by the supervisor
                    tracing::warn!("failed to publish track to room: {err}");
                }
            }
            // SFU -> client
            Direction::SendOnly => {
                tracing::info!(?media, "handle_new_media from other participant");
                match media.kind {
                    MediaKind::Video => self.subscribed_video_tracks.insert(
                        media.mid,
                        MidOutSlot {
                            kind: media.kind,
                            simulcast: media.simulcast,
                            track_id: None,
                        },
                    ),
                    MediaKind::Audio => self.subscribed_audio_tracks.insert(
                        media.mid,
                        MidOutSlot {
                            kind: media.kind,
                            simulcast: media.simulcast,
                            track_id: None,
                        },
                    ),
                };

                // self.reconfigure_downstreams().await;
            }
            dir => {
                tracing::warn!("{dir} transceiver is unsupported, shutdown misbehaving client");
                self.rtc.disconnect();
            }
        }
    }

    fn handle_forward_media(&mut self, track: Arc<TrackMeta>, data: Arc<MediaData>) {
        let Some(track) = self.available_video_tracks.get(&track.id.internal) else {
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
}

impl ParticipantActor {
    pub fn new(
        system_ctx: system::SystemContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<ParticipantId>,
        rtc: Box<Rtc>,
    ) -> Self {
        Self {
            system_ctx,
            room_handle,
            participant_id,
            rtc,

            initialized: false,
            published_video_tracks: HashMap::new(),
            published_audio_tracks: HashMap::new(),
            available_video_tracks: HashMap::new(),
            available_audio_tracks: HashMap::new(),
            subscribed_video_tracks: HashMap::new(),
            subscribed_audio_tracks: HashMap::new(),
            cid: None,

            pending_published_tracks: Vec::new(),
            pending_unpublished_tracks: Vec::new(),
            pending_switched_tracks: Vec::new(),
            should_resync: false,
        }
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantActor>;
