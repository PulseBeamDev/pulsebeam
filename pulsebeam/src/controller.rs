use std::{collections::HashMap, io, sync::Arc, time::Duration};

use crate::{
    entity::{ConnectionId, ParticipantId, RoomId},
    node,
    participant::ParticipantActor,
    room,
};
use pulsebeam_runtime::{
    actor::{self, ActorKind, ActorStatus},
    net::UnifiedSocketWriter,
};
use pulsebeam_runtime::{net::Transport, prelude::*};
use str0m::{
    Candidate, Rtc, RtcConfig, RtcError,
    change::{SdpAnswer, SdpOffer},
    format::{Codec, FormatParams},
    media::{Direction, Frequency, MediaKind, Pt},
    net::TcpType,
};
use tokio::{sync::oneshot, task::JoinSet, time::Instant};

pub const MAX_RECV_VIDEO_SLOTS: usize = 1;
pub const MAX_RECV_AUDIO_SLOTS: usize = 1;
pub const MAX_SEND_VIDEO_SLOTS: usize = 16;
pub const MAX_SEND_AUDIO_SLOTS: usize = 9;
pub const MAX_DATA_CHANNELS: usize = 1;

#[derive(Debug)]
pub enum MediaType {
    Video,
    Audio,
    Application,
    Unknown,
}

impl MediaType {
    fn as_str(&self) -> &str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Application => "application",
            Self::Unknown => "unknown",
        }
    }
}

impl From<&str> for MediaType {
    fn from(value: &str) -> Self {
        match value {
            "video" => MediaType::Video,
            "audio" => MediaType::Audio,
            "application" => MediaType::Application,
            _ => MediaType::Unknown,
        }
    }
}

impl From<MediaType> for MediaKind {
    fn from(value: MediaType) -> Self {
        match value {
            MediaType::Video => MediaKind::Video,
            MediaType::Audio => MediaKind::Audio,
            typ => panic!("unexpected media type: {}", typ),
        }
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

#[derive(Debug, derive_more::From)]
pub enum ControllerMessage {
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

#[derive(Debug)]
pub struct CreateParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(thiserror::Error, Debug)]
pub enum OfferRejectedReason {
    #[error("RTC engine error: {0}")]
    Rtc(#[from] RtcError),
    #[error("{0} {1} slots limit exceeded (max {2})")]
    SlotsLimit(MediaType, Direction, usize),
    #[error("SendRecv direction is not supported for {0}")]
    DirectionNotSupported(MediaType),
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] OfferRejectedReason),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

pub struct ControllerMessageSet;

impl actor::MessageSet for ControllerMessageSet {
    type Msg = ControllerMessage;
    type Meta = Arc<String>;
    type ObservableState = ();
}

pub struct ControllerActor {
    node_ctx: node::NodeContext,

    id: Arc<String>,
    /// Each room entry pairs the actor handle with the track-watch sender so that
    /// new participants can subscribe to the room's authoritative track snapshot
    /// without any round-trip to the room actor.
    rooms: HashMap<RoomId, RoomEntry>,
    room_tasks: JoinSet<(RoomId, ActorStatus)>,
    /// Monotonic counter for round-robin assignment of rooms to cpu_handles.
    room_counter: usize,
}

/// Bundles a room's actor handle with the write-half of its track watch channel.
struct RoomEntry {
    handle: room::RoomHandle,
    track_watch: room::TrackWatchSender,
}

impl actor::Actor<ControllerMessageSet> for ControllerActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> ActorKind {
        "controller"
    }

    fn meta(&self) -> Arc<String> {
        self.id.clone()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ControllerMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select:{} ,
            select: {
                Some(Ok((room_id, _))) = self.room_tasks.join_next() => {
                    self.rooms.remove(&room_id);
                }
            }
        );
        Ok(())
    }

    async fn on_msg(
        &mut self,
        ctx: &mut actor::ActorContext<ControllerMessageSet>,
        msg: ControllerMessage,
    ) -> () {
        match msg {
            ControllerMessage::CreateParticipant(m, reply_tx) => {
                let answer = self.handle_create_participant(ctx, m).await;
                let _ = reply_tx.send(answer);
            }

            ControllerMessage::DeleteParticipant(m) => {
                if let Some(entry) = self.rooms.get_mut(&m.room_id) {
                    // if the room has exited, the participants have already cleaned up too.
                    let _ = entry
                        .handle
                        .send(room::RemoveParticipant {
                            participant_id: m.participant_id,
                        })
                        .await;
                }
            }
            ControllerMessage::PatchParticipant(m, reply_tx) => {
                let answer = self.handle_patch_participant(ctx, m).await;
                let _ = reply_tx.send(answer);
            }
        }
    }
}

