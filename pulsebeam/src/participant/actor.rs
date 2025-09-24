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
    media::{KeyframeRequest, MediaData},
    net::Transmit,
};
use tokio::time::Instant;

use crate::{
    entity, gateway, message,
    participant::{
        core::ParticipantCore,
        effect::{self, Effect},
    },
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
    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackMessageSet>>,
}

impl ParticipantContext {
    async fn apply_effects(
        &mut self,
        effects: &mut effect::Queue,
        self_handle: &ParticipantHandle,
    ) {
        if effects.is_empty() {
            return;
        }

        tracing::debug!("applying effects: {:?}", effects);
        for e in effects.drain(..) {
            match e {
                Effect::Subscribe(mut track_handle) => {
                    if track_handle
                        .send_high(track::TrackControlMessage::Subscribe(self_handle.clone()))
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "failed to subscribe to {}. Will be cleaned up by room broadcast",
                            track_handle.meta
                        );
                    }
                }
                Effect::SpawnTrack(track_meta) => {
                    let track_actor =
                        track::TrackActor::new(self_handle.clone(), track_meta.clone());
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
                    tracing::info!("published track: {}", track_meta.id);
                }
                Effect::Disconnect => {
                    self.rtc.disconnect();
                }
            }
        }
    }
}

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<entity::ParticipantId>;
    type ObservableState = ();
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
    data_channel: Option<ChannelId>,

    core: ParticipantCore,
    effects: VecDeque<effect::Effect>,
}

impl actor::Actor<ParticipantMessageSet> for ParticipantActor {
    fn meta(&self) -> Arc<entity::ParticipantId> {
        self.core.participant_id.clone()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {
                let timeout = match self.poll().await {
                    Some(delay) => delay,
                    None => return Ok(()), // RTC disconnected
                };

                self.ctx.apply_effects(&mut self.effects, &ctx.handle).await;
            },
            select: {
                _ = tokio::time::sleep(timeout) => {
                    // Timer expired, poll again
                }
                Some((track_meta, _)) = self.ctx.track_tasks.next() => {
                    self.core.handle_track_finished(track_meta);
                }
            }
        );

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<ParticipantMessageSet>,
        msg: ParticipantControlMessage,
    ) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.core
                    .handle_published_tracks(&mut self.effects, &tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.core
                    .handle_published_tracks(&mut self.effects, &tracks);
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core
                    .remove_available_tracks(&mut self.effects, &tracks);
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {
                // TODO: Notify client of rejection
            }
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<ParticipantMessageSet>,
        msg: ParticipantDataMessage,
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

                if let Err(e) = self.ctx.rtc.handle_input(input) {
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
            track_tasks: FuturesUnordered::new(),
        };

        let core = ParticipantCore::new(participant_id);
        let effects = VecDeque::with_capacity(16);

        Self {
            ctx,
            core,
            effects,
            data_channel: None,
        }
    }

    /// Main event loop with proper timeout handling
    async fn poll(&mut self) -> Option<Duration> {
        while self.ctx.rtc.is_alive() {
            match self.ctx.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = Instant::now().into_std();
                    let duration = deadline.saturating_duration_since(now);

                    if duration.is_zero() {
                        // Handle timeout immediately
                        if let Err(e) = self.ctx.rtc.handle_input(Input::Timeout(now)) {
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
                    self.handle_event(event).await;
                }
                Err(e) => {
                    tracing::error!("RTC poll error: {}", e);
                    break;
                }
            }
        }

        None
    }

    async fn handle_transmit(&mut self, transmit: Transmit) {
        let packet = net::SendPacket {
            buf: Bytes::copy_from_slice(&transmit.contents),
            dst: transmit.destination,
        };

        if let Err(e) = self
            .ctx
            .system_ctx
            .gw_handle
            .send_low(gateway::GatewayDataMessage::Packet(packet))
            .await
        {
            tracing::error!("Failed to send packet: {}", e);
        }
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => match state {
                str0m::IceConnectionState::Disconnected => self.ctx.rtc.disconnect(),
                _ => tracing::trace!("ICE state: {:?}", state),
            },
            Event::MediaAdded(media) => {
                self.core.handle_media_added(&mut self.effects, media);
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
                    self.ctx.rtc.disconnect();
                }
            }
            Event::MediaData(data) => {
                self.handle_media_data(data).await;
            }
            Event::KeyframeRequest(req) => {
                self.handle_keyframe_request(req);
            }
            Event::Connected => {
                tracing::info!("connected");
            }
            _ => tracing::trace!("Unhandled event: {:?}", event),
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

        let Some(track) = self.core.get_published_track_mut(&data.mid) else {
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
        let Some(track_handle) = self.core.video_allocator.get_track_mut(&req.mid) else {
            return;
        };
        let _ = track_handle.try_send_low(track::TrackDataMessage::KeyframeRequest(req.into()));
    }

    fn handle_forward_media(&mut self, track_meta: Arc<message::TrackMeta>, data: Arc<MediaData>) {
        let Some(mid) = self.core.get_slot(&track_meta, &data) else {
            return;
        };

        let Some(writer) = self.ctx.rtc.writer(mid) else {
            return;
        };

        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            tracing::error!("Failed to write media: {}", e);
        }
    }

    fn request_keyframe_internal(&mut self, req: KeyframeRequest) {
        let Some(mut writer) = self.ctx.rtc.writer(req.mid) else {
            tracing::warn!("No writer for mid {:?}", req.mid);
            return;
        };

        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            tracing::warn!("Failed to request keyframe: {}", e);
        }
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
