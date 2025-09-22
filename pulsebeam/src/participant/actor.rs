use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures::{StreamExt, stream::FuturesUnordered};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    channel::ChannelId,
    error::SdpError,
    media::{Direction, KeyframeRequest, MediaAdded, MediaData, MediaKind, Mid},
    net::Transmit,
};
use tokio::time::Instant;

use crate::{
    entity, gateway, message,
    participant::{audio::AudioAllocator, core::ParticipantCore, effect, video::VideoAllocator},
    room, system, track,
};
use pulsebeam_runtime::{actor, net};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::rpc";

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
    // #[error("Invalid RPC format: {0}")]
    // InvalidRpcFormat(#[from] DecodeError),
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

pub struct ParticipantContext {
    // Core dependencies
    system_ctx: system::SystemContext,
    room_handle: room::RoomHandle,
    rtc: Rtc,
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
    ctx: ParticipantContext,

    // Identity
    participant_id: Arc<entity::ParticipantId>,
    data_channel: Option<ChannelId>,

    core: ParticipantCore,
    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackActor>>,
    effects: VecDeque<effect::Effect>,
}

impl ParticipantActor {
    pub fn new(
        system_ctx: system::SystemContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        let ctx = ParticipantContext {
            system_ctx,
            room_handle,
            rtc,
        };

        let core = ParticipantCore::new();
        let effects = VecDeque::with_capacity(16);

        Self {
            ctx,
            core,
            effects,
            participant_id,
            data_channel: None,
            track_tasks: FuturesUnordered::new(),
        }
    }

    /// Main event loop with proper timeout handling
    async fn poll(&mut self, ctx: &mut actor::ActorContext<Self>) -> Option<Duration> {
        while self.rtc.is_alive() {
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

        let effects: Vec<effect::Effect> = self
            .state
            .video_allocator
            .effects
            .drain(..)
            .chain(self.state.audio_allocator.effects.drain(..))
            .collect();

        None
    }

    async fn apply_effects(&mut self, effects: &[effect::Effect]) {
        for e in &effects {}
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
            if track_handle.meta.id.origin_participant == self.participant_id {
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
        match track_handle.meta.kind {
            MediaKind::Video => {
                self.state
                    .video_allocator
                    .add_track(track_handle.clone(), &mut self.effects);
            }
            MediaKind::Audio => {
                todo!();
            }
        }
    }

    fn remove_available_tracks(
        &mut self,
        track_ids: &HashMap<Arc<entity::TrackId>, track::TrackHandle>,
    ) {
        for (track_id, track_handle) in track_ids.into_iter() {
            match track_handle.meta.kind {
                MediaKind::Video => {
                    self.video_allocator.remove_track(track_id);
                }
                MediaKind::Audio => todo!(),
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

                // TODO: handle PulseBeam signaling
                // if let Err(e) = self.handle_rpc(data).await {
                //     tracing::warn!("RPC error: {}", e);
                // }
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
        let (track_handle, join_handle) = actor::spawn(
            track_actor,
            actor::RunnerConfig::default().with_lo(1024).with_hi(1024),
        );

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
        match media.kind {
            MediaKind::Video => self.video_allocator.add_slot(media.mid),
            MediaKind::Audio => {
                todo!()
            }
        }
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
        let Some(track_handle) = self.video_allocator.get_track_mut(&req.mid) else {
            return;
        };
        let _ = track_handle.try_send_low(track::TrackDataMessage::KeyframeRequest(req.into()));
    }

    fn handle_forward_media(&mut self, track_meta: Arc<message::TrackMeta>, data: Arc<MediaData>) {
        let Some(mid) = self.video_allocator.get_slot(&track_meta.id) else {
            return;
        };

        let Some(writer) = self.rtc.writer(*mid) else {
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
