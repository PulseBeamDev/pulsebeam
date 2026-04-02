use pulsebeam_runtime::sync::Arc;
use std::fmt::{Debug, Display};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pulsebeam_runtime::unsync::spmc;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use tokio::sync::Notify;
use tokio::time::Instant;

#[cfg(test)]
use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};
use str0m::rtp::rtcp::SenderInfo;

pub type StreamId = (TrackId, Option<Rid>);

/// Leading-edge debounce interval for keyframe requests forwarded upstream.
pub const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);
pub const MAX_SIMULCAST_LAYERS: usize = 3;

/// Sends keyframe requests from a [`SimulcastReceiver`] to the upstream publisher.
///
/// Always requests PLI — the publisher decides the exact RTCP feedback type.
#[derive(Clone, Debug)]
pub struct KeyframeRequester {
    signal: Arc<AtomicBool>,
    notify: Arc<Notify>,
    #[cfg(test)]
    pub request_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl KeyframeRequester {
    /// Signal that a PLI keyframe is needed.
    pub fn request(&self) {
        self.signal.store(true, Ordering::Relaxed);
        self.notify.notify_one();
        #[cfg(test)]
        {
            self.request_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Polls a pending keyframe request with leading-edge debounce.
///
/// Owned by `UpstreamAllocator`; the corresponding [`KeyframeRequester`] is held
/// by each [`SimulcastReceiver`].
#[derive(Debug)]
pub struct KeyframePoll {
    signal: Arc<AtomicBool>,
    pub mid: Mid,
    pub rid: Option<Rid>,
    debounce: Duration,
    last_sent: Option<Instant>,
}

impl KeyframePoll {
    /// Return a pending PLI [`KeyframeRequest`] if one is flagged and debounce permits.
    ///
    /// Leading-edge semantics: the first request in any window passes through
    /// immediately; subsequent requests within the same window are discarded.
    pub fn take_pending(&mut self, now: Instant) -> Option<KeyframeRequest> {
        if !self.signal.load(Ordering::Relaxed) {
            return None;
        }
        self.signal.store(false, Ordering::Relaxed);
        // Within debounce window: discard.
        if let Some(last) = self.last_sent
            && now.duration_since(last) < self.debounce
        {
            return None;
        }
        self.last_sent = Some(now);
        Some(KeyframeRequest {
            kind: KeyframeRequestKind::Pli,
            rid: self.rid,
            mid: self.mid,
        })
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SimulcastQuality {
    Low = 1,
    Medium = 2,
    High = 3,
}

impl std::fmt::Debug for SimulcastQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        };
        f.write_str(txt)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct TrackMeta {
    pub id: crate::entity::TrackId,
    pub origin_participant: crate::entity::ParticipantId,
    pub kind: MediaKind,
    pub simulcast_rids: arrayvec::ArrayVec<Rid, MAX_SIMULCAST_LAYERS>,
}

#[derive(Clone)]
pub struct SimulcastReceiver {
    pub meta: TrackMeta,
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub channel: spmc::Receiver<RtpPacket>,
    pub keyframe_requester: KeyframeRequester,
    pub state: StreamState,
}

impl PartialEq for SimulcastReceiver {
    fn eq(&self, other: &Self) -> bool {
        other.meta == self.meta && other.rid == self.rid && other.quality == self.quality
    }
}

impl Debug for SimulcastReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self, f)
    }
}

impl Display for SimulcastReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{}:{}",
            self.meta.id,
            self.rid.as_deref().unwrap_or("_")
        ))
    }
}

impl SimulcastReceiver {
    pub fn request_keyframe(&self) {
        self.keyframe_requester.request();
    }
}

#[derive(Debug)]
pub struct SimulcastSender {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub monitor: StreamMonitor,
    synchronizer: Synchronizer,
    channel: spmc::Sender<RtpPacket>,
    filter: PacketFilter,
}

