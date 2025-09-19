use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::{StreamExt, stream::FuturesUnordered};
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    channel::{ChannelData, ChannelId},
    error::SdpError,
    media::{Direction, KeyframeRequest, MediaAdded, MediaData, MediaKind, Mid},
    net::Transmit,
};
use tokio::time::Instant;

use crate::{audio_selector::AudioSelector, entity, gateway, message, proto, room, system, track};
use pulsebeam_runtime::{actor, net};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::rpc";

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
    #[error("Invalid RPC format: {0}")]
    InvalidRpcFormat(#[from] DecodeError),
}

#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<Arc<entity::TrackId>, track::TrackHandle>),
    TracksPublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackHandle>>),
    TracksUnpublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackHandle>>),
    TrackPublishRejected(track::TrackHandle),
}

#[derive(Debug)]
pub enum ParticipantDataMessage {
    UdpPacket(net::RecvPacket),
    ForwardMedia(Arc<message::TrackMeta>, Arc<MediaData>),
    KeyframeRequest(Arc<entity::TrackId>, message::KeyframeRequest),
}

#[derive(Clone)]
struct TrackOut {
    handle: track::TrackHandle,
    mid: Option<Mid>,
}

struct MidSlot {
    track_id: Option<Arc<entity::TrackId>>,
}

/// Manages WebRTC participant connections and media routing
///
/// Core responsibilities:
/// - WebRTC signaling and peer connection management
/// - Media routing between client and room
/// - Video subscription management and layer selection
/// - Audio filtering using AudioSelector (subscribes to all, forwards selectively)
/// - Bandwidth control and congestion management
pub struct ParticipantActor {
    // Core dependencies
    system_ctx: system::SystemContext,
    room_handle: room::RoomHandle,
    rtc: Rtc,

    // Identity
    participant_id: Arc<entity::ParticipantId>,
    data_channel: Option<ChannelId>,

    // Track management
    published_tracks: PublishedTracks,
    available_tracks: AvailableTracks,
    subscribed_slots: SubscribedSlots,
    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackActor>>,

    // Audio filtering - subscribes to all audio but filters forwarding
    audio_selector: AudioSelector,

    // Client sync state
    sync_state: ClientSyncState,
}

#[derive(Default)]
struct PublishedTracks {
    video: HashMap<Mid, track::TrackHandle>,
    audio: HashMap<Mid, track::TrackHandle>,
}

#[derive(Default)]
struct AvailableTracks {
    video: HashMap<Arc<entity::EntityId>, TrackOut>,
    // Audio tracks are always subscribed to, so we track them separately
    audio: HashMap<Arc<entity::EntityId>, TrackOut>,
}

#[derive(Default)]
struct SubscribedSlots {
    video: HashMap<Mid, MidSlot>,
    audio: HashMap<Mid, MidSlot>,
}

#[derive(Default)]
struct ClientSyncState {
    pending_published: Vec<proto::sfu::TrackInfo>,
    pending_unpublished: Vec<entity::EntityId>,
    pending_switched: Vec<proto::sfu::TrackSwitchInfo>,
    needs_resync: bool,
}

impl ParticipantActor {
    pub fn new(
        system_ctx: system::SystemContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        Self {
            system_ctx,
            room_handle,
            participant_id,
            rtc,
            data_channel: None,
            published_tracks: PublishedTracks::default(),
            available_tracks: AvailableTracks::default(),
            subscribed_slots: SubscribedSlots::default(),
            track_tasks: FuturesUnordered::new(),
            audio_selector: AudioSelector::with_chromium_limit(),
            sync_state: ClientSyncState::default(),
        }
    }

