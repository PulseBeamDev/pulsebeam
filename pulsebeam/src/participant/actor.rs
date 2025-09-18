use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::{StreamExt, stream::FuturesUnordered};
use str0m::{Event, Output, Rtc, channel::ChannelId, media::MediaData};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, TrackId},
    gateway, message, room, system, track,
};

use super::core::{ParticipantCore, ParticipantEffect};
use super::rtc::WebRtcHandler;
use pulsebeam_runtime::{actor, net};

const DATA_CHANNEL_LABEL: &str = "pulsebeam::rpc";
pub type ParticipantHandle = actor::ActorHandle<ParticipantActor>;

/// High-priority control messages from the Room or other system components.
#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<Arc<TrackId>, track::TrackHandle>),
    TracksPublished(Arc<HashMap<Arc<TrackId>, track::TrackHandle>>),
    TracksUnpublished(Arc<HashMap<Arc<TrackId>, track::TrackHandle>>),
    TrackPublishRejected(track::TrackHandle),
}

/// Low-priority data messages (Media Packets, Keyframe Requests).
#[derive(Debug)]
pub enum ParticipantDataMessage {
    UdpPacket(net::RecvPacket),
    ForwardMedia(Arc<message::TrackMeta>, Arc<MediaData>),
    KeyframeRequest(Arc<TrackId>, message::KeyframeRequest),
}

/// The Actor shell for a Participant.
/// It coordinates the `ParticipantCore` (logic) and `WebRtcHandler` (I/O).
pub struct ParticipantActor {
    system_ctx: system::SystemContext,
    room_handle: room::RoomHandle,

    core: ParticipantCore,
    webrtc: WebRtcHandler,

    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackActor>>,
    data_channel_id: Option<ChannelId>,
}

impl ParticipantActor {
    pub fn new(
        system_ctx: system::SystemContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        Self {
            system_ctx,
            room_handle,
            core: ParticipantCore::new(participant_id),
            webrtc: WebRtcHandler::new(rtc),
            track_tasks: FuturesUnordered::new(),
            data_channel_id: None,
        }
    }

    /// The main WebRTC polling loop. Processes outputs and events until a timeout is reached.
    #[inline]
    async fn poll_and_process_webrtc(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
    ) -> Option<Duration> {
        // 1. Handle any pending state synchronization before polling.
        let effects = self.core.handle_resync_check();
        self.process_control_effects(ctx, effects).await;

        // 2. Poll the WebRTC engine.
        loop {
            let output = match self.webrtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = Instant::now().into_std();
                    return Some(deadline.saturating_duration_since(now));
                }
                Ok(output) => output,
                Err(e) => {
                    tracing::error!("WebRTC poll error: {e}, disconnecting participant");
                    self.webrtc.disconnect();
                    return None;
                }
            };

            match output {
                Output::Transmit(t) => {
                    self.process_control_effects(ctx, vec![ParticipantEffect::Transmit(t)])
                        .await;
                }
                Output::Event(e) => {
                    self.handle_webrtc_event(ctx, e).await;
                }
                Output::Timeout(_) => unreachable!("Handled above"),
            }