impl PartialEq for SimulcastSender {
    fn eq(&self, other: &Self) -> bool {
        self.mid == other.mid && self.rid == other.rid
    }
}

impl Eq for SimulcastSender {}

impl SimulcastSender {
    pub fn poll_stats(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.monitor.poll(now, is_any_sibling_active);
    }

    pub fn forward(&mut self, mut pkt: RtpPacket, sr: Option<SenderInfo>) {
        // Synchronizer stamps playout_time in-place — no move-in/move-out round trip.
        self.synchronizer.process(&mut pkt, sr);
        self.monitor
            .process_packet(&pkt, pkt.payload.len() + pkt.header_len);
        if (self.filter)(&pkt) {
            // Move packet into the broadcast ring — zero copies, zero arc bumps.
            self.channel.send(pkt);
        }
    }
}

pub struct TrackSender {
    pub meta: TrackMeta,
    pub simulcast: Vec<SimulcastSender>,
    /// Shared notification handle; set by subscribers via [`KeyframeRequester`].
    pub keyframe_notify: Arc<Notify>,
    /// Per-simulcast-layer poll handles, drained by the upstream actor.
    pub keyframe_polls: Vec<KeyframePoll>,
}

impl PartialEq for TrackSender {
    fn eq(&self, other: &Self) -> bool {
        self.meta == other.meta && self.simulcast == other.simulcast
    }
}

impl Eq for TrackSender {}

impl TrackSender {
    pub fn forward(&mut self, rid: Option<&Rid>, packet: RtpPacket, sr: Option<SenderInfo>) {
        let sender = self
            .simulcast
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always be available");
        sender.forward(packet, sr);
    }

    pub fn by_rid_mut(&mut self, rid: &Option<Rid>) -> Option<&mut SimulcastSender> {
        self.simulcast.iter_mut().find(|s| s.rid == *rid)
    }

    pub fn poll_stats(&mut self, now: Instant) {
        let total_active_streams = self
            .simulcast
            .iter()
            .filter(|s| !s.monitor.shared_state().is_inactive())
            .count();

        for layer in self.simulcast.iter_mut() {
            let is_current_layer_active = !layer.monitor.shared_state().is_inactive();
            let is_any_sibling_active = if is_current_layer_active {
                total_active_streams > 1
            } else {
                total_active_streams > 0
            };

            layer.poll_stats(now, is_any_sibling_active);
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: TrackMeta,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    pub fn by_quality(&self, quality: SimulcastQuality) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|s| s.quality == quality)
    }

    pub fn higher_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        self.simulcast
            .iter()
            // Find layers strictly greater than current
            .filter(|s| s.quality > current)
            // Pick the smallest one that is still better than current
            // (e.g., if you are at Low and have Med/High available, pick Med)
            .min_by_key(|s| s.quality)
    }

    pub fn lower_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        self.simulcast
            .iter()
            // Find layers strictly less than current
            .filter(|s| s.quality < current)
            // Pick the largest one that is still below current
            .max_by_key(|s| s.quality)
    }

    pub fn lowest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .iter()
            .min_by_key(|s| s.quality)
            .expect("TrackReceiver must have at least one layer")
    }

    pub fn highest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .iter()
            .max_by_key(|s| s.quality)
            .expect("TrackReceiver must have at least one layer")
    }
}

