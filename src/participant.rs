use std::{
    collections::HashMap,
    fmt::{self, Display, Pointer},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use prost::{DecodeError, Message};
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
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
use tracing::instrument;

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
    mid: Mid,
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

        loop {
            let deadline = if let Some(deadline) = self.poll().await {
                deadline
            } else {
                // Rtc timeout
                break;
            };

            tokio::select! {
                msg = self.data_receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_data_message(msg).await,
                        None => break,
                    }
                }

                msg = self.control_receiver.recv() => {
                    match msg {
                        Some(msg) => self.handle_control_message(msg).await,
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
    async fn handle_data_message(&mut self, msg: ParticipantDataMessage) {
        match msg {
            ParticipantDataMessage::UdpPacket(packet) => {
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
            ParticipantDataMessage::ForwardMedia(track, data) => {
                self.handle_forward_media(track, data);
            }

            ParticipantDataMessage::KeyframeRequest(track_id, req) => {
                let Some(mut writer) = self.rtc.writer(track_id.origin_mid) else {
                    tracing::warn!(mid=?track_id.origin_mid, "mid is not found for regenerating a keyframe");
                    return;
                };

                writer.request_keyframe(req.rid, req.kind);
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
                    for (mid, slot) in &mut self.mid_out_slots {
                        if slot.track_id.is_none() {
                            track.subscribe(self.handle.clone()).await;
                            slot.track_id = Some(track_id.clone());
                            let track_out = TrackOut {
                                handle: track,
                                mid: *mid,
                            };
                            self.subscribed_tracks.insert(track_id, track_out);
                            return;
                        }
                    }

                    // HACK: test swapping stream
                    // let Some((mid, slot)) = self.mid_out_slots.iter_mut().next() else {
                    //     return;
                    // };
                    // track.subscribe(self.handle.clone()).await;
                    // slot.track_id = Some(track_id.clone());
                    // let track_out = TrackOut {
                    //     handle: track,
                    //     mid: *mid,
                    // };
                    // self.subscribed_tracks.insert(track_id, track_out);
                    // tracing::warn!("swapping stream");
                    // return;
                }
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
                    tracing::debug!("forwarding media data to {}", e.mid);
                    track.forward_media(Arc::new(e));
                }
            }
            Event::KeyframeRequest(req) => self.handle_keyframe_request(req),
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
        tracing::info!(?media, "handle_new_media");
        match media.direction {
            // client -> SFU
            Direction::RecvOnly => {
                // TODO: handle back pressure by buffering temporarily
                let track_id = TrackId::new(self.participant_id.clone(), media.mid);
                let track_id = Arc::new(track_id);
                let track = TrackIn {
                    id: track_id,
                    kind: media.kind,
                    simulcast: media.simulcast,
                };
                self.room.publish(track).await;
            }
            // SFU -> client
            Direction::SendOnly => {
                self.mid_out_slots.insert(
                    media.mid,
                    MidOutSlot {
                        kind: media.kind,
                        simulcast: media.simulcast,
                        track_id: None,
                    },
                );
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

        let Some(writer) = self.rtc.writer(track.mid) else {
            return;
        };

        if let Err(err) = writer.write(data.pt, data.network_time, data.time, data.data.clone()) {
            tracing::error!("failed to write media: {}", err);
            self.rtc.disconnect();
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
        source: UdpSourceHandle,
        sink: UdpSinkHandle,
        room: RoomHandle,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> (Self, ParticipantActor) {
        let (data_sender, data_receiver) = mpsc::channel(64);
        let (control_sender, control_receiver) = mpsc::channel(1);
        let handle = Self {
            data_sender,
            control_sender,
            participant_id: participant_id.clone(),
        };
        let actor = ParticipantActor {
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
