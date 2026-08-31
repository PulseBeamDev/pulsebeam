mod audio;
mod data;
mod video;

use crate::entity::AudioOrigin;
use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::keys::TrackKey;
use crate::log::LogCtx;
use crate::participant::allocation::{AllocationInput, AllocationOutput, Bitrate};
use crate::participant::downstream::video::START_BANDWIDTH;
use crate::participant::event::ParticipantSink;
pub use crate::participant::intent::AudioIntent;
use crate::rtp::{
    Codec, CodecPayloadTypes, EncodingId as Rid, KeyframeRequest, MediaKind, MediaSectionId as Mid,
    MediaTime, PacketForwardingState, PayloadType as Pt, SequenceNumber as SeqNo, Ssrc,
};
use crate::track::{StreamWriter, Track, TrackMeta};
use ahash::HashSetExt;
pub use audio::DownstreamAudio;
pub(crate) use data::DownstreamData;
use indexmap::IndexMap;
use slotmap::SecondaryMap;
use tokio::time::Instant;
pub use video::{DownstreamVideo, INITIAL_BANDWIDTH};

#[derive(Clone)]
pub struct SlotConfig {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub payload_types: CodecPayloadTypes,
    pub kind: MediaKind,
}

impl Default for SlotConfig {
    fn default() -> Self {
        let mut payload_types = CodecPayloadTypes::default();
        payload_types.insert(Codec::H264, Pt::DEFAULT);
        Self {
            mid: Mid::from("0"),
            rid: None,
            ssrc: 0u32.into(),
            payload_types,
            kind: MediaKind::Video,
        }
    }
}

struct PlayoutDelayConfirm {
    mid: Mid,
    rid: Option<Rid>,
    seq: SeqNo,
}

pub struct Downstream {
    pub dirty_allocation: bool,
    pub video: DownstreamVideo,
    pub(crate) audio: DownstreamAudio,
    pub(crate) data: DownstreamData,
    catalog: SecondaryMap<TrackKey, TrackCatalogEntry>,
    /// Audio publications in the room, whether or not anyone is hearing them.
    ///
    /// The allocator claims slots dynamically and needs no registration to do
    /// it, but the roster does: a client cannot pin a speaker it was never told
    /// exists, and until someone is loud enough to be forwarded there is
    /// otherwise nothing that names them.
    audio_tracks: IndexMap<TrackId, TrackMeta>,

    available_bandwidth: Bitrate,
    last_desired: Bitrate,

    playout_delay: Option<(MediaTime, MediaTime)>,
    playout_delay_pending: bool,
    playout_delay_confirm: Option<PlayoutDelayConfirm>,
}

#[derive(Clone, Copy)]
pub(crate) struct TrackCatalogEntry {
    pub(crate) participant_id: ParticipantId,
    pub(crate) track_id: TrackId,
}

pub type DownstreamAllocator = Downstream;