impl ControllerActor {
    pub async fn handle_create_participant(
        &mut self,
        _ctx: &mut actor::ActorContext<ControllerMessageSet>,
        m: CreateParticipant,
    ) -> Result<CreateParticipantReply, ControllerError> {
        let answer = self.create_participant(m.offer, m.state).await?;
        Ok(CreateParticipantReply { answer })
    }

    pub async fn handle_patch_participant(
        &mut self,
        _ctx: &mut actor::ActorContext<ControllerMessageSet>,
        m: PatchParticipant,
    ) -> Result<PatchParticipantReply, ControllerError> {
        let answer = self.create_participant(m.offer, m.state).await?;
        Ok(PatchParticipantReply { answer })
    }

    async fn create_participant(
        &mut self,
        offer: SdpOffer,
        s: ParticipantState,
    ) -> Result<SdpAnswer, ControllerError> {
        let udp_egress = self.node_ctx.allocate_udp_egress();
        let tcp_egress = self.node_ctx.allocate_tcp_egress();
        // Clone the gateway handle before the mutable borrow taken by get_or_create_room.
        let gateway = self.node_ctx.gateway.clone();
        let sockets = [&udp_egress, &tcp_egress];

        let (rtc, answer) = self.create_answer(offer, &sockets)?;

        // Scope the entry borrow so it is released before we need `room_handle` for the send.
        let (room_handle, tracks_rx) = {
            let entry = self.get_or_create_room(&s.room_id);
            // Subscribe before the participant is spawned so it sees the current
            // track snapshot immediately on first poll — no TracksSnapshot message needed.
            let tracks_rx = entry.track_watch.subscribe();
            let room_handle = entry.handle.clone();
            (room_handle, tracks_rx)
        };

        let participant = ParticipantActor::new(
            gateway,
            room_handle.clone(),
            udp_egress,
            tcp_egress,
            s.participant_id,
            rtc,
            s.manual_sub,
            tracks_rx,
        );
        // TODO: probably retry? Or, let the client to retry instead?
        // Each room will always have a graceful timeout before closing.
        // But, a data race can still occur nonetheless
        room_handle
            .tx
            .send(room::RoomMessage::AddParticipant(room::AddParticipant {
                participant,
                connection_id: s.connection_id,
                old_connection_id: s.old_connection_id,
            }))
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        Ok(answer)
    }

    fn create_answer(
        &mut self,
        offer: SdpOffer,
        sockets: &[&UnifiedSocketWriter],
    ) -> Result<(Rtc, SdpAnswer), ControllerError> {
        const PT_OPUS: Pt = Pt::new_with_value(111);

        tracing::debug!("{offer}");
        let mut rtc_config = RtcConfig::new()
            .clear_codecs()
            .set_rtp_mode(true)
            // .set_stats_interval(Some(Duration::from_millis(200)))
            // TODO: enable bwe
            .enable_bwe(Some(str0m::bwe::Bitrate::kbps(300)))
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            // enable for compatibility, some clients don't support remote ice-lite
            .set_ice_lite(false);
        rtc_config.set_initial_stun_rto(Duration::from_millis(200));
        rtc_config.set_max_stun_rto(Duration::from_millis(1500));
        rtc_config.set_max_stun_retransmits(5);
        let codec_config = rtc_config.codec_config();
        codec_config.add_config(
            PT_OPUS,
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                min_p_time: Some(10),
                use_inband_fec: Some(true),
                use_dtx: Some(true),
                ..Default::default()
            },
        );
        // codec_config.enable_vp8(true);
        // h264 as the lowest common denominator due to small clients like
        // embedded devices, smartphones, OBS only supports H264.
        // Baseline profile to ensure compatibility with all platforms.

        // Level 3.1 to 4.1. This is mainly to support clients that don't handle
        // level-asymmetry-allowed=true properly.
        // let baseline_levels = [0x1f, 0x20, 0x28, 0x29];
        let baseline_levels = [0x34]; // 5.2 level matching OpenH264
        let mut pt = 96; // start around 96–127 range for dynamic types

        for level in &baseline_levels {
            // // Baseline
            // codec_config.add_h264(
            //     pt.into(),
            //     Some((pt + 1).into()), // RTX PT
            //     true,
            //     0x420000 | level,
            // );
            // pt += 2;
            //
            // Constrained Baseline
            codec_config.add_h264(
                pt.into(),
                Some((pt + 1).into()), // RTX PT
                true,
                0x42e000 | level,
            );
            pt += 2;
        }
        // codec_config.enable_h264(true);

        // TODO: OBS only supports Baseline level 3.1
        // // ESP32-P4 supports up to 1080p@30fps
        // // https://components.espressif.com/components/espressif/esp_h264/versions/1.1.3/readme
        // // Baseline Level 4.0, (pt=127, rtx=121)
        // codec_config.add_h264(127.into(), Some(121.into()), true, 0x420028);
        // // Constrained Baseline Level 4.0, (pt=108, rtx=109)
        // codec_config.add_h264(108.into(), Some(109.into()), true, 0x42e028);

