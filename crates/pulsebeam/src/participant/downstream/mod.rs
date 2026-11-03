mod audio;
mod data;
mod video;

use std::time::Duration;

use crate::bitrate::Bitrate;
use crate::entity::AudioOrigin;
use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::keys::TrackHandle;
use crate::log::{LogCtx, plog_trace};
use crate::media::{KeyframeRequest, MediaKind, Mid, Pt, Rid, SeqNo, Ssrc};
use crate::participant::downstream::video::START_BANDWIDTH;
use crate::participant::event::ParticipantSink;
pub use crate::participant::intent::AudioIntent;
use crate::rtp::MediaTime;
use crate::rtp::RtpPacket;
use crate::track::{StreamWriter, Track, TrackLayer, TrackMeta};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
pub use audio::DownstreamAudio;
pub(crate) use data::DownstreamData;
use indexmap::IndexMap;
use slotmap::SecondaryMap;
use tokio::time::Instant;
pub use video::DownstreamVideo;

#[derive(Clone)]
pub struct SlotConfig {
    pub media_index: u32,
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub pt: Pt,
    pub kind: MediaKind,
}

#[cfg(test)]
impl Default for SlotConfig {
    fn default() -> Self {
        Self {
            media_index: 0,
            mid: Mid::from("0"),
            rid: None,
            ssrc: 0u32.into(),
            pt: 100u8.into(),
            kind: MediaKind::Video,
        }
    }
}

const BWE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(150);
const BWE_FALL_TIME_CONSTANT: Duration = Duration::from_millis(800);

#[derive(Debug)]
struct BweFilter {
    filtered_bps: f64,
    target_bps: f64,
    last_update: Option<Instant>,
}

impl BweFilter {
    fn new(initial: Bitrate) -> Self {
        Self {
            filtered_bps: initial.as_f64(),
            target_bps: initial.as_f64(),
            last_update: None,
        }
    }

    fn tick(&mut self, now: Instant) {
        self.advance(now, self.target_bps);
    }

    fn update(&mut self, now: Instant, raw: Bitrate) {
        self.target_bps = raw.as_f64();
        self.advance(now, self.target_bps);
    }

    fn advance(&mut self, now: Instant, target_bps: f64) {
        debug_assert!(target_bps.is_finite());
        debug_assert!(target_bps >= 0.0);
        let Some(last_update) = self.last_update else {
            self.last_update = Some(now);
            self.filtered_bps = target_bps;
            return;
        };

        debug_assert!(now >= last_update);
        let elapsed = now.saturating_duration_since(last_update);
        self.last_update = Some(now);
        if target_bps >= self.filtered_bps {
            let alpha = (-elapsed.as_secs_f64() / BWE_RISE_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = target_bps + (self.filtered_bps - target_bps) * alpha;
        } else {
            let alpha = (-elapsed.as_secs_f64() / BWE_FALL_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = target_bps + (self.filtered_bps - target_bps) * alpha;
        }
        debug_assert!(self.filtered_bps.is_finite());
        debug_assert!(self.filtered_bps >= 0.0);
    }

    fn current(&self) -> Bitrate {
        Bitrate::from(crate::bitrate::saturating_bps(self.filtered_bps))
    }
}

struct PlayoutDelayConfirm {
    rid: Option<Rid>,
    seq: SeqNo,
}

struct FixedReceiver {
    bounds: (MediaTime, MediaTime),
    pending: bool,
    confirm: Option<PlayoutDelayConfirm>,
}

pub struct Downstream {
    ctx: LogCtx,
    pub dirty_allocation: bool,
    pub video: DownstreamVideo,
    pub(crate) audio: DownstreamAudio,
    pub(crate) data: DownstreamData,
    catalog: SecondaryMap<TrackHandle, TrackCatalogEntry>,
    /// Audio publications in the room, whether or not anyone is hearing them.
    ///
    /// The allocator claims slots dynamically and needs no registration to do
    /// it, but the roster does: a client cannot pin a speaker it was never told
    /// exists, and until someone is loud enough to be forwarded there is
    /// otherwise nothing that names them.
    audio_tracks: IndexMap<TrackId, TrackMeta>,

    available_bandwidth: BweFilter,
    last_desired: Bitrate,
    /// When forwarding fell below the rate needed to keep measuring the link, while demand
    /// exists, and the estimate it was at then.
    desired_playout: HashMap<TrackId, (MediaTime, MediaTime)>,
    fixed_receivers: HashMap<Mid, FixedReceiver>,
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
            ctx,
            video: DownstreamVideo::new(ctx, manual_sub),
            audio: DownstreamAudio::new(ctx, manual_sub),
            data: DownstreamData::new(),
            catalog: SecondaryMap::new(),
            audio_tracks: IndexMap::new(),
            dirty_allocation: false,

            available_bandwidth: BweFilter::new(START_BANDWIDTH),
            last_desired: video::START_BANDWIDTH,
            desired_playout: HashMap::new(),
            fixed_receivers: HashMap::new(),
        }
    }