    /// Main event loop with proper timeout handling
    async fn poll(&mut self, ctx: &mut actor::ActorContext<Self>) -> Option<Duration> {
        while self.rtc.is_alive() {
            if self.sync_state.needs_resync {
                self.resync_with_client(ctx).await;
            }

            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = Instant::now().into_std();
                    let duration = deadline.saturating_duration_since(now);

                    if duration.is_zero() {
                        // Handle timeout immediately
                        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
                            tracing::error!("Failed to handle timeout: {}", e);
                            break;
                        }
                        continue;
                    }

                    return Some(duration);
                }
                Ok(Output::Transmit(transmit)) => {
                    self.handle_transmit(transmit).await;
                }
                Ok(Output::Event(event)) => {
                    self.handle_event(ctx, event).await;
                }
                Err(e) => {
                    tracing::error!("RTC poll error: {}", e);
                    break;
                }
            }
        }
        None
    }

    async fn resync_with_client(&mut self, ctx: &mut actor::ActorContext<Self>) {
        // TODO: Send pending state changes to client via data channel
        self.auto_subscribe_tracks(ctx).await;
        self.sync_state.needs_resync = false;
    }

    /// Automatically subscribe available tracks to open slots
    /// Audio: Always subscribe to all audio tracks (filtering happens later)
    /// Video: Subscribe based on available slots
    async fn auto_subscribe_tracks(&mut self, ctx: &mut actor::ActorContext<Self>) {
        // TODO: audio

        let mut available_tracks = self.available_tracks.video.iter_mut();
        for (slot_id, slot) in self.subscribed_slots.video.iter_mut() {
            if slot.track_id.is_some() {
                continue;
            }

            for (track_id, track) in &mut available_tracks {
                if track.mid.is_some() {
                    continue;
                }

                if track
                    .handle
                    .send_high(track::TrackControlMessage::Subscribe(ctx.handle.clone()))
                    .await
                    .is_err()
                {
                    continue;
                }

                track.mid.replace(*slot_id);
                let meta = &track.handle.meta;
                slot.track_id.replace(meta.id.clone());
                tracing::info!("allocated video slot: {track_id} -> {slot_id}");
                break;
            }
        }
    }

    fn handle_track_finished(&mut self, track_meta: Arc<message::TrackMeta>) {
        let tracks = match track_meta.kind {
            MediaKind::Video => &mut self.published_tracks.video,
            MediaKind::Audio => &mut self.published_tracks.audio,
        };

        tracks.remove(&track_meta.id.origin_mid);
        tracing::info!("Track finished: {}", track_meta.id);
    }

    fn handle_published_tracks(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        tracks: &HashMap<Arc<entity::TrackId>, track::TrackHandle>,
    ) {
        for track_handle in tracks.values() {
            let track_meta = &track_handle.meta;

            if track_meta.id.origin_participant == self.participant_id {
                // Our own track - add to published
                self.add_published_track(track_handle);
            } else {
                // Track from another participant - add to available
                self.add_available_track(track_handle);
            }
        }
    }

    fn add_published_track(&mut self, track_handle: &track::TrackHandle) {
        let track_meta = &track_handle.meta;

        let tracks = match track_meta.kind {
            MediaKind::Video => &mut self.published_tracks.video,
            MediaKind::Audio => &mut self.published_tracks.audio,
        };

        tracks.insert(track_meta.id.origin_mid, track_handle.clone());
        tracing::info!("Published track: {}", track_meta.id);
    }

    fn add_available_track(&mut self, track_handle: &track::TrackHandle) {
        let track_meta = &track_handle.meta;
        let track_out = TrackOut {
            handle: track_handle.clone(),
            mid: None,
        };

        let (tracks, kind) = match track_meta.kind {
            MediaKind::Video => (
                &mut self.available_tracks.video,
                proto::sfu::TrackKind::Video,
            ),
            MediaKind::Audio => {
                // Add to audio selector immediately for filtering
                self.audio_selector.add_track(track_meta.id.clone());
                (
                    &mut self.available_tracks.audio,
                    proto::sfu::TrackKind::Audio,
                )
            }
        };

        tracks.insert(track_meta.id.internal.clone(), track_out);

        // Queue for client notification
        self.sync_state
            .pending_published
            .push(proto::sfu::TrackInfo {
                track_id: track_meta.id.to_string(),
                kind: kind as i32,
                participant_id: track_meta.id.origin_participant.to_string(),
            });

        tracing::info!("Available track: {}", track_meta.id);
    }

    fn remove_available_tracks(
        &mut self,
        track_ids: &HashMap<Arc<entity::TrackId>, track::TrackHandle>,
    ) {
        for track_id in track_ids.keys() {
            let track_out = self
                .available_tracks
                .video
                .remove(&track_id.internal)
                .or_else(|| self.available_tracks.audio.remove(&track_id.internal));

            if let Some(track_out) = track_out {
                // Remove from audio selector if it was an audio track
                if track_out.handle.meta.kind == MediaKind::Audio {
                    self.audio_selector.remove_track(track_id);
                }

                if let Some(mid) = track_out.mid {
                    // Clear the slot
                    match track_out.handle.meta.kind {
                        MediaKind::Video => {
                            if let Some(slot) = self.subscribed_slots.video.get_mut(&mid) {
                                slot.track_id = None;
                            }
                        }
                        MediaKind::Audio => {
                            if let Some(slot) = self.subscribed_slots.audio.get_mut(&mid) {
                                slot.track_id = None;
                            }
                        }
                    }
                }

                self.sync_state
                    .pending_unpublished
                    .push(track_id.to_string());
                tracing::info!("Removed track: {}", track_id);
            }
        }
    }

    async fn handle_transmit(&mut self, transmit: Transmit) {
        let packet = net::SendPacket {
            buf: Bytes::copy_from_slice(&transmit.contents),
            dst: transmit.destination,
        };

        if let Err(e) = self
            .system_ctx
            .gw_handle
            .send_low(gateway::GatewayDataMessage::Packet(packet))
            .await
        {
            tracing::error!("Failed to send packet: {}", e);
        }
    }

    async fn handle_event(&mut self, ctx: &mut actor::ActorContext<Self>, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => match state {
                str0m::IceConnectionState::Disconnected => self.rtc.disconnect(),
                _ => tracing::trace!("ICE state: {:?}", state),
            },
            Event::MediaAdded(media) => {
                self.handle_media_added(ctx, media).await;
            }
            Event::ChannelOpen(id, label) => {
                if label == DATA_CHANNEL_LABEL {
                    self.data_channel = Some(id);
                    tracing::info!("Data channel opened");
                }
            }
            Event::ChannelData(data) => {
                let Some(ch) = self.data_channel else {
                    return;
                };

                if ch != data.id {
                    return;
                }

                if let Err(e) = self.handle_rpc(data).await {
                    tracing::warn!("RPC error: {}", e);
                }
            }
            Event::ChannelClose(id) => {
                if Some(id) == self.data_channel {
                    self.rtc.disconnect();
                }
            }
            Event::MediaData(data) => {
                self.handle_media_data(data).await;
            }
            Event::KeyframeRequest(req) => {
                self.handle_keyframe_request(req);
            }
            Event::Connected => {
                tracing::info!("Participant connected: {}", self.participant_id);
            }
            _ => tracing::trace!("Unhandled event: {:?}", event),
        }
    }

    async fn handle_media_added(&mut self, ctx: &mut actor::ActorContext<Self>, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                // Client publishing to us
                self.handle_incoming_media(ctx, media).await;
            }
            Direction::SendOnly => {
                // We're sending to client
                self.allocate_outgoing_slot(media);
            }
            dir => {
                tracing::warn!("Unsupported direction {:?}, disconnecting", dir);
                self.rtc.disconnect();
            }
        }
        self.sync_state.needs_resync = true;
    }

    async fn handle_incoming_media(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        media: MediaAdded,
    ) {
        let track_id = Arc::new(entity::TrackId::new(self.participant_id.clone(), media.mid));
        let track_meta = Arc::new(message::TrackMeta {
            id: track_id,
            kind: media.kind,
            simulcast_rids: media.simulcast.map(|s| s.recv),
        });

        let track_actor = track::TrackActor::new(ctx.handle.clone(), track_meta.clone());
        let (track_handle, join_handle) =
            actor::spawn(track_actor, actor::RunnerConfig::default().with_lo(1024));

        self.track_tasks.push(join_handle);

        if let Err(e) = self
            .room_handle
            .send_high(room::RoomMessage::PublishTrack(track_handle))
            .await
        {
            tracing::error!("Failed to publish track: {}", e);
        }
        tracing::info!("Published new track: {:?}", track_meta);
    }

    fn allocate_outgoing_slot(&mut self, media: MediaAdded) {
        let slots = match media.kind {
            MediaKind::Video => &mut self.subscribed_slots.video,
            MediaKind::Audio => &mut self.subscribed_slots.audio,
        };

        slots.insert(media.mid, MidSlot { track_id: None });
        tracing::info!("Allocated outgoing slot: {:?}", media.mid);
    }

    async fn handle_media_data(&mut self, data: MediaData) {
        // Handle contiguous flag for keyframe requests
        if data.contiguous {
            self.request_keyframe_internal(KeyframeRequest {
                rid: data.rid,
                mid: data.mid,
                kind: str0m::media::KeyframeRequestKind::Fir,
            });
        }

        // Forward to appropriate published track
        let track_handle = self
            .published_tracks
            .video
            .get_mut(&data.mid)
            .or_else(|| self.published_tracks.audio.get_mut(&data.mid));

        let Some(track) = track_handle else {
            return;
        };

        if let Err(e) = track
            .send_low(track::TrackDataMessage::ForwardMedia(Arc::new(data)))
            .await
        {
            tracing::error!("Failed to forward media: {}", e);
        }
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        let slot = self.subscribed_slots.video.get(&req.mid);
        let Some(MidSlot {
            track_id: Some(track_id),
            ..
        }) = slot
        else {
            return;
        };

        let Some(track_out) = self.available_tracks.video.get_mut(&track_id.internal) else {
            return;
        };

        let _ = track_out
            .handle
            .try_send_low(track::TrackDataMessage::KeyframeRequest(req.into()));
    }

    fn handle_forward_media(&mut self, track_meta: Arc<message::TrackMeta>, data: Arc<MediaData>) {
        let Some(track_out) = self.available_tracks.video.get(&track_meta.id.internal) else {
            return;
        };

        let Some(mid) = track_out.mid else {
            return;
        };

        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };

        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            tracing::error!("Failed to write media: {}", e);
            self.rtc.disconnect();
        }
    }

    fn request_keyframe_internal(&mut self, req: KeyframeRequest) {
        let Some(mut writer) = self.rtc.writer(req.mid) else {
            tracing::warn!("No writer for mid {:?}", req.mid);
            return;
        };

        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            tracing::warn!("Failed to request keyframe: {}", e);
        }
    }

    async fn handle_rpc(&mut self, data: ChannelData) -> Result<(), ParticipantError> {
        let msg = proto::sfu::ClientMessage::decode(data.data.as_slice())?;
        let Some(payload) = msg.payload else {
            return Ok(());
        };

        match payload {
            proto::sfu::client_message::Payload::Subscribe(req) => {
                let _mid = Mid::from(req.mid.as_str());
                // TODO: Implement subscription logic
            }
            proto::sfu::client_message::Payload::Unsubscribe(req) => {
                let _mid = Mid::from(req.mid.as_str());
                // TODO: Implement unsubscription logic
            }
        }

        Ok(())
    }
}

