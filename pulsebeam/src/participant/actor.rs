use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures::{StreamExt, io, stream::FuturesUnordered, task::SpawnExt};
use str0m::{
    Event, Input, Output, Rtc, RtcError, channel::ChannelId, error::SdpError,
    media::KeyframeRequest, net::Transmit, rtp::RtpPacket,
};
use tokio::time::Instant;

use crate::{
    entity, gateway, message, node,
    participant::{
        batcher::Batcher,
        core::ParticipantCore,
        effect::{self, Effect},
    },
    room, track,
};
use pulsebeam_runtime::{actor, collections::double_buffer, mailbox, net};

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
    ForwardRtp(Arc<message::TrackMeta>, Arc<RtpPacket>),
    KeyframeRequest(Arc<entity::TrackId>, message::KeyframeRequest),
}

pub struct ParticipantContext {
    // Core dependencies
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    rtc: Rtc,
    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackMessageSet>>,

    egress: Arc<net::UnifiedSocket>,
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

    batcher: Batcher,
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
        let ufrag = self.ctx.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = mailbox::new(64);
        self.ctx
            .node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;

        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {
                let timeout = match self.poll().await {
                    Some(delay) => delay,
                    None => return Ok(()), // RTC disconnected
                };

                self.ctx.apply_effects(&mut self.effects, &ctx.handle).await;
            },
            select: {
                // biased toward writing to socket
                Ok(_) = self.ctx.egress.writable(), if !self.batcher.is_empty() => {
                    if let Err(err) = self.flush_egress() {
                        tracing::error!("failed to write socket: {err}");
                        break;
                    }
                }
                Some(pkt) = gateway_rx.recv() => {
                    self.handle_udp_packet(pkt);
                }
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
            ParticipantDataMessage::ForwardRtp(track_meta, rtp) => {
                self.handle_forward_rtp(track_meta, rtp);
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
    const MAX_MTU: usize = 1500;

    pub fn new(
        node_ctx: node::NodeContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        let egress = node_ctx.allocate_egress();
        let gso_segments = egress.max_gso_segments();
        let ctx = ParticipantContext {
            node_ctx,
            room_handle,
            rtc,
            track_tasks: FuturesUnordered::new(),
            egress,
        };

        let core = ParticipantCore::new(participant_id);
        let effects = VecDeque::with_capacity(16);

        Self {
            ctx,
            core,
            effects,
            data_channel: None,
            batcher: Batcher::with_capacity(gso_segments * Self::MAX_MTU),
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
                    self.batcher
                        .push_back(transmit.destination, transmit.contents.into());
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

    fn flush_egress(&mut self) -> io::Result<()> {
        while let Some(state) = self.batcher.front() {
            let ok = self.ctx.egress.try_send_batch(&net::SendPacketBatch {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            });

            tracing::trace!("flush egress: {ok}");
            if ok {
                self.batcher.pop_front();
            } else {
                break;
            }
        }

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => match state {
                str0m::IceConnectionState::Disconnected => self.ctx.rtc.disconnect(),
                _ => tracing::trace!("ICE state: {:?}", state),
            },
            Event::MediaAdded(media) => {
                tracing::debug!("media added: {media:?}");
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
            Event::RtpPacket(rtp) => {
                self.handle_rtp_packet(rtp).await;
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

    async fn handle_rtp_packet(&mut self, mut rtp: RtpPacket) {
        tracing::trace!("handle_rtp_packet");
        let mut api = self.ctx.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            tracing::warn!("no stream_rx matched rtp ssrc, dropping.");
            return;
        };

        let Some(track) = self.core.get_published_track_mut(&stream.mid()) else {
            tracing::warn!("no published track matched mid, dropping.");
            return;
        };

        // HACK: inject SDP rid to rtp.
        rtp.header.ext_vals.rid = stream.rid();
        if let Err(e) = track
            .send_low(track::TrackDataMessage::ForwardRtp(Arc::new(rtp)))
            .await
        {
            tracing::error!("Failed to forward rtp: {}", e);
        }
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        let Some(track_handle) = self.core.video_allocator.get_track_mut(&req.mid) else {
            return;
        };
        let _ = track_handle.try_send_low(track::TrackDataMessage::KeyframeRequest(req.into()));
    }

    fn handle_udp_packet(&mut self, pkt: net::RecvPacket) {
        let input = Input::Receive(
            Instant::now().into_std(),
            str0m::net::Receive {
                proto: str0m::net::Protocol::Udp,
                source: pkt.src,
                destination: pkt.dst,
                contents: (&*pkt.buf).try_into().unwrap(),
            },
        );

        if let Err(e) = self.ctx.rtc.handle_input(input) {
            tracing::warn!("dropped UDP packet: {}", e);
        }
    }

    fn handle_forward_rtp(&mut self, track_meta: Arc<message::TrackMeta>, rtp: Arc<RtpPacket>) {
        tracing::trace!("handle_forward_rtp");
        let Some(mid) = self.core.get_slot(&track_meta, &rtp.header.ext_vals) else {
            return;
        };

        let pt = {
            let Some(media) = self.ctx.rtc.media(mid) else {
                tracing::warn!("no media found, dropping");
                return;
            };

            let Some(pt) = media.remote_pts().first() else {
                // TODO: better logic than matching first pt
                tracing::warn!("no remote pt found, dropping");
                return;
            };
            *pt
        };

        let mut api = self.ctx.rtc.direct_api();
        let Some(writer) = api.stream_tx_by_mid(mid, None) else {
            return;
        };

        if let Err(err) = writer.write_rtp(
            pt,
            rtp.seq_no,
            rtp.header.timestamp,
            rtp.timestamp,
            rtp.header.marker,
            rtp.header.ext_vals.clone(),
            true,
            rtp.payload.clone(),
        ) {
            tracing::warn!("failed to write rtp: {err}");
        }
    }

    fn request_keyframe_internal(&mut self, req: KeyframeRequest) {
        tracing::debug!("request_keyframe_internal");
        self.request_keyframe_internal_rtp(req);
    }

    fn request_keyframe_internal_media(&mut self, req: KeyframeRequest) {
        let Some(mut writer) = self.ctx.rtc.writer(req.mid) else {
            tracing::warn!("no writer for mid {:?}", req.mid);
            return;
        };

        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            tracing::warn!("failed to request keyframe: {}", e);
        }
    }

    fn request_keyframe_internal_rtp(&mut self, req: KeyframeRequest) {
        let mut api = self.ctx.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(req.mid, req.rid) else {
            tracing::warn!("stream_rx not found, keyframe request failed");
            return;
        };

        stream.request_keyframe(req.kind);
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