    pub(crate) fn add_track_candidate(
        &mut self,
        key: TrackHandle,
        track: &Track,
        channels: &[(crate::media::ChannelId, crate::track::DataTopicChannel)],
    ) -> bool {
        if !self.register_candidate(key, track) {
            return false;
        }
        self.data.add_candidate(key, track, channels);
        true
    }

    pub(crate) fn register_candidate(&mut self, key: TrackHandle, track: &Track) -> bool {
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
        debug_assert!(previous.is_none(), "a TrackHandle must be installed once");
        true
    }

    pub(crate) fn remove_track_candidate(&mut self, key: TrackHandle) -> Option<TrackCatalogEntry> {
        self.data.remove_candidate(key);
        self.catalog.remove(key)
    }

    pub(crate) fn track_candidate(&self, key: TrackHandle) -> Option<TrackCatalogEntry> {
        self.catalog.get(key).copied()
    }

    /// A fixed receiver cannot return to native defaults during this transport generation.
    /// The caller must send Reconnect and suppress the incompatible packet.
    pub fn playout_delay_to_stamp(
        &mut self,
        track_id: TrackId,
        mid: Mid,
    ) -> Result<Option<(MediaTime, MediaTime)>, ()> {
        let Some(&bounds) = self.desired_playout.get(&track_id) else {
            return if self.fixed_receivers.contains_key(&mid) {
                Err(())
            } else {
                Ok(None)
            };
        };
        let receiver = self.fixed_receivers.entry(mid).or_insert(FixedReceiver {
            bounds,
            pending: true,
            confirm: None,
        });
        if receiver.bounds != bounds {
            receiver.bounds = bounds;
            receiver.pending = true;
            receiver.confirm = None;
        }
        let stamp = receiver.pending.then_some(bounds);
        if track_id.kind() == TrackKind::Audio {
            self.audio.mark_fixed_receiver(mid);
        }
        Ok(stamp)
    }

    pub fn record_playout_delay_stamp(&mut self, mid: Mid, rid: Option<Rid>, seq: SeqNo) {
        if let Some(receiver) = self.fixed_receivers.get_mut(&mid)
            && receiver.confirm.is_none()
        {
            receiver.confirm = Some(PlayoutDelayConfirm { rid, seq });
        }
    }

    pub fn handle_egress_stats(&mut self, mid: Mid, rid: Option<Rid>, remote_max_seq: SeqNo) {
        let Some(receiver) = self.fixed_receivers.get_mut(&mid) else {
            return;
        };
        let Some(confirm) = &receiver.confirm else {
            return;
        };
        if confirm.rid == rid && remote_max_seq >= confirm.seq {
            receiver.pending = false;
            receiver.confirm = None;
        }
    }

    pub(crate) fn install_track(&mut self, key: TrackHandle, track: Track) {
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

    pub(crate) fn activate_track_binding(&mut self, key: TrackHandle, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.activate_track_binding(key, track_id);
        }
    }