pub fn new(mid: Mid, meta: TrackMeta) -> (TrackSender, TrackReceiver) {
    const BASE_CAP: usize = 128;

    let simulcast_rids: Vec<Option<Rid>> = if meta.simulcast_rids.is_empty() {
        vec![None]
    } else {
        meta.simulcast_rids.iter().map(|rid| Some(*rid)).collect()
    };

    let keyframe_notify = Arc::new(Notify::new());
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    let mut keyframe_polls = Vec::new();

    for (index, &rid) in simulcast_rids.iter().enumerate() {
        // TODO: get this from SDP instead
        let (clock_rate, filter) = match meta.kind {
            MediaKind::Audio => (rtp::AUDIO_FREQUENCY, should_forward_audio as PacketFilter),
            MediaKind::Video => (rtp::VIDEO_FREQUENCY, should_forward_noop as PacketFilter),
        };

        let (quality, bitrate, cap_tier) = match (meta.kind, rid, index) {
            (MediaKind::Audio, _, _) => (SimulcastQuality::Low, 64_000, 1),
            (MediaKind::Video, None, _) => (SimulcastQuality::Low, 500_000, 4),

            // Ordering here determines the highest quality first.
            // https://datatracker.ietf.org/doc/html/rfc8853#section-5.2
            // https://github.com/obsproject/obs-studio/pull/10885
            (MediaKind::Video, Some(_), 0) => (SimulcastQuality::High, 2_500_000, 4),
            (MediaKind::Video, Some(_), 1) => (SimulcastQuality::Medium, 750_000, 2),
            (MediaKind::Video, Some(_), _) => (SimulcastQuality::Low, 150_000, 1),
        };

        let (tx, rx) = spmc::channel(BASE_CAP * cap_tier);
        // Shared atomic signal: receiver writes, poll-side reads.
        let signal = Arc::new(AtomicBool::new(false));

        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.kind, stream_id, stream_state.clone());

        // Only video tracks need keyframe polling.
        if meta.kind == MediaKind::Video {
            keyframe_polls.push(KeyframePoll {
                signal: signal.clone(),
                mid,
                rid,
                debounce: KEYFRAME_DEBOUNCE,
                last_sent: None,
            });
        }

        senders.push(SimulcastSender {
            mid,
            rid,
            quality,
            filter,
            synchronizer: Synchronizer::new(clock_rate),
            channel: tx,
            monitor,
        });
        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            quality,
            rid,
            channel: rx,
            keyframe_requester: KeyframeRequester {
                signal,
                notify: keyframe_notify.clone(),
                #[cfg(test)]
                request_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            state: stream_state,
        });
    }
    senders.sort_by_key(|e| std::cmp::Reverse(e.quality));
    receivers.sort_by_key(|e| std::cmp::Reverse(e.quality));

    tracing::info!("discovered simulcast mapping: {:?}", receivers);

    (
        TrackSender {
            meta: meta.clone(),
            simulcast: senders,
            keyframe_notify,
            keyframe_polls,
        },
        TrackReceiver {
            meta,
            simulcast: receivers,
        },
    )
}

type PacketFilter = fn(packet: &RtpPacket) -> bool;

/// Determines if an audio packet is essential (Speech or DTX) and should be forwarded.
#[inline]
pub fn should_forward_audio(packet: &RtpPacket) -> bool {
    const DTX_THRESHOLD: usize = 12;
    // -50dBov is a standard noise floor. Anything quieter is background hiss.
    const NOISE_FLOOR_DB: i8 = -50;

    // 1. Always relay tiny packets (Comfort Noise / DTX)
    if packet.payload.len() < DTX_THRESHOLD {
        return true;
    }

    let ext = &packet.ext_vals;

    // 2. Strict VAD Check (Priority)
    // If the V bit is present, trust it explicitly.
    if let Some(vad) = ext.voice_activity {
        return vad;
    }

    // 3. Noise Gate Fallback
    // If VAD is missing but we have levels, drop silence/background noise.
    if let Some(level) = ext.audio_level {
        return level > NOISE_FLOOR_DB;
    }

    // 4. Fallback: No metadata, assumed active.
    true
}

#[inline]
fn should_forward_noop(_: &RtpPacket) -> bool {
    true
}

#[cfg(test)]
pub mod test_utils {
    use super::*;