impl Downstream {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool) -> Self {
        Self {
            video: DownstreamVideo::new(ctx, manual_sub),
            audio: DownstreamAudio::new(ctx, manual_sub),
            data: DownstreamData::new(),
            catalog: SecondaryMap::new(),
            audio_tracks: IndexMap::new(),
            dirty_allocation: false,

            available_bandwidth: START_BANDWIDTH,
            last_desired: video::START_BANDWIDTH,
            playout_delay: None,
            playout_delay_pending: false,
            playout_delay_confirm: None,
        }
    }

    pub(crate) fn add_track_candidate(
        &mut self,
        key: TrackKey,
        track: &Track,
        channels: &[(
            crate::participant::data::ChannelId,
            crate::track::DataTopicChannel,
        )],
    ) -> bool {
        let entry = TrackCatalogEntry {
            participant_id: track.meta().origin,
            track_id: track.id(),
        };
        if let Some(previous) = self.catalog.get(key) {
            debug_assert_eq!(previous.track_id, entry.track_id);
            debug_assert_eq!(previous.participant_id, entry.participant_id);
            return false;
        }
        let previous = self.catalog.insert(key, entry);
        debug_assert!(previous.is_none(), "a TrackKey must be installed once");
        self.data.add_candidate(key, track, channels);
        true
    }

    pub(crate) fn remove_track_candidate(&mut self, key: TrackKey) -> Option<TrackCatalogEntry> {
        self.data.remove_candidate(key);
        self.catalog.remove(key)
    }

    pub(crate) fn track_candidate(&self, key: TrackKey) -> Option<TrackCatalogEntry> {
        self.catalog.get(key).copied()
    }

    pub fn set_playout_delay(&mut self, bounds: Option<(u32, u32)>) {
        const MAX_HUNDREDTHS: u64 = 0xfff;
        let to_hundredths = |ms: u32| ((ms as u64).saturating_add(5) / 10).min(MAX_HUNDREDTHS);
        let Some(bounds) = bounds else {
            return;
        };
        let max = to_hundredths(bounds.1);
        let min = to_hundredths(bounds.0).min(max);
        let delay = (
            MediaTime::from_hundredths(min),
            MediaTime::from_hundredths(max),
        );
        if self.playout_delay == Some(delay) {
            return;
        }
        self.playout_delay = Some(delay);
        self.playout_delay_pending = true;
        self.playout_delay_confirm = None;
    }

    /// Returns the playout delay to stamp if the receiver has not yet confirmed
    /// receipt. Returns `None` once confirmed — extension is sticky so no need
    /// to keep sending unchanged values.
    #[inline]
    pub fn playout_delay_to_stamp(&self) -> Option<(MediaTime, MediaTime)> {
        if self.playout_delay_pending {
            self.playout_delay
        } else {
            None
        }
    }

    /// Record that a packet with the current playout delay values was stamped.
    /// Tracks the first such packet per change for RTCP confirmation.
    pub fn record_playout_delay_stamp(&mut self, mid: Mid, rid: Option<Rid>, seq: SeqNo) {
        if self.playout_delay_confirm.is_none() {
            self.playout_delay_confirm = Some(PlayoutDelayConfirm { mid, rid, seq });
        }
    }

    /// Called when RTCP receiver report stats arrive for a stream. Clears the
    /// pending flag once the remote has acknowledged receipt past our tracked seq.
    pub fn handle_egress_stats(&mut self, mid: Mid, rid: Option<Rid>, remote_max_seq: SeqNo) {
        let Some(confirm) = &self.playout_delay_confirm else {
            return;
        };
        if confirm.mid == mid && confirm.rid == rid && remote_max_seq >= confirm.seq {
            self.playout_delay_pending = false;
            self.playout_delay_confirm = None;
        }
    }

    pub(crate) fn install_track(&mut self, key: TrackKey, track: Track) {
        if track.kind() == TrackKind::Video {
            self.video.install_track(key, track);
            self.dirty_allocation = true;
            return;
        }
        // The allocator claims audio slots dynamically, so this registration is
        // purely so the roster can name the track before anyone hears it.
        let id = track.id();
        self.audio_tracks.insert(id, track.meta().clone());
    }

    pub(crate) fn activate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.activate_track_binding(key, track_id);
        }
    }

    pub(crate) fn deactivate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.deactivate_track_binding(key, track_id);
        }
    }

    /// Apply the client's audio selection policy.
    pub fn set_audio_intent(&mut self, intent: AudioIntent) {
        self.audio.set_intent(intent);
    }

    pub(crate) fn apply_signaling_intents(
        &mut self,
        intents: crate::participant::signaling::SignalingIntents,
    ) {
        if let Some(video) = intents.video {
            self.video.configure(&video);
        }
        if let Some(audio) = intents.audio {
            self.set_audio_intent(audio);
        }
        if intents.playout_delay.is_some() {
            self.set_playout_delay(intents.playout_delay);
        }
        self.dirty_allocation = true;
    }

    pub(crate) fn signaling_snapshot(&self) -> crate::participant::signaling::SignalingSnapshot {
        use crate::participant::signaling::{
            SignalingAudioBinding, SignalingSnapshot, SignalingVideoBinding,
        };
        SignalingSnapshot {
            publications: self
                .video
                .tracks()
                .chain(self.audio_tracks.values())
                .cloned()
                .collect(),
            participants: ahash::HashSet::new(),
            video: self
                .video
                .slots()
                .map(|slot| SignalingVideoBinding {
                    mid: slot.mid.to_string(),
                    track_id: slot.track.id.as_str(),
                    paused: slot.paused,
                })
                .collect(),
            audio: self
                .audio_assignments()
                .iter()
                .map(|heard| SignalingAudioBinding {
                    mid: heard.mid.to_string(),
                    track_id: heard.origin.track.as_str(),
                    level_dbov: i32::from(heard.level_dbov),
                })
                .collect(),
        }
    }

    pub fn audio_slot_count(&self) -> usize {
        self.audio.slot_count()
    }

    pub(super) fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let removed = self.video.remove_track(track_id);
        if removed {
            self.dirty_allocation = true;
        }
        // Audio too, and not folded into `removed`: that flag drives the *video* allocator's
        // rebalance. A speaker leaving still has to stop being announced, or the room keeps a
        // tile for somebody who is not in it.
        let audio_removed = self.audio.remove_track(track_id);
        let announced = self.audio_tracks.shift_remove(track_id).is_some();
        removed || audio_removed || announced
    }

    pub fn add_slot(&mut self, slot: SlotConfig) {
        match slot.kind {
            MediaKind::Video => {
                self.video.add_slot(slot);
            }
            MediaKind::Audio => {
                self.audio.add_slot(slot);
            }
        }
        self.dirty_allocation = true;
    }

    pub fn has_slot(&self, kind: MediaKind, mid: Mid) -> bool {
        match kind {
            MediaKind::Video => self.video.has_slot(mid),
            MediaKind::Audio => self.audio.has_slot(mid),
        }
    }

    pub fn refresh_ssrc(
        &mut self,
        kind: MediaKind,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
    ) -> bool {
        match kind {
            MediaKind::Video => self.video.refresh_ssrc(mid, rid, ssrc),
            MediaKind::Audio => {
                debug_assert!(rid.is_none());
                self.audio.refresh_ssrc(mid, ssrc)
            }
        }
    }

    pub fn update_allocation_input(&mut self, _now: Instant, input: AllocationInput) {
        self.available_bandwidth = input.estimate;
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, _now: Instant) -> (bool, AllocationOutput) {
        self.dirty_allocation = false;
        let (desired, assignments_changed, _unfunded) =
            self.video.update_allocations(self.available_bandwidth);
        let allocated = self.video.current_allocation();
        self.last_desired = desired;
        (assignments_changed, AllocationOutput { desired, allocated })
    }

    pub(crate) fn reconcile_routes(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        self.video.poll_slow(now, self.available_bandwidth, events);
    }

    pub(crate) fn poll_slow(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> (bool, AllocationOutput) {
        let (assignments_changed, output) = self.update_allocations(now);
        self.video.poll_slow(now, self.available_bandwidth, events);
        (assignments_changed, output)
    }

    pub fn on_forward_rtp(
        &mut self,
        track_key: TrackKey,
        arrival_ts: Instant,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp(track_key, arrival_ts, cache, writer)
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        origin: AudioOrigin,
        pkt: &PacketForwardingState,
        media: &crate::participant::ForwardPacket,
        audio_level_extension: Option<u8>,
        writer: &mut StreamWriter,
    ) {
        self.audio
            .on_rtp_with_media(origin, pkt, media, audio_level_extension, writer);
    }

    /// Whether someone new took over an audio slot since this was last asked.
    pub fn take_audio_speakers_changed(&mut self) -> bool {
        self.audio.take_speakers_changed()
    }

    /// Who this subscriber is currently hearing, loudest first.
    pub fn audio_assignments(&self) -> Vec<crate::participant::downstream::audio::Heard> {
        self.audio.assignments()
    }

    pub fn handle_keyframe_request(
        &mut self,
        req: KeyframeRequest,
    ) -> Option<(TrackKey, Option<Rid>)> {
        self.video.handle_keyframe_request(req)
    }
}