    pub(crate) fn deactivate_track_binding(&mut self, key: TrackHandle, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.deactivate_track_binding(key, track_id);
        }
    }

    /// Apply the client's audio selection policy.
    pub fn set_audio_intent(&mut self, intent: AudioIntent) {
        self.audio.set_intent(intent);
    }

    pub(crate) fn native_publications(&self) -> Vec<crate::track::TrackMeta> {
        self.video
            .tracks()
            .chain(self.audio_tracks.values())
            .chain(self.data.publications())
            .cloned()
            .collect()
    }

    pub(crate) fn native_assignments(&self) -> (Vec<(u32, TrackId)>, Vec<(u32, TrackId)>) {
        let video = self.video.native_mapping_slots().collect();
        let audio = self
            .audio_assignments()
            .into_iter()
            .filter_map(|heard| {
                self.audio
                    .receiver_index(heard.mid)
                    .map(|index| (index, heard.origin.track))
            })
            .collect();
        (video, audio)
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

    pub(crate) fn receiver_index(&self, kind: MediaKind, mid: Mid) -> Option<u32> {
        match kind {
            MediaKind::Video => self.video.receiver_index(mid),
            MediaKind::Audio => self.audio.receiver_index(mid),
        }
    }

    pub fn update_bitrate(&mut self, now: Instant, available_bandwidth: Bitrate) {
        self.available_bandwidth.update(now, available_bandwidth);
        plog_trace!(self.ctx, raw=%available_bandwidth, filtered=%self.available_bandwidth.current(), "downstream bandwidth sample");
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, now: Instant) -> bool {
        self.available_bandwidth.tick(now);
        self.dirty_allocation = false;
        let (desired, assignments_changed, _) = self
            .video
            .update_allocations(now, self.available_bandwidth.current());
        self.last_desired = desired;
        assignments_changed
    }

    pub(crate) fn desired_bitrate(&self) -> Bitrate {
        self.last_desired
    }

    pub(crate) fn reconcile_routes(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
    }

    pub(crate) fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) -> bool {
        let assignments_changed = self.update_allocations(now);
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
        assignments_changed
    }

    pub fn on_forward_rtp(
        &mut self,
        track_handle: TrackHandle,
        arrival_ts: Instant,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp(track_handle, arrival_ts, cache, writer)
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        origin: AudioOrigin,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.audio.on_rtp(origin, pkt, writer);
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
        now: Instant,
        req: KeyframeRequest,
    ) -> Option<(TrackHandle, &TrackLayer)> {
        self.video.handle_keyframe_request(now, req)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    #[test]
    fn fixed_playout_is_per_receiver_and_defaults_require_a_fresh_mid() {
        let mut downstream = Downstream::new(
            LogCtx {
                room_id: crate::entity::RoomId::from_external(
                    &crate::entity::RoomExternalId::new("room").unwrap(),
                ),
                participant_id: ParticipantId::new(),
            },
            false,
        );
        let fixed = crate::entity::ConnectionId::new().derive_track_id(TrackKind::Video, "fixed");
        let default =
            crate::entity::ConnectionId::new().derive_track_id(TrackKind::Video, "default");
        let first = Mid::from("0");
        let fresh = Mid::from("1");
        let bounds = (
            MediaTime::from_hundredths(200),
            MediaTime::from_hundredths(200),
        );
        downstream.desired_playout.insert(fixed, bounds);
        assert_eq!(
            downstream.playout_delay_to_stamp(fixed, first),
            Ok(Some(bounds))
        );
        downstream.record_playout_delay_stamp(first, None, SeqNo::from(7u64));
        downstream.handle_egress_stats(first, None, SeqNo::from(8u64));
        assert_eq!(downstream.playout_delay_to_stamp(fixed, first), Ok(None));
        downstream.desired_playout.clear();
        assert_eq!(downstream.playout_delay_to_stamp(default, first), Err(()));
        assert_eq!(downstream.playout_delay_to_stamp(default, fresh), Ok(None));
        let zero = (MediaTime::from_hundredths(0), MediaTime::from_hundredths(0));
        downstream.desired_playout.insert(default, zero);
        assert_eq!(
            downstream.playout_delay_to_stamp(default, first),
            Ok(Some(zero))
        );
    }

    fn expected(initial: f64, target: f64, elapsed: Duration, time_constant: Duration) -> f64 {
        let alpha = (-elapsed.as_secs_f64() / time_constant.as_secs_f64()).exp();
        target + (initial - target) * alpha
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 2.0,
            "actual={actual}, expected={expected}"
        );
    }

    #[test]
    fn bwe_filter_rise_uses_each_update_interval_once() {
        let start = Instant::now();
        let initial = 300_000.0;
        let target = 1_300_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(200),
                BWE_RISE_TIME_CONSTANT,
            ),
        );
    }

    #[test]
    fn bwe_filter_fall_uses_each_update_interval_once() {
        let start = Instant::now();
        let initial = 2_000_000.0;
        let target = 300_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(200),
                BWE_FALL_TIME_CONSTANT,
            ),
        );
    }

    #[test]
    fn bwe_filter_ticks_converge_to_the_latest_sample() {
        let start = Instant::now();
        let initial = 300_000.0;
        let target = 4_200_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        for elapsed_ms in (200..=1_000).step_by(100) {
            filter.tick(start + Duration::from_millis(elapsed_ms));
        }

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(1_000),
                BWE_RISE_TIME_CONSTANT,
            ),
        );
    }
}
