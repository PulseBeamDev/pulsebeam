use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use crate::{
    control::{controller::ControllerCommand, registry::RoomRegistry, room::Room},
    entity::{ParticipantId, RoomId},
    shard::worker::ShardEvent,
};
use futures_lite::StreamExt;
use indexmap::IndexMap;
use str0m::{
    Candidate, RtcError,
    media::{Direction, MediaKind},
};

pub const MAX_RECV_VIDEO_SLOTS: usize = 1;
pub const MAX_RECV_AUDIO_SLOTS: usize = 1;
pub const MAX_SEND_VIDEO_SLOTS: usize = 16;
pub const MAX_SEND_AUDIO_SLOTS: usize = 5;
pub const MAX_DATA_CHANNELS: usize = 1;
/// Maximum participants allowed per "slot" before hashing to a new shard epoch.
const MAX_PARTICIPANTS_PER_SHARD_SLOT: usize = 16;
const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub enum ControllerEvent {}

pub struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
}

impl ControllerEventQueue {
    pub fn default() -> Self {
        Self {
            queue: VecDeque::with_capacity(1024),
        }
    }

    pub fn push(&mut self, ev: ControllerEvent) {
        self.queue.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }
}

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

#[derive(thiserror::Error, Debug)]
pub enum OfferRejectedReason {
    #[error("RTC engine error: {0}")]
    Rtc(#[from] RtcError),
    #[error("{0} {1} slots limit exceeded (max {2})")]
    SlotsLimit(MediaType, Direction, usize),
    #[error("SendRecv direction is not supported for {0}")]
    DirectionNotSupported(MediaType),
}

struct ParticipantMeta {
    shard_id: usize,
    room_id: RoomId,
}

pub struct ControllerCore {
    candidates: Vec<Candidate>,
    registry: RoomRegistry,
}

impl ControllerCore {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            registry: RoomRegistry::new(),
        }
    }

    pub async fn next_expired(&mut self) {
        self.registry.next_expired().await;
    }

    pub fn process_shard_event(&mut self, ev: ShardEvent, eq: &mut ControllerEventQueue) {
        match ev {
            ShardEvent::TrackPublished(track) => {
                let origin = track.meta.origin;
                let Some(room) = self.registry.room_mut_for(&track.meta.origin) else {
                    return;
                };

                // TODO: make room shard aware?
                let mut shard_ids: IndexMap<usize, ()> = IndexMap::new();
                for participant_id in room.participants_iter() {
                    if *participant_id == origin {
                        continue;
                    }
                    if let Some(p) = self.registry.get(participant_id) {
                        shard_ids.entry(p.shard_id).or_default();
                    }
                }

                tracing::info!(
                    track = %track.meta.id,
                    %origin,
                    room_id = ?room_id,
                    shard_count = shard_ids.len(),
                    "fanning out track to shards"
                );
                room.publish_track(track.clone());
                for (shard_id, _) in shard_ids {
                    self.router
                        .send(shard_id, ShardCommand::PublishTrack(track.clone(), room_id))
                        .await;
                }
            }

            ShardEvent::ParticipantExited(participant_id) => {
                self.delete_participant(&participant_id).await;
            }
            ShardEvent::KeyframeRequest(req) => {
                let meta = self.registry.get(&req.origin).or_else(|| {
                    tracing::warn!(origin = %req.origin, track = ?req.stream_id.0, "KeyframeRequest: origin participant not found in controller");
                    None
                })?;
                self.router
                    .send(meta.shard_id, ShardCommand::RequestKeyframe(req))
                    .await;
            }
        }

        Some(())
    }

    pub fn process_command(&mut self, cmd: ControllerCommand, eq: &mut ControllerEventQueue) {
        match cmd {
            ControllerCommand::CreateParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .await
                    .map(|res| CreateParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }

            ControllerCommand::DeleteParticipant(m) => {
                self.delete_participant(&m.participant_id).await;
            }
            ControllerCommand::PatchParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .await
                    .map(|res| PatchParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }
        }
    }

    pub async fn handle_create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let answer = self.create_participant(state, offer).await?;
        Ok(answer)
    }

    async fn create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let (mut rtc, answer) = self.create_answer(offer)?;
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag.clone();
        let room = self
            .rooms
            .entry(state.room_id)
            .or_insert_with(|| Room::new(state.room_id));
        let tracks = room.tracks_for(&state.participant_id);
        // TODO: handle patch
        let epoch = room.participant_count() / MAX_PARTICIPANTS_PER_SHARD_SLOT;
        let routing_key = format!("{}-{}", state.room_id, epoch);
        let participant_id = state.participant_id;
        let shard_id = self
            .router
            .try_route(routing_key)
            .ok_or(ControllerError::ServiceUnavailable)?;
        let cfg = ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id,
            rtc,
            available_tracks: tracks.cloned().collect(),
        };
        tracing::info!("routed {} to {}", participant_id, shard_id);
        self.router
            .send(shard_id, ShardCommand::AddParticipant(cfg))
            .await;
        self.router
            .broadcast(|| ShardCommand::RegisterParticipant {
                participant_id,
                shard_id,
                ufrag: ufrag.clone(),
            })
            .await;

        room.add_participant(&participant_id);
        self.participants.insert(
            participant_id,
            ParticipantMeta {
                shard_id,
                room_id: state.room_id,
            },
        );
        Ok(answer)
    }

    async fn delete_participant(&mut self, participant_id: &ParticipantId) {
        let Some(meta) = self.participants.remove(participant_id) else {
            return;
        };
        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remove_participant(participant_id);
            if room.participant_count() == 0 {
                self.sweeper.insert(meta.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        self.router
            .broadcast(|| ShardCommand::UnregisterParticipant {
                participant_id: *participant_id,
            })
            .await;
    }

    fn maybe_delete_room(&mut self, room_id: &RoomId) {
        if let Some(room) = self.rooms.get(room_id) {
            // someone may have joined after the timer started.
            if room.participant_count() == 0 {
                tracing::info!(%room_id, "removing empty room after grace period");
                self.rooms.remove(room_id);
            }
        }
    }

    fn create_answer(&mut self, offer: SdpOffer) -> Result<(Rtc, SdpAnswer), ControllerError> {
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
        for c in &self.candidates {
            rtc.add_local_candidate(c.clone());
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(OfferRejectedReason::Rtc)?;
        Self::enforce_media_lines(&answer)?;

        tracing::debug!("{answer}");
        Ok((rtc, answer))
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