            if !self.webrtc.is_alive() {
                return None;
            }
        }
    }

    /// Handles events emitted by the WebRTC engine.
    #[inline]
    async fn handle_webrtc_event(&mut self, ctx: &mut actor::ActorContext<Self>, event: Event) {
        match event {
            // --- HOT PATH (INGRESS): Media from Client ---
            Event::MediaData(data) => {
                // Synchronous lookup and non-blocking forward.
                if let Some(track_handle) = self.core.get_published_track_mut(data.mid) {
                    // trace!(?data.mid, "Forwarding ingress media to track");
                    let _ = track_handle
                        .try_send_low(track::TrackDataMessage::ForwardMedia(Arc::new(data)));
                }
            }

            // --- CONTROL PLANE ---
            Event::MediaAdded(e) => {
                let effects = self.core.handle_media_added(e);
                self.process_control_effects(ctx, effects).await;
            }
            Event::KeyframeRequest(req) => {
                self.process_control_effects(
                    ctx,
                    self.core.handle_keyframe_request_from_client(req),
                )
                .await;
            }
            Event::ChannelOpen(cid, label) if label == DATA_CHANNEL_LABEL => {
                tracing::info!(?cid, "Data channel '{DATA_CHANNEL_LABEL}' opened");
                self.data_channel_id = Some(cid);
            }
            Event::ChannelData(data) if Some(data.id) == self.data_channel_id => {
                let effects = self.core.handle_rpc_data(data);
                self.process_control_effects(ctx, effects).await;
            }
            Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                tracing::warn!("ICE Disconnected");
                self.webrtc.disconnect();
            }
            Event::ChannelClose(cid) if Some(cid) == self.data_channel_id => {
                tracing::warn!("RPC Data Channel closed");
                self.webrtc.disconnect();
            }
            Event::Connected => {
                tracing::info!("Participant connected");
            }
            _ => {} // Ignore other events
        }
    }

    /// Executes the side effects generated by the `ParticipantCore` control plane.
    async fn process_control_effects(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        effects: Vec<ParticipantEffect>,
    ) {
        if effects.is_empty() {
            return;
        }

        for effect in effects {
            match effect {
                ParticipantEffect::Transmit(t) => {
                    let packet = Bytes::copy_from_slice(&t.contents);
                    let msg = gateway::GatewayDataMessage::Packet(net::SendPacket {
                        buf: packet,
                        dst: t.destination,
                    });
                    let _ = self.system_ctx.gw_handle.send_low(msg).await;
                }

                ParticipantEffect::RequestKeyframeToClient(req) => {
                    self.webrtc.request_keyframe(req);
                }

                ParticipantEffect::RequestKeyframeToTrack(mut handle, req) => {
                    let _ = handle.try_send_low(track::TrackDataMessage::KeyframeRequest(req));
                }

                ParticipantEffect::SubscribeToTrack(mut handle) => {
                    let _ = handle
                        .send_high(track::TrackControlMessage::Subscribe(ctx.handle.clone()))
                        .await;
                }

                ParticipantEffect::SendRpc(bytes) => {
                    let cid = match self.data_channel_id {
                        Some(cid) => cid,
                        None => continue,
                    };

                    let mut channel = match self.webrtc.channel(cid) {
                        Some(channel) => channel,
                        None => continue,
                    };

                    if let Err(e) = channel.write(true, &bytes) {
                        tracing::warn!("Failed to send RPC: {e}");
                    }
                }

                ParticipantEffect::SpawnTrack(meta) => {
                    tracing::info!(track_id=%meta.id, "Spawning new TrackActor for client publication");
                    let track_actor = track::TrackActor::new(ctx.handle.clone(), meta.clone());
                    let (track_handle, track_join) =
                        actor::spawn(track_actor, actor::RunnerConfig::default().with_lo(8192));
                    self.track_tasks.push(track_join);

                    let room_msg = room::RoomMessage::PublishTrack(track_handle.clone());
                    if let Err(e) = self.room_handle.send_high(room_msg).await {
                        tracing::error!("Failed to publish track to room: {e}");
                        // The track task will be cleaned up when the actor drops.
                    }
                }

                ParticipantEffect::Disconnect => {
                    self.webrtc.disconnect();
                }
            }
        }
    }
}

impl actor::MessageSet for ParticipantActor {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<ParticipantId>;
    type ObservableState = ();
}

impl actor::Actor for ParticipantActor {
    fn meta(&self) -> Self::Meta {
        self.core.state.participant_id.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {
                // Poll WebRTC and process all immediate outputs/events.
                let Some(delay) = self.poll_and_process_webrtc(ctx).await else {
                    break; // Disconnected or failed
                };
            },
            select: {
                // 1. Wait for the WebRTC timeout.
                _ = tokio::time::sleep(delay), if !delay.is_zero() => {
                    self.webrtc.handle_timeout(Instant::now().into_std());
                }

                // 2. Handle the termination of spawned TrackActors.
                Some((track_meta, _)) = self.track_tasks.next() => {
                    self.core.handle_track_unpublished(track_meta);
                }
            }
        );

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(t) => {
                self.core.handle_tracks_update(&t);
            }
            ParticipantControlMessage::TracksPublished(t) => {
                self.core.handle_tracks_update(t.as_ref());
            }
            ParticipantControlMessage::TracksUnpublished(t) => {
                self.core.handle_tracks_removed(t.as_ref());
            }
            ParticipantControlMessage::TrackPublishRejected(_handle) => {
                // TODO: Notify client
            }
        }
    }

    /// This handler is for the HOT PATH (data plane). It must be fast and non-blocking.
    async fn on_low_priority(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) {
        match msg {
            // HOT PATH (INGRESS): UDP Packet -> WebRTC Engine
            ParticipantDataMessage::UdpPacket(packet) => {
                self.webrtc.handle_input_packet(packet);
            }

            // HOT PATH (EGRESS): Track Actor -> WebRTC Engine -> Client
            ParticipantDataMessage::ForwardMedia(track_meta, data) => {
                // Synchronous lookups and write.
                if let Some(mid) = self.core.get_egress_mid(&track_meta.id) {
                    self.webrtc.write_media(mid, &data);
                }
            }

            // CONTROL PATH: Keyframe Request (Track -> Client)
            ParticipantDataMessage::KeyframeRequest(track_id, req) => {
                let effects = self.core.handle_keyframe_request_from_track(track_id, req);
                self.process_control_effects(ctx, effects).await;
            }
        }
    }
}