impl actor::MessageSet for ParticipantActor {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<entity::ParticipantId>;
    type ObservableState = ();
}

impl actor::Actor for ParticipantActor {
    fn meta(&self) -> Self::Meta {
        self.participant_id.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {
                let timeout = match self.poll(ctx).await {
                    Some(delay) => delay,
                    None => return Ok(()), // RTC disconnected
                };
            },
            select: {
                _ = tokio::time::sleep(timeout) => {
                    // Timer expired, poll again
                }
                Some((track_meta, _)) = self.track_tasks.next() => {
                    self.handle_track_finished(track_meta);
                }
            }
        );

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) {
        self.sync_state.needs_resync = true;

        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.handle_published_tracks(ctx, &tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.handle_published_tracks(ctx, &tracks);
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.remove_available_tracks(&tracks);
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {
                // TODO: Notify client of rejection
            }
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) {
        match msg {
            ParticipantDataMessage::UdpPacket(packet) => {
                let input = Input::Receive(
                    Instant::now().into_std(),
                    str0m::net::Receive {
                        proto: str0m::net::Protocol::Udp,
                        source: packet.src,
                        destination: packet.dst,
                        contents: (&*packet.buf).try_into().unwrap(),
                    },
                );

                if let Err(e) = self.rtc.handle_input(input) {
                    tracing::warn!("Dropped UDP packet: {}", e);
                }
            }
            ParticipantDataMessage::ForwardMedia(track_meta, data) => {
                self.handle_forward_media(track_meta, data);
            }
            ParticipantDataMessage::KeyframeRequest(track_id, req) => {
                self.request_keyframe_internal(KeyframeRequest {
                    mid: track_id.origin_mid,
                    kind: req.kind,
                    rid: req.rid,
                });
            }
        }
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantActor>;
