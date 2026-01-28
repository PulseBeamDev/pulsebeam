use std::fmt::Display;
use std::pin::Pin;
use std::{
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};

use pulsebeam_runtime::sync::spmc;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};

pub type KeyframeRequestStream = Pin<Box<dyn Stream<Item = KeyframeRequest> + Send>>;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SimulcastQuality {
    Undefined = 0,
    Low = 1,
    Medium = 2,
    High = 3,
}

impl std::fmt::Debug for SimulcastQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Self::Undefined => "undefined",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        };
        f.write_str(txt)
    }
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: crate::entity::TrackId,
    pub origin_participant: crate::entity::ParticipantId,
    pub kind: MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub meta: Arc<TrackMeta>,
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub channel: spmc::Receiver<RtpPacket>,
    pub keyframe_requester: mpsc::Sender<KeyframeRequestKind>,
    pub state: StreamState,
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
    pub fn request_keyframe(&self, kind: KeyframeRequestKind) {
        if let Err(err) = self.keyframe_requester.try_send(kind) {
            tracing::warn!("failed to request keyframe: {err:?}");
        }
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
    keyframe_requests: Option<mpsc::Receiver<KeyframeRequestKind>>,
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

    pub fn forward(&mut self, pkt: RtpPacket) {
        // RTP Pipeline
        let pkt = self.synchronizer.process(pkt);
        self.monitor
            .process_packet(&pkt, pkt.payload.len() + pkt.raw_header.header_len);
        if (self.filter)(&pkt) {
            self.channel.send(pkt);
        }
    }

    pub fn keyframe_request_stream(&mut self, debounce: Duration) -> KeyframeRequestStream {
        let stream = self.keyframe_requests.take().unwrap();
        let rid = self.rid;
        let mid = self.mid;
        let watch_stream = ReceiverStream::new(stream);

        let debounced = LeadingEdgeDebounce::new(watch_stream, debounce)
            .map(move |kind| KeyframeRequest { kind, rid, mid });

        Box::pin(debounced)
    }
}

#[derive(PartialEq, Eq)]
pub struct TrackSender {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastSender>,
}

impl TrackSender {
    pub fn forward(&mut self, rid: Option<&Rid>, packet: RtpPacket) {
        let sender = self
            .simulcast
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always be available");
        sender.forward(packet);
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
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    pub fn by_rid(&self, rid: &Option<Rid>) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|s| s.rid == *rid)
    }

    pub fn by_quality(&self, quality: SimulcastQuality) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|s| s.quality == quality)
    }

    pub fn higher_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        let next_quality = match current {
            SimulcastQuality::Undefined => Some(SimulcastQuality::Low),
            SimulcastQuality::Low => Some(SimulcastQuality::Medium),
            SimulcastQuality::Medium => Some(SimulcastQuality::High),
            SimulcastQuality::High => None,
        };
        if let Some(next) = next_quality {
            self.simulcast.iter().find(|s| s.quality == next)
        } else {
            None
        }
    }

    pub fn lower_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        let prev_quality = match current {
            SimulcastQuality::High => Some(SimulcastQuality::Medium),
            SimulcastQuality::Medium => Some(SimulcastQuality::Low),
            SimulcastQuality::Low => Some(SimulcastQuality::Undefined),
            SimulcastQuality::Undefined => None,
        };

        if let Some(prev) = prev_quality {
            self.simulcast.iter().find(|s| s.quality == prev)
        } else {
            None
        }
    }

    pub fn lowest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .iter()
            .min_by_key(|s| s.quality)
            .expect("no lowest quality, there must be at least 1 layer for TrackReceiver to exist")
    }

    pub fn highest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .iter()
            .max_by_key(|s| s.quality)
            .expect("no highest quality, there must be at least 1 layer for TrackReceiver to exist")
    }
}

pub fn new(mid: Mid, meta: Arc<TrackMeta>, base_cap: usize) -> (TrackSender, TrackReceiver) {
    let mut simulcast_rids = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    simulcast_rids.sort_by_key(|rid| rid.unwrap_or_default().to_string());

    let mut senders = Vec::new();
    let mut receivers = Vec::new();

    for rid in simulcast_rids {
        // TODO: get this from SDP instead
        let (clock_rate, filter) = match meta.kind {
            MediaKind::Audio => (rtp::AUDIO_FREQUENCY, should_forward_audio as PacketFilter),
            MediaKind::Video => (rtp::VIDEO_FREQUENCY, should_forward_noop as PacketFilter),
        };
        let (quality, bitrate, cap_tier) = match (meta.kind, rid) {
            (MediaKind::Audio, _) => (SimulcastQuality::Undefined, 64_000, 1),
            (MediaKind::Video, None) => (SimulcastQuality::Undefined, 500_000, 4),
            (MediaKind::Video, Some(r)) if r.starts_with('f') => {
                (SimulcastQuality::High, 800_000, 4)
            }
            (MediaKind::Video, Some(r)) if r.starts_with('h') => {
                (SimulcastQuality::Medium, 300_000, 2)
            }
            (MediaKind::Video, Some(r)) if r.starts_with('q') => {
                (SimulcastQuality::Low, 150_000, 1)
            }
            (MediaKind::Video, Some(rid)) => {
                tracing::warn!("use default bitrate due to unsupported rid: {rid}");
                (SimulcastQuality::Undefined, 500_000, 2)
            }
        };

        let (tx, rx) = spmc::channel(base_cap * cap_tier);
        let (keyframe_tx, keyframe_rx) = mpsc::channel(1);

        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.kind, stream_id, stream_state.clone());

        senders.push(SimulcastSender {
            mid,
            rid,
            quality,
            filter,
            synchronizer: Synchronizer::new(clock_rate),
            channel: tx,
            keyframe_requests: Some(keyframe_rx),
            monitor,
        });
        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            quality,
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
            state: stream_state,
        });
    }
    senders.sort_by_key(|e| std::cmp::Reverse(e.quality));
    receivers.sort_by_key(|e| std::cmp::Reverse(e.quality));

    (
        TrackSender {
            meta: meta.clone(),
            simulcast: senders,
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

    let ext = &packet.raw_header.ext_vals;

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

/// A stream wrapper for leading-edge debounce
pub struct LeadingEdgeDebounce<S> {
    inner: S,
    debounce_duration: Duration,
    last_emitted: Option<Instant>,
}

impl<S> LeadingEdgeDebounce<S> {
    pub fn new(inner: S, debounce_duration: Duration) -> Self {
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
                            // Ignore item, still in debounce period
                            continue;
                        }
                        _ => {
                            // Leading edge: emit immediately
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
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

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
