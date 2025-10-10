use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures::{Stream, io};
use futures_concurrency::stream::StreamGroup;
use pulsebeam_runtime::{prelude::*, sync::spmc};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    channel::ChannelId,
    error::SdpError,
    media::{KeyframeRequest, MediaKind},
    rtp::RtpPacket,
};
use tokio::time::Instant;

use crate::{
    entity::{self, TrackId},
    gateway, message, node,
    participant::{
        batcher::Batcher,
        core::ParticipantCore,
        effect::{self, Effect},
    },
    room,
    track::{self, TrackMeta},
};
use pulsebeam_runtime::{actor, mailbox, net};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::rpc";

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
}

#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<Arc<entity::TrackId>, track::TrackReceiver>),
    TracksPublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackReceiver>>),
    TracksUnpublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackReceiver>>),
    TrackPublishRejected(track::TrackReceiver),
}

#[derive(Debug)]
pub enum ParticipantDataMessage {
    ForwardRtp(Arc<track::TrackMeta>, Arc<RtpPacket>),
    KeyframeRequest(Arc<entity::TrackId>, message::KeyframeRequest),
}

pub struct ParticipantContext {
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    rtc: Rtc,
    egress: Arc<net::UnifiedSocket>,
}

impl ParticipantContext {
    async fn apply_effects(&mut self, core: &mut ParticipantCore, effects: &mut effect::Queue) {
        if effects.is_empty() {
            return;
        }

        tracing::debug!("Applying effects: {:?}", effects);
        for e in effects.drain(..) {
            match e {
                Effect::SpawnTrack(track_meta) => {
                    let (tx, rx) = track::new(track_meta, 64);
                    core.add_published_track(tx);

                    if let Err(e) = self
                        .room_handle
                        .send_high(room::RoomMessage::PublishTrack(rx))
                        .await
                    {
                        tracing::error!("Failed to publish track: {}", e);
                    }
                }
                Effect::Disconnect => {
                    self.rtc.disconnect();
                }
                _ => {}
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

/// The SFU participant actor: manages WebRTC and media flow.
pub struct ParticipantActor {
    ctx: ParticipantContext,
    data_channel: Option<ChannelId>,
    core: ParticipantCore,
    effects: VecDeque<effect::Effect>,
    batcher: Batcher,
    // TODO: This shouldn't return track meta..
    stream_group: StreamGroup<Pin<Box<dyn Stream<Item = (Arc<TrackMeta>, Arc<RtpPacket>)> + Send>>>,
}

impl actor::Actor<ParticipantMessageSet> for ParticipantActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

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
                    None => return Ok(()), // disconnected
                };
                self.ctx.apply_effects(&mut self.core, &mut self.effects).await;
            },
            select: {
                Ok(_) = self.ctx.egress.writable(), if !self.batcher.is_empty() => {
                    let _ = self.flush_egress();
                }
                Some((meta, rtp)) = self.stream_group.next() => {
                    self.handle_forward_rtp(meta, rtp);
                }
                Some(pkt) = gateway_rx.recv() => {
                    self.handle_udp_packet(pkt);
                }
                _ = tokio::time::sleep(timeout) => {
                    // Timer expired, poll again
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

                for (_, track) in &tracks {
                    if track.meta.kind == MediaKind::Video {
                        let Some(mut simulcast) = track.by_default() else {
                            continue;
                        };

                        let meta = track.meta.clone();
                        let stream = async_stream::stream! {
                            loop {
                                match simulcast.channel.recv().await {
                                    Ok(Some(pkt)) => yield (meta.clone(), pkt),
                                    Ok(None) => break,                      // channel closed
                                    Err(spmc::RecvError::Lagged(n)) => {
                                        tracing::warn!("lagged by {n}");
                                        continue;
                                    }, // skip dropped frames
                                }
                            }
                        };
                        self.stream_group.insert(stream.boxed());
                    }
                }
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.core
                    .handle_published_tracks(&mut self.effects, &tracks);

                for (_, track) in tracks.as_ref() {
                    if track.meta.kind == MediaKind::Video {
                        let Some(mut simulcast) = track.by_default() else {
                            continue;
                        };

                        let meta = track.meta.clone();
                        let stream = async_stream::stream! {
                            loop {
                                match simulcast.channel.recv().await {
                                    Ok(Some(pkt)) => yield (meta.clone(), pkt),
                                    Ok(None) => break,                      // channel closed
                                    Err(spmc::RecvError::Lagged(n)) => {
                                        tracing::warn!("lagged by {n}");
                                        continue;
                                    }, // skip dropped frames
                                }
                            }
                        };
                        self.stream_group.insert(stream.boxed());
                    }
                }
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core
                    .remove_available_tracks(&mut self.effects, &tracks);
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {
                // could notify client
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
                self.request_keyframe(KeyframeRequest {
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
            egress,
        };

        let core = ParticipantCore::new(participant_id);
        let effects = VecDeque::with_capacity(16);

        Self {
            ctx,
            core,
            effects,
            data_channel: None,
            stream_group: StreamGroup::new(),
            batcher: Batcher::with_capacity(gso_segments * Self::MAX_MTU),
        }
    }

    async fn poll(&mut self) -> Option<Duration> {
        while self.ctx.rtc.is_alive() {
            match self.ctx.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = Instant::now().into_std();
                    let duration = deadline.saturating_duration_since(now);

                    if duration.is_zero() {
                        let _ = self.ctx.rtc.handle_input(Input::Timeout(now));
                        continue;
                    }

                    return Some(duration);
                }
                Ok(Output::Transmit(tx)) => {
                    self.batcher.push_back(tx.destination, tx.contents.into());
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
            if self.ctx.egress.try_send_batch(&net::SendPacketBatch {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            }) {
                self.batcher.pop_front();
            } else {
                break;
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => {
                tracing::trace!("ICE state: {:?}", state);
                if state == str0m::IceConnectionState::Disconnected {
                    self.ctx.rtc.disconnect();
                }
            }
            Event::MediaAdded(media) => {
                tracing::debug!("Media added: {:?}", media);
                self.core.handle_media_added(&mut self.effects, media);
            }
            Event::RtpPacket(rtp) => {
                self.handle_rtp_packet(rtp).await;
            }
            Event::KeyframeRequest(req) => {
                self.handle_keyframe_request(req);
            }
            Event::Connected => {
                tracing::info!("Connected");
            }
            _ => tracing::trace!("Unhandled event: {:?}", event),
        }
    }

    async fn handle_rtp_packet(&mut self, mut rtp: RtpPacket) {
        let mut api = self.ctx.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            tracing::warn!("no stream_rx matched ssrc, dropping");
            return;
        };

        let Some(track) = self.core.get_published_track_mut(&stream.mid()) else {
            tracing::warn!("no published track matched mid, dropping");
            return;
        };

        rtp.header.ext_vals.rid = stream.rid();

        // Forward into track’s broadcast channel
        track.send(stream.rid().as_ref(), rtp);
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        let Some(track_handle) = self.core.video_allocator.get_track_mut(&req.mid) else {
            return;
        };
        let _ = track_handle;
    }

    fn handle_udp_packet(&mut self, packet: net::RecvPacket) {
        let contents = match (&*packet.buf).try_into() {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!("invalid packet, dropping: {err}");
                return;
            }
        };

        let recv = str0m::net::Receive {
            proto: str0m::net::Protocol::Udp,
            source: packet.src,
            destination: packet.dst,
            contents,
        };
        let ev = Input::Receive(Instant::now().into_std(), recv);
        if let Err(e) = self.ctx.rtc.handle_input(ev) {
            tracing::error!("Rtc receive error: {}", e);
        }
    }

    fn handle_forward_rtp(&mut self, track_meta: Arc<track::TrackMeta>, rtp: Arc<RtpPacket>) {
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
            tracing::warn!("no mid found, dropping");
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

    fn request_keyframe(&mut self, req: KeyframeRequest) {
        let mut api = self.ctx.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(req.mid, req.rid) else {
            tracing::warn!("stream_rx not found, keyframe request failed");
            return;
        };

        stream.request_keyframe(req.kind);
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