        let mut rtc = rtc_config.build(Instant::now().into());
        for s in sockets {
            let candidate = match s.transport() {
                Transport::Udp(_) => Candidate::builder()
                    .udp()
                    .host(s.local_addr())
                    .build()
                    .expect("a UDP host candidate"),
                Transport::Tcp => Candidate::builder()
                    .tcp()
                    .host(s.local_addr())
                    .tcptype(TcpType::Passive)
                    .build()
                    .expect("a TCP passive host candidate"),
            };

            rtc.add_local_candidate(candidate);
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(OfferRejectedReason::Rtc)?;
        Self::enforce_media_lines(&answer)?;

        tracing::debug!("{answer}");
        Ok((rtc, answer))
    }

    fn get_or_create_room(&mut self, room_id: &RoomId) -> &mut RoomEntry {
        if !self.rooms.contains_key(room_id) {
            tracing::info!("create_room: {}", room_id);
            // Create the watch channel upfront so participants can subscribe before
            // the room actor processes their AddParticipant message.
            let (watch_tx, _initial_rx) =
                tokio::sync::watch::channel(std::sync::Arc::new(room::TrackMap::new()));
            let track_watch = std::sync::Arc::new(watch_tx);
            // Assign this room to a core round-robin.  All participant tasks
            // spawned inside the room actor will inherit the same current_thread
            // runtime (via Handle::current()), so they are permanently pinned to
            // the same OS thread as their room — zero work-stealing possible.
            let handle_idx = self.room_counter % self.node_ctx.cpu_handles.len();
            let assigned_handle = self.node_ctx.cpu_handles[handle_idx].clone();
            self.room_counter += 1;

            let room_actor =
                room::RoomActor::new(self.node_ctx.clone(), room_id.clone(), track_watch.clone());
            let (room_handle, room_task) = actor::prepare(
                room_actor,
                actor::RunnerConfig::default().with_mailbox_cap(1024),
            );
            self.room_tasks.spawn_on(room_task, &assigned_handle);
            self.rooms.insert(
                room_id.clone(),
                RoomEntry {
                    handle: room_handle,
                    track_watch,
                },
            );
        } else {
            tracing::info!("get_room: {}", room_id);
        }
        self.rooms.get_mut(room_id).expect("just inserted")
    }

    fn enforce_media_lines(answer: &SdpAnswer) -> Result<(), OfferRejectedReason> {
        let mut video_recv_count = 0;
        let mut video_send_count = 0;
        let mut audio_recv_count = 0;
        let mut audio_send_count = 0;
        let mut data_channel_count = 0;

        for m in &answer.media_lines {
            let kind = m.typ.to_string();
            let dir = m.direction();
            let media_type = kind.as_str().into();

            if (dir == Direction::SendRecv || dir == Direction::Inactive) && kind != "application" {
                return Err(OfferRejectedReason::DirectionNotSupported(
                    MediaType::Application,
                ));
            }

            match (media_type, dir) {
                (MediaType::Video, Direction::RecvOnly) => {
                    video_recv_count += 1;
                    if video_recv_count > MAX_RECV_VIDEO_SLOTS {
                        return Err(OfferRejectedReason::SlotsLimit(
                            MediaType::Video,
                            Direction::SendOnly,
                            MAX_RECV_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Video, Direction::SendOnly) => {
                    video_send_count += 1;
                    if video_send_count > MAX_SEND_VIDEO_SLOTS {
                        return Err(OfferRejectedReason::SlotsLimit(
                            MediaType::Video,
                            Direction::RecvOnly,
                            MAX_SEND_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::RecvOnly) => {
                    audio_recv_count += 1;
                    if audio_recv_count > MAX_RECV_AUDIO_SLOTS {
                        return Err(OfferRejectedReason::SlotsLimit(
                            MediaType::Audio,
                            Direction::SendOnly,
                            MAX_RECV_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::SendOnly) => {
                    audio_send_count += 1;
                    if audio_send_count > MAX_SEND_AUDIO_SLOTS {
                        return Err(OfferRejectedReason::SlotsLimit(
                            MediaType::Audio,
                            Direction::RecvOnly,
                            MAX_SEND_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Application, dir) => {
                    data_channel_count += 1;
                    if data_channel_count > MAX_DATA_CHANNELS {
                        return Err(OfferRejectedReason::SlotsLimit(
                            MediaType::Application,
                            dir,
                            MAX_DATA_CHANNELS,
                        ));
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl ControllerActor {
    pub fn new(system_ctx: node::NodeContext, id: Arc<String>) -> Self {
        Self {
            id,
            node_ctx: system_ctx,
            rooms: HashMap::new(),
            room_tasks: JoinSet::new(),
            room_counter: 0,
        }
    }
}

pub type ControllerHandle = actor::ActorHandle<ControllerMessageSet>;