    pub fn make_track(
        participant_id: ParticipantId,
        kind: MediaKind,
        mid: Mid,
        rids: Option<Vec<Rid>>,
    ) -> (TrackSender, TrackReceiver) {
        let track_id = participant_id.derive_track_id(kind, &mid);
        let rids = rids.map(|rid| rid).iter().collect();
        let meta = TrackMeta {
            id: track_id,
            origin_participant: participant_id,
            kind,
            simulcast_rids: rids,
        };

        crate::track::new(mid, meta)
    }

    pub fn make_video_track(
        participant_id: ParticipantId,
        mid: Mid,
    ) -> (TrackSender, TrackReceiver) {
        make_track(
            participant_id,
            MediaKind::Video,
            mid,
            Some(vec!["f".into(), "h".into(), "q".into()]),
        )
    }

    pub fn make_audio_track(
        participant_id: ParticipantId,
        mid: Mid,
    ) -> (TrackSender, TrackReceiver) {
        make_track(participant_id, MediaKind::Audio, mid, None)
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};

    /// Stream wrapper that implements leading-edge debounce (test helper only).
    struct LeadingEdgeDebounce<S> {
        inner: S,
        debounce_duration: Duration,
        last_emitted: Option<Instant>,
    }

    impl<S> LeadingEdgeDebounce<S> {
        fn new(inner: S, debounce_duration: Duration) -> Self {
            Self {
                inner,
                debounce_duration,
                last_emitted: None,
            }
        }
    }

    impl<S, T> Stream for LeadingEdgeDebounce<S>
    where
        S: Stream<Item = T> + Unpin,
    {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            loop {
                match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Ready(Some(item)) => {
                        let now = Instant::now();
                        match self.last_emitted {
                            Some(last) if now.duration_since(last) < self.debounce_duration => {
                                continue;
                            }
                            _ => {
                                self.last_emitted = Some(now);
                                return Poll::Ready(Some(item));
                            }
                        }
                    }
                    other => return other,
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_leading_edge_debounce() {
        // 1. Setup: Debounce for 100ms
        let (tx, rx) = mpsc::channel(10);
        let stream = ReceiverStream::new(rx);
        let mut debounced_stream = LeadingEdgeDebounce::new(stream, Duration::from_millis(100));

        // 2. Event A (Time 0ms): Should pass through immediately (Leading Edge)
        tx.send("A").await.unwrap();

        let item = debounced_stream.next().await;
        assert_eq!(item, Some("A"), "First item should be emitted immediately");

        // 3. Event B (Time 1ms): Should be dropped (inside 100ms window)
        tokio::time::advance(Duration::from_millis(1)).await;
        tx.send("B").await.unwrap();

        // We poll the stream to ensure it processes "B" and returns Pending
        // We use tokio::time::timeout to ensure we don't hang if logic is wrong
        let result = tokio::time::timeout(Duration::from_millis(1), debounced_stream.next()).await;
        assert!(
            result.is_err(),
            "Item B should be swallowed/debounced, so stream should be Pending"
        );

        // 4. Event C (Time 50ms): Still inside window (total 51ms), should be dropped
        tokio::time::advance(Duration::from_millis(49)).await;
        tx.send("C").await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(1), debounced_stream.next()).await;
        assert!(
            result.is_err(),
            "Item C should be swallowed (only 50ms passed)"
        );

        // 5. Event D (Time 101ms): Window passed, should emit
        // Advance enough to pass the 100ms threshold from the LAST EMISSION (Time 0)
        tokio::time::advance(Duration::from_millis(51)).await; // Total time since A = 102ms
        tx.send("D").await.unwrap();

        let item = debounced_stream.next().await;
        assert_eq!(
            item,
            Some("D"),
            "Item D should be emitted as 100ms has passed since A"
        );

        // 6. Event E (Time 103ms): Should be dropped (new window started at D)
        tokio::time::advance(Duration::from_millis(1)).await;
        tx.send("E").await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(1), debounced_stream.next()).await;
        assert!(
            result.is_err(),
            "Item E should be swallowed (too close to D)"
        );
    }
}
